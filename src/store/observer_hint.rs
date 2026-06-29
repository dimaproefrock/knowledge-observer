//! Pending observer hints + dedupe/last-informed bookkeeping per session.
//!
//! The observer drops a pending hint here; the `knowledge-hint`
//! UserPromptSubmit hook consumes it (read+clear) at the next turn. Stored under
//! `<knowledge_dir>/observer-hints/<session_id>.md`. Sibling per-session state
//! (`<session_id>.state.json`) tracks which nodes this session was already told
//! about + the last-informed high-water mark, so hints don't repeat.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Per-session hint bookkeeping (dedupe + cooldown), persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct HintState {
    /// Node ids this session has already been pointed at (dedupe per node).
    pub informed_node_ids: Vec<String>,
    /// High-water mark (node `datum`) up to which deltas were already reported.
    pub last_informed_datum: String,
}

impl HintState {
    /// Record that this session was pointed at `node_id` and advance the
    /// last-informed high-water mark. The id is added only if absent (dedupe);
    /// `last_informed_datum` is bumped to the lexicographic max of the current
    /// value and `datum` (ISO-8601 `datum`s order lexicographically).
    pub fn mark_informed(&mut self, node_id: &str, datum: &str) {
        if !self.informed_node_ids.iter().any(|id| id == node_id) {
            self.informed_node_ids.push(node_id.to_string());
        }
        if datum > self.last_informed_datum.as_str() {
            self.last_informed_datum = datum.to_string();
        }
    }
}

fn hints_dir(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("observer-hints")
}

fn hint_path(knowledge_dir: &Path, session_id: &str) -> PathBuf {
    hints_dir(knowledge_dir).join(format!("{session_id}.md"))
}

fn state_path(knowledge_dir: &Path, session_id: &str) -> PathBuf {
    hints_dir(knowledge_dir).join(format!("{session_id}.state.json"))
}

/// Append a pending hint line for a session (creates the file/dir if missing).
/// No-op for an empty/whitespace-only `hint`.
pub fn append(knowledge_dir: &Path, session_id: &str, hint: &str) -> Result<()> {
    if hint.trim().is_empty() {
        return Ok(());
    }
    super::ensure_dir(&hints_dir(knowledge_dir))?;
    let path = hint_path(knowledge_dir, session_id);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{hint}")?;
    Ok(())
}

/// Read AND clear the pending hint for a session (consumed by the hook). Empty if none.
pub fn take(knowledge_dir: &Path, session_id: &str) -> String {
    let path = hint_path(knowledge_dir, session_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return String::new(),
    };
    let _ = std::fs::remove_file(&path);
    content.trim().to_string()
}

/// Load the per-session hint bookkeeping (missing/corrupt → default).
pub fn load_state(knowledge_dir: &Path, session_id: &str) -> HintState {
    let path = state_path(knowledge_dir, session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// Persist the per-session hint bookkeeping as pretty JSON, creating the
/// `observer-hints/` directory if necessary.
pub fn save_state(knowledge_dir: &Path, session_id: &str, state: &HintState) -> Result<()> {
    super::ensure_dir(&hints_dir(knowledge_dir))?;
    let path = state_path(knowledge_dir, session_id);
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("obs-test-hint-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_append_then_take_returns_both_lines_in_order() {
        let dir = temp_dir();

        append(&dir, "session-1", "Index aktualisiert: Foo").unwrap();
        append(&dir, "session-1", "Wissen zu Bar existiert").unwrap();

        let taken = take(&dir, "session-1");
        assert_eq!(taken, "Index aktualisiert: Foo\nWissen zu Bar existiert");

        // Cleared after take.
        assert_eq!(take(&dir, "session-1"), "");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_take_absent_file_is_empty() {
        let dir = temp_dir();
        assert_eq!(take(&dir, "nope"), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_append_empty_is_noop() {
        let dir = temp_dir();
        append(&dir, "s", "   ").unwrap();
        append(&dir, "s", "").unwrap();
        assert_eq!(take(&dir, "s"), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_state_roundtrip() {
        let dir = temp_dir();
        let mut state = HintState::default();
        state.mark_informed("node-a", "2026-06-17T10:00:00Z");

        save_state(&dir, "session-1", &state).unwrap();
        let loaded = load_state(&dir, "session-1");

        assert_eq!(loaded, state);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_state_missing_returns_default() {
        let dir = temp_dir();
        assert_eq!(load_state(&dir, "missing"), HintState::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_state_corrupt_returns_default() {
        let dir = temp_dir();
        super::super::ensure_dir(&hints_dir(&dir)).unwrap();
        std::fs::write(state_path(&dir, "garbage"), b"{ not valid json ]]]").unwrap();

        assert_eq!(load_state(&dir, "garbage"), HintState::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_mark_informed_dedups_and_tracks_max_datum() {
        let mut state = HintState::default();
        state.mark_informed("node-a", "2026-06-10T00:00:00Z");
        state.mark_informed("node-a", "2026-06-05T00:00:00Z"); // dup id, older datum
        state.mark_informed("node-b", "2026-06-17T00:00:00Z");

        assert_eq!(state.informed_node_ids, vec!["node-a", "node-b"]);
        // Max datum wins, the older second call must not regress it.
        assert_eq!(state.last_informed_datum, "2026-06-17T00:00:00Z");
    }
}
