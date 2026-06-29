//! The plugin's own **read-only** MCP server, exposed as the `observer mcp-serve`
//! subcommand. It is the third read-side pillar of the Knowledge-Observer plugin,
//! alongside the SessionStart index-push and the UserPromptSubmit hint
//! (`crate::cli`). Where those two *push* knowledge at the agent, this server lets
//! the work agent **actively pull** it within a session via real MCP tool calls.
//!
//! ## In-process reads
//! This server calls [`crate::dispatch::dispatch_store`] **directly in-process**.
//! Reads are safe under concurrent access (the store re-reads files each call), so
//! no daemon/IPC is needed for the read path.
//!
//! ## Read-only by construction
//! Only the READ tools are exposed: `query_knowledge`, `get_knowledge`,
//! `current_knowledge`, `list_documents`, and `knowledge_ping` (health). The WRITE
//! tools (record_knowledge / add_fact / update_knowledge / link / merge_knowledge)
//! are **never** advertised or callable here — the write side is transparent via
//! the background observer.
//!
//! ## Transport
//! MCP stdio transport = newline-delimited JSON-RPC 2.0.
//! One JSON object per line on stdin; one response line per request on stdout.
//! stdout is the protocol channel — **never** print anything but JSON-RPC there
//! (logs go to stderr).

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::ipc::IpcRequest;

const SERVER_NAME: &str = "knowledge-observer";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Fallback when the client doesn't send a `protocolVersion` in `initialize`.
/// We otherwise echo whatever the client requested for max compatibility.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// Run the stdio MCP server loop. Blocks reading stdin until EOF, then returns.
/// Synchronous on purpose — this subprocess has no need for an async runtime.
pub fn run_mcp_serve() {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut line = String::new();

    eprintln!("[observer-mcp] {SERVER_NAME} v{SERVER_VERSION} serving on stdio (read-only)");

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — Claude closed the pipe; shut down cleanly.
            Ok(_) => {}
            Err(e) => {
                eprintln!("[observer-mcp] stdin read error: {e}");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_message(trimmed) {
            if writeln!(writer, "{response}").is_err() {
                break;
            }
            let _ = writer.flush();
        }
    }
}

/// Resolve the project dir for a tool call: `CLAUDE_PROJECT_DIR` (native plugin
/// env) → `OBSERVER_PROJECT_DIR` → the process cwd. Mirrors the hook resolution in
/// [`crate::cli`] (minus the hook-stdin `cwd`, which MCP has no equivalent for).
fn resolve_project_dir() -> String {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("OBSERVER_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()))
        .unwrap_or_else(|| ".".to_string())
}

/// Dispatch a single JSON-RPC message line. Returns the response line, or `None`
/// for notifications (messages without an `id`), which get no reply. Kept
/// straightforward to unit-test: `tools/call` routes through `dispatch_store`,
/// which resolves config/knowledge-dir from the request's `project_dir`.
fn handle_message(line: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(Value::Null, -32700, &format!("parse error: {e}")));
        }
    };

    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    // No `id` field → notification (e.g. `notifications/initialized`) → no response.
    let id = match msg.get("id") {
        Some(id) => id.clone(),
        None => return None,
    };

    let response = match method {
        "initialize" => handle_initialize(id, msg.get("params")),
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(id, tools_list()),
        "tools/call" => handle_tools_call(id, msg.get("params")),
        other => error_response(id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

fn handle_initialize(id: Value, params: Option<&Value>) -> String {
    // Echo the client's requested protocol version when present.
    let protocol_version = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION)
        .to_string();

    result_response(
        id,
        json!({
            "protocolVersion": protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        }),
    )
}

/// One tool entry, marked to load **eagerly** (`anthropic/alwaysLoad`) rather than
/// deferred behind tool-search — so the model can call it without a ToolSearch
/// step. Without this, Claude treats the tools as deferred and won't activate them
/// proactively.
fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "_meta": { "anthropic/alwaysLoad": true }
    })
}

/// The advertised tool set: a health check plus the READ knowledge tools only.
/// The write tools are deliberately ABSENT — the background observer is the sole
/// writer of the DAG.
fn tools_list() -> Value {
    json!({ "tools": all_tools() })
}

