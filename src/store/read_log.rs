//! Append-only log of the working agent's knowledge READS (`<knowledge_dir>/knowledge-reads.log`).
//!
//! ## Why this exists
//! Whether the knowledge is actually used was only reconstructable from transcripts
//! (2026-10-20 evaluation: 4 active reads in 12 sessions). One JSON line per MCP read
//! makes "how often, which tool, which nodes" countable directly from the store.
//!
//! ## Guarantees
//! Best-effort like `hook_trace`: IO errors are swallowed and the file is only written
//! when the knowledge dir already exists. Lines are small single appends, so concurrent
//! MCP server processes (one per session) do not corrupt each other. `pid` identifies
//! the MCP server process, i.e. roughly one working session.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

const FILE: &str = "knowledge-reads.log";

/// Append one read record. `args` are the tool arguments as sent by the agent;
/// `returned` is the number of nodes in the response (`None` if not node-shaped).
pub fn record(knowledge_dir: &Path, tool: &str, args: &Value, returned: Option<usize>) {
    if !knowledge_dir.is_dir() {
        return;
    }
    let line = json!({
        "at": crate::store::now(),
        "pid": std::process::id(),
        "tool": tool,
        "args": args,
        "returned": returned,
    })
    .to_string()
        + "\n";
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(knowledge_dir.join(FILE))
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_read_and_skips_missing_dir() {
        let dir = std::env::temp_dir().join(format!("read-log-{}", uuid::Uuid::new_v4()));
        record(&dir, "query_knowledge", &json!({}), Some(1));
        assert!(!dir.exists(), "must not create the store just to log");

        std::fs::create_dir_all(&dir).unwrap();
        record(&dir, "get_knowledge", &json!({ "ids": ["a"] }), Some(3));
        record(&dir, "current_knowledge", &json!({}), None);
        let body = std::fs::read_to_string(dir.join(FILE)).unwrap();
        let lines: Vec<Value> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], "get_knowledge");
        assert_eq!(lines[0]["args"]["ids"][0], "a");
        assert_eq!(lines[0]["returned"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
