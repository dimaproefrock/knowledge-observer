//! Loopback IPC between the knowledge MCP server (a separate process Claude spawns)
//! and the running host.
//!
//! Architecture: the MCP server is a **thin client** — it forwards every knowledge
//! tool call over this channel to the host, which is the **single writer** of the
//! knowledge files. That eliminates cross-process write races (multiple sessions →
//! multiple MCP processes) and lets the host update its view instantly via its
//! normal event path.
//!
//! Transport: plain TCP on `127.0.0.1` (ephemeral port), one newline-delimited
//! JSON request per connection, one JSON response back. A random per-launch
//! token authenticates callers so no other local process can reach the graph.
//! Dependency-free (std::net + serde_json) and runs on a dedicated std thread.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request from the MCP server (thin client) to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Shared secret minted at host launch; must match or the request is rejected.
    pub token: String,
    /// Which project's knowledge graph this call targets (the MCP server gets
    /// this from its `OBSERVER_PROJECT_DIR` env). Empty if unset.
    pub project_dir: String,
    /// Operation name — for now the MCP tool name (e.g. `ping`, later
    /// `record_knowledge` / `query_knowledge`).
    pub op: String,
    /// Operation arguments (the MCP tool's `arguments`). `Null` if none.
    #[serde(default)]
    pub payload: Value,
    /// Origin session (the MCP server reads its `OBSERVER_SESSION_ID`).
    /// Provenance only — stamped onto recorded nodes. Empty if unset.
    #[serde(default)]
    pub session_id: String,
}

/// The host's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(data: Value) -> Self {
        Self { ok: true, data, error: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, data: Value::Null, error: Some(msg.into()) }
    }
}

/// Read timeout for a single connection on the server side — a wedged client
/// must never block the accept loop's worker thread forever.
const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Client-side read timeout waiting for the host's reply.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Bind a fresh loopback listener and mint a token. Returns the listener (to be
/// handed to [`serve`]), the chosen port, and the token. Call once at host launch.
pub fn bind() -> std::io::Result<(TcpListener, u16, String)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().to_string();
    Ok((listener, port, token))
}

/// Run the accept loop forever (call on a dedicated thread). Each connection is
/// handled on its own short-lived worker thread so one slow client can't stall
/// the others. `handler` dispatches an authenticated request to a response.
pub fn serve<F>(listener: TcpListener, token: String, handler: F)
where
    F: Fn(IpcRequest) -> IpcResponse + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let token = Arc::new(token);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let h = Arc::clone(&handler);
                let t = Arc::clone(&token);
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &t, h.as_ref()) {
                        eprintln!("[knowledge-ipc] connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[knowledge-ipc] accept error: {e}"),
        }
    }
}

fn handle_conn<F>(stream: TcpStream, expected_token: &str, handler: &F) -> std::io::Result<()>
where
    F: Fn(IpcRequest) -> IpcResponse,
{
    stream.set_read_timeout(Some(SERVER_READ_TIMEOUT))?;

    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut line)?;
    }

    let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
        Ok(req) if req.token == expected_token => handler(req),
        Ok(_) => IpcResponse::err("unauthorized"),
        Err(e) => IpcResponse::err(format!("bad request: {e}")),
    };

    let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| {
        r#"{"ok":false,"error":"failed to serialize response"}"#.to_string()
    });
    out.push('\n');
    (&stream).write_all(out.as_bytes())?;
    (&stream).flush()?;
    Ok(())
}

/// Client side (used by `mcp-serve`): open a connection, send one request, read
/// one response. Connection-per-call keeps the protocol trivial and stateless.
pub fn call(
    port: u16,
    token: &str,
    project_dir: &str,
    op: &str,
    payload: Value,
    session_id: &str,
) -> Result<IpcResponse, String> {
    let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(CLIENT_READ_TIMEOUT))
        .map_err(|e| format!("set timeout: {e}"))?;

    let req = IpcRequest {
        token: token.to_string(),
        project_dir: project_dir.to_string(),
        op: op.to_string(),
        payload,
        session_id: session_id.to_string(),
    };
    let mut line = serde_json::to_string(&req).map_err(|e| format!("encode: {e}"))?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    (&stream).flush().map_err(|e| format!("flush: {e}"))?;

    let mut resp_line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader
            .read_line(&mut resp_line)
            .map_err(|e| format!("read: {e}"))?;
    }
    serde_json::from_str(resp_line.trim()).map_err(|e| format!("decode response: {e}"))
}