/// Every advertised tool definition (health check + the four read tools). No write
/// tools exist here at all.
fn all_tools() -> Vec<Value> {
    let no_args = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    vec![
        tool(
            "knowledge_ping",
            "Health check for the knowledge graph service. Returns a pong. \
Use to confirm the knowledge tools are reachable.",
            no_args.clone(),
        ),
        tool(
            "query_knowledge",
            "COMPACT overview / search of the project's knowledge graph: each node as id + typ \
+ status + score + short title + tags (NO begruendung, truncated inhalt) plus compact edges and \
questions — cheap even for large graphs. Use it to see WHAT EXISTS and to pick ids. Consult it \
BEFORE answering, deciding or recording. Narrow with 'q' (keyword), 'typ', 'status', 'tags' (any \
of), 'limit' (top-N by score); matches include direct neighbours. Then fetch FULL detail \
(begruendung/quellen/full inhalt + sub-DAG) for chosen ids via get_knowledge.",
            json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string", "description": "Keyword matched in node content (case-insensitive)." },
                    "typ": { "type": "string", "enum": ["entscheidung","erkenntnis","fakt","beobachtung","recherche","vermutung"] },
                    "status": { "type": "string", "enum": ["gestützt","umstritten","widerlegt","unbelegt"] },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Match nodes carrying ANY of these topic/area tags." },
                    "limit": { "type": "integer", "description": "Keep only the top-N matches by score." }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "get_knowledge",
            "Deep fetch by id: return FULL nodes (with begruendung, quellen, full inhalt, \
session origin) for the given ids PLUS their neighbourhood up to 'depth' hops (supporting \
evidence / the decisions they support). This is how you pull detail after spotting relevant ids \
in query_knowledge.",
            json!({
                "type": "object",
                "properties": {
                    "ids": { "type": "array", "items": { "type": "string" }, "description": "Node ids (from query_knowledge) to fetch in full." },
                    "depth": { "type": "integer", "description": "Neighbourhood hops to include (default 1)." }
                },
                "required": ["ids"],
                "additionalProperties": false
            }),
        ),
        tool(
            "current_knowledge",
            "\"What holds right now\": the project's ACTIVE (non-superseded) decisions plus the \
open questions with their currently leading value. Use this on re-entry / at the start of a work \
session to get the current state without reading the whole history.",
            no_args.clone(),
        ),
        tool(
            "list_documents",
            "List the project's indexed documents (specs, ADRs, feature docs, notes — \
auto-scanned from project subfolders) that you can cite as sources. Returns each document's id \
(= path), titel and art. Use an id in the 'quellen' parameter of record_knowledge / add_fact to \
ground a node in a real document.",
            no_args.clone(),
        ),
    ]
}

fn handle_tools_call(id: Value, params: Option<&Value>) -> String {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Map MCP tool name → store op. Only read ops are exposed; nothing else can
    // mutate the graph from here.
    let op = match name {
        "query_knowledge" => "query",
        "get_knowledge" => "get",
        "current_knowledge" => "current_state",
        "list_documents" => "list_documents",
        "knowledge_ping" => "ping",
        other => {
            // Unknown tool name is a *tool* error (isError), not a JSON-RPC
            // protocol error — the call itself was well-formed.
            return result_response(
                id,
                json!({
                    "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
                    "isError": true
                }),
            );
        }
    };

    dispatch_op(id, op, args)
}

/// Build an `IpcRequest` for the given op + args and execute it directly in-process
/// via [`crate::dispatch::dispatch_store`] (no IPC, no daemon — reads are safe
/// concurrent). Shape the store response as an MCP tool result: the JSON payload as
/// pretty text content; a store failure maps to `isError: true` (a tool error, not
/// a protocol error). The project dir comes from the environment; `session_id` is
/// irrelevant for reads (provenance only applies to writes) so it stays empty.
fn dispatch_op(id: Value, op: &str, args: Value) -> String {
    let req = IpcRequest {
        token: String::new(),
        project_dir: resolve_project_dir(),
        op: op.to_string(),
        payload: args,
        session_id: String::new(),
    };
    let (resp, _changed) = crate::dispatch::dispatch_store(&req);
    if resp.ok {
        let text = serde_json::to_string_pretty(&resp.data).unwrap_or_default();
        result_response(
            id,
            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        )
    } else {
        let msg = resp.error.unwrap_or_else(|| "knowledge op failed".to_string());
        result_response(
            id,
            json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
        )
    }
}

