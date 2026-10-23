//! `observer curate` — human curation of the knowledge graph THROUGH the daemon.
//!
//! A host (Enclade's knowledge window) pipes ONE JSON request on stdin:
//! `{"project_dir":"<abs project root>","op":"curate_…","args":{…}}`.
//! We resolve the knowledge dir like every other subcommand (`Config`:
//! `<project>/.claude/observer.json` > env `OBSERVER_KNOWLEDGE_DIR` > default),
//! make sure the per-project daemon runs (`ensure_daemon`), forward the op over IPC
//! and print the daemon's `IpcResponse` JSON on stdout.
//!
//! The daemon stays the single writer: if it cannot be reached we return an error
//! and NEVER touch the store ourselves. Exit code: 0 = ok, 1 = op/daemon error,
//! 2 = malformed request.

use std::path::PathBuf;

use serde_json::Value;

use crate::config::Config;
use crate::ipc::IpcResponse;

/// The only ops `curate` forwards (write-side curation; reads go elsewhere).
pub const CURATE_OPS: &[&str] = &[
    "curate_update_node",
    "curate_delete_node",
    "curate_add_edge",
    "curate_delete_edge",
    "curate_scan_documents",
];

#[derive(Debug, PartialEq)]
pub struct CurateRequest {
    pub project_dir: PathBuf,
    pub op: String,
    pub args: Value,
}

/// Pure validation of the stdin request (unit-testable).
pub fn parse_curate_request(v: &Value) -> Result<CurateRequest, String> {
    if !v.is_object() {
        return Err("curate: expected one JSON object on stdin".into());
    }
    let project_dir = v
        .get("project_dir")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("curate: missing 'project_dir'")?;
    let op = v.get("op").and_then(Value::as_str).unwrap_or("");
    if !CURATE_OPS.contains(&op) {
        return Err(format!("curate: unsupported op '{op}' (allowed: {})", CURATE_OPS.join(", ")));
    }
    let args = match v.get("args") {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(a) if a.is_object() => a.clone(),
        Some(_) => return Err("curate: 'args' must be an object".into()),
    };
    Ok(CurateRequest { project_dir: PathBuf::from(project_dir), op: op.to_string(), args })
}

/// Forward a parsed request to the project's daemon (autostarting it).
fn forward(req: &CurateRequest) -> IpcResponse {
    let kdir = Config::resolve(&req.project_dir).knowledge_dir_abs(&req.project_dir);
    let Some((port, token)) = crate::observer::daemon::ensure_daemon(&req.project_dir, &kdir) else {
        return IpcResponse::err(format!(
            "daemon unavailable: could not start/reach the knowledge daemon for {}",
            kdir.display()
        ));
    };
    let project = req.project_dir.to_string_lossy();
    crate::ipc::call(port, &token, &project, &req.op, req.args.clone(), "")
        .unwrap_or_else(|e| IpcResponse::err(format!("daemon unavailable: {e}")))
}

/// Entry point: stdin → daemon → stdout, exit code per the module doc.
pub fn run_curate() {
    let raw = crate::cli::read_stdin_json(std::time::Duration::from_secs(5));
    let (resp, code) = match parse_curate_request(&raw) {
        Ok(req) => {
            let r = forward(&req);
            let code = if r.ok { 0 } else { 1 };
            (r, code)
        }
        Err(e) => (IpcResponse::err(e), 2),
    };
    println!(
        "{}",
        serde_json::to_string(&resp)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"failed to serialize response"}"#.into())
    );
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_valid_request() {
        let r = parse_curate_request(&json!({
            "project_dir": "C:/proj", "op": "curate_delete_edge", "args": { "edge_id": "e1" }
        }))
        .unwrap();
        assert_eq!(r.project_dir, PathBuf::from("C:/proj"));
        assert_eq!(r.op, "curate_delete_edge");
        assert_eq!(r.args, json!({ "edge_id": "e1" }));
    }

    #[test]
    fn missing_args_become_an_empty_object() {
        let r = parse_curate_request(&json!({ "project_dir": "/p", "op": "curate_scan_documents" }))
            .unwrap();
        assert_eq!(r.args, json!({}));
    }

    #[test]
    fn rejects_malformed_requests() {
        for bad in [
            Value::Null,
            json!("x"),
            json!({ "op": "curate_delete_node" }),
            json!({ "project_dir": "  ", "op": "curate_delete_node" }),
            json!({ "project_dir": "/p" }),
            json!({ "project_dir": "/p", "op": "record" }),
            json!({ "project_dir": "/p", "op": "curate_delete_node", "args": [1] }),
        ] {
            assert!(parse_curate_request(&bad).is_err(), "should reject {bad}");
        }
    }
}