/// Convenience for the MCP server: read `OBSERVER_IPC_PORT` / `OBSERVER_IPC_TOKEN`
/// / `OBSERVER_PROJECT_DIR` from the environment (set by the host in the generated
/// `--mcp-config`) and forward the call to the host.
pub fn call_from_env(op: &str, payload: Value) -> Result<IpcResponse, String> {
    let port: u16 = std::env::var("OBSERVER_IPC_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or("OBSERVER_IPC_PORT not set or invalid")?;
    let token = std::env::var("OBSERVER_IPC_TOKEN").map_err(|_| "OBSERVER_IPC_TOKEN not set")?;
    let project = std::env::var("OBSERVER_PROJECT_DIR").unwrap_or_default();
    let session = std::env::var("OBSERVER_SESSION_ID").unwrap_or_default();
    call(port, &token, &project, op, payload, &session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Start a server on a dedicated thread with the given handler; return its
    /// (port, token). The thread is detached and lives until the test process
    /// exits — fine for tests.
    fn start_test_server<F>(handler: F) -> (u16, String)
    where
        F: Fn(IpcRequest) -> IpcResponse + Send + Sync + 'static,
    {
        let (listener, port, token) = bind().expect("bind");
        let token_for_server = token.clone();
        std::thread::spawn(move || serve(listener, token_for_server, handler));
        (port, token)
    }

    #[test]
    fn roundtrip_echoes_through_handler() {
        let (port, token) = start_test_server(|req| {
            IpcResponse::ok(json!({ "echo_op": req.op, "project": req.project_dir, "sid": req.session_id }))
        });

        let resp = call(port, &token, "C:/proj", "ping", json!({"x": 1}), "sess-7").expect("call");
        assert!(resp.ok, "expected ok, got {resp:?}");
        assert_eq!(resp.data["echo_op"], json!("ping"));
        assert_eq!(resp.data["project"], json!("C:/proj"));
        assert_eq!(resp.data["sid"], json!("sess-7"));
    }

    #[test]
    fn wrong_token_is_rejected() {
        let (port, _token) = start_test_server(|_| IpcResponse::ok(json!("should not reach")));

        let resp = call(port, "WRONG-TOKEN", "", "ping", Value::Null, "").expect("call");
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("unauthorized"));
    }

    #[test]
    fn handler_error_is_propagated() {
        let (port, token) = start_test_server(|req| match req.op.as_str() {
            "known" => IpcResponse::ok(json!(true)),
            other => IpcResponse::err(format!("unknown op: {other}")),
        });

        let resp = call(port, &token, "", "bogus", Value::Null, "").expect("call");
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("unknown op: bogus"));
    }

    #[test]
    fn connection_refused_is_client_error() {
        // Nothing listening on this port (we bind then immediately drop it to
        // free the port, keeping only the number).
        let port = {
            let (listener, port, _t) = bind().unwrap();
            drop(listener);
            port
        };
        let result = call(port, "t", "", "ping", Value::Null, "");
        assert!(result.is_err(), "expected connect error, got {result:?}");
    }

    #[test]
    fn malformed_request_line_yields_bad_request() {
        // Drive the server with a raw non-JSON line to exercise the parse path.
        let (listener, port, token) = bind().unwrap();
        std::thread::spawn(move || serve(listener, token, |_| IpcResponse::ok(Value::Null)));

        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        (&stream).write_all(b"this is not json\n").unwrap();
        (&stream).flush().unwrap();
        let mut resp_line = String::new();
        BufReader::new(&stream).read_line(&mut resp_line).unwrap();
        let resp: IpcResponse = serde_json::from_str(resp_line.trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap_or_default().starts_with("bad request"));
    }
}