fn result_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    /// A fresh project dir, wired into the env so `dispatch_op` resolves to it.
    /// Returns the dir; the caller removes it. Serialized via the env var, so the
    /// store-backed tests below set + read it within one call (no cross-test race
    /// because each uses a unique uuid dir and we only assert on that dir's data).
    fn fresh_project(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("observer-mcp-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn initialize_echoes_protocol_version_and_server_info() {
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude","version":"x"}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-03-26"));
        assert_eq!(resp["result"]["serverInfo"]["name"], json!(SERVER_NAME));
        // Must advertise the tools capability.
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_default_protocol_version() {
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert_eq!(resp["result"]["protocolVersion"], json!(DEFAULT_PROTOCOL_VERSION));
    }

    #[test]
    fn notification_without_id_gets_no_response() {
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle_message(note).is_none());
    }

    /// The advertised tool set is EXACTLY the read tools + health check — and
    /// crucially contains NONE of the write tools (the write side is transparent).
    #[test]
    fn tools_list_is_read_only_exactly() {
        let tools = all_tools();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        // All read tools + health check present.
        for expected in [
            "knowledge_ping",
            "query_knowledge",
            "get_knowledge",
            "current_knowledge",
            "list_documents",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}; got {names:?}");
        }
        // No write tool leaked in.
        for write in [
            "record_knowledge",
            "add_fact",
            "update_knowledge",
            "link",
            "merge_knowledge",
        ] {
            assert!(!names.contains(&write), "write tool {write} must not be exposed; got {names:?}");
        }
        // Exactly five tools, all with an input schema.
        assert_eq!(names.len(), 5, "expected exactly 5 read tools; got {names:?}");
        assert!(tools.iter().all(|t| t["inputSchema"].is_object()));
        // Loaded eagerly so the model can call them without a ToolSearch step.
        assert!(tools.iter().all(|t| t["_meta"]["anthropic/alwaysLoad"] == json!(true)));
    }

    /// Calling a write tool by name is rejected as an unknown tool (it is not in
    /// the routing table at all) — defense confirming no write path exists.
    #[test]
    fn write_tool_call_is_unknown_tool() {
        let req = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"record_knowledge","arguments":{}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert!(resp.get("error").is_none(), "should be a tool error, not protocol error");
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("unknown tool"), "got: {text}");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let req = r#"{"jsonrpc":"2.0","id":6,"method":"does/not/exist"}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert_eq!(resp["id"], json!(6));
    }

    #[test]
    fn malformed_json_is_parse_error_with_null_id() {
        let resp = parse(&handle_message("{not valid json").unwrap());
        assert_eq!(resp["error"]["code"], json!(-32700));
        assert_eq!(resp["id"], Value::Null);
    }

    #[test]
    fn string_ids_are_preserved() {
        let req = r#"{"jsonrpc":"2.0","id":"abc-1","method":"ping"}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert_eq!(resp["id"], json!("abc-1"));
        assert!(resp["result"].is_object());
    }

    #[test]
    fn unknown_tool_is_tool_error_not_protocol_error() {
        let req = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"]["isError"], json!(true));
    }

    // ---- store-backed tool/call (direct in-process dispatch, no GUI/IPC) ----

    #[test]
    fn tools_call_ping_routes_through_dispatch() {
        let proj = fresh_project("ping");
        std::env::set_var("CLAUDE_PROJECT_DIR", &proj);
        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"knowledge_ping","arguments":{}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        std::env::remove_var("CLAUDE_PROJECT_DIR");

        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // The store's ping payload identifies the observer host + echoes project.
        assert!(text.contains("knowledge-observer"), "got: {text}");
        assert!(text.contains("\"pong\""), "got: {text}");
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn tools_call_current_knowledge_against_tempdir_store() {
        let proj = fresh_project("current");
        // Seed an active decision through the same store dispatch_store reads from.
        let kdir = crate::config::Config::resolve(&proj).knowledge_dir_abs(&proj);
        crate::store::knowledge_store::add_node(
            &kdir,
            crate::store::knowledge_store::NodeType::Entscheidung,
            "Wir nutzen Tauri 2".into(),
            "damit wir eine Desktop-App haben",
            None,
            "session",
        )
        .unwrap();

        std::env::set_var("CLAUDE_PROJECT_DIR", &proj);
        let req = r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"current_knowledge","arguments":{}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        std::env::remove_var("CLAUDE_PROJECT_DIR");

        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).expect("tool result is JSON");
        let decisions = payload["aktive_entscheidungen"].as_array().unwrap();
        assert_eq!(decisions.len(), 1, "expected one active decision: {text}");
        assert_eq!(decisions[0]["inhalt"], json!("Wir nutzen Tauri 2"));
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn tools_call_query_knowledge_against_tempdir_store() {
        let proj = fresh_project("query");
        let kdir = crate::config::Config::resolve(&proj).knowledge_dir_abs(&proj);
        crate::store::knowledge_store::add_node(
            &kdir,
            crate::store::knowledge_store::NodeType::Beobachtung,
            "Ein beobachtetes Detail".into(),
            "welche Frage beantwortet das?",
            None,
            "session",
        )
        .unwrap();

        std::env::set_var("CLAUDE_PROJECT_DIR", &proj);
        let req = r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"query_knowledge","arguments":{}}}"#;
        let resp = parse(&handle_message(req).unwrap());
        std::env::remove_var("CLAUDE_PROJECT_DIR");

        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).expect("tool result is JSON");
        let nodes = payload["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1, "expected one node in compact overview: {text}");
        assert_eq!(nodes[0]["titel"], json!("Ein beobachtetes Detail"));
        let _ = std::fs::remove_dir_all(&proj);
    }
}
