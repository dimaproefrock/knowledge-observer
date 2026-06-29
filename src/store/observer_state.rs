//! Per-session observer state persistence.
//!
//! Stores `{ jsonl_stem, watermark, rolling_summary }` per session under
//! `<knowledge_dir>/observer-state/<session_id>.json` so a Resume / app restart
//! does not lose the tail position or the rolling summary.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Resume state for a session's background observer.
///
/// `serde(default)` makes partial/old JSON (and a missing file) tolerate-load
/// into sensible defaults — this is best-effort resume state, never a hard
/// dependency.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ObserverState {
    /// The transcript JSONL file stem this watermark refers to.
    pub jsonl_stem: String,
    /// Byte offset of the last processed complete line.
    pub watermark: u64,
    /// Compact running summary of the session arc.
    pub rolling_summary: String,
    /// The persistent observer-agent conversation id for this session. Empty until
    /// the first extraction pass mints one. `serde(default)` keeps older state
    /// files (without this field) loadable.
    #[serde(default)]
    pub observer_sid: String,
    /// How many turns the current observer conversation has consumed. Drives the
    /// re-seed threshold (`should_reseed`). `serde(default)` → old files start at 0.
    #[serde(default)]
    pub turn_count: u32,
}

fn state_dir(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("observer-state")
}

fn state_path(knowledge_dir: &Path, session_id: &str) -> PathBuf {
    state_dir(knowledge_dir).join(format!("{session_id}.json"))
}

/// Load the observer state for a session. Missing / unreadable / corrupt files
/// resolve to `ObserverState::default()` — reads never error.
pub fn load(knowledge_dir: &Path, session_id: &str) -> ObserverState {
    let path = state_path(knowledge_dir, session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// Persist the observer state for a session as pretty JSON, creating the
/// `observer-state/` directory if necessary.
pub fn save(knowledge_dir: &Path, session_id: &str, state: &ObserverState) -> Result<()> {
    super::ensure_dir(&state_dir(knowledge_dir))?;
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
            .join(format!("obs-test-observer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_roundtrip() {
        let dir = temp_dir();
        let state = ObserverState {
            jsonl_stem: "abc-123".to_string(),
            watermark: 4096,
            rolling_summary: "User refactored the pty manager.".to_string(),
            observer_sid: "obs-sid-42".to_string(),
            turn_count: 7,
        };

        save(&dir, "session-1", &state).unwrap();
        let loaded = load(&dir, "session-1");

        assert_eq!(loaded, state);
        assert_eq!(loaded.observer_sid, "obs-sid-42");
        assert_eq!(loaded.turn_count, 7);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_old_file_without_new_fields_defaults() {
        let dir = temp_dir();
        super::super::ensure_dir(&state_dir(&dir)).unwrap();
        // An older state file predating observer_sid/turn_count.
        std::fs::write(
            state_path(&dir, "old"),
            br#"{ "jsonl_stem": "s", "watermark": 12, "rolling_summary": "arc" }"#,
        )
        .unwrap();

        let loaded = load(&dir, "old");
        assert_eq!(loaded.jsonl_stem, "s");
        assert_eq!(loaded.watermark, 12);
        assert_eq!(loaded.rolling_summary, "arc");
        // New fields default cleanly.
        assert_eq!(loaded.observer_sid, "");
        assert_eq!(loaded.turn_count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_missing_file_returns_default() {
        let dir = temp_dir();
        let loaded = load(&dir, "does-not-exist");

        assert_eq!(loaded, ObserverState::default());
        assert_eq!(loaded.jsonl_stem, "");
        assert_eq!(loaded.watermark, 0);
        assert_eq!(loaded.rolling_summary, "");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_corrupt_json_returns_default() {
        let dir = temp_dir();
        super::super::ensure_dir(&state_dir(&dir)).unwrap();
        std::fs::write(state_path(&dir, "garbage"), b"{ not valid json ]]]").unwrap();

        let loaded = load(&dir, "garbage");

        assert_eq!(loaded, ObserverState::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_partial_json_tolerated() {
        let dir = temp_dir();
        super::super::ensure_dir(&state_dir(&dir)).unwrap();
        // Old/partial JSON missing fields → defaults fill in.
        std::fs::write(
            state_path(&dir, "partial"),
            br#"{ "watermark": 99 }"#,
        )
        .unwrap();

        let loaded = load(&dir, "partial");

        assert_eq!(loaded.watermark, 99);
        assert_eq!(loaded.jsonl_stem, "");
        assert_eq!(loaded.rolling_summary, "");

        std::fs::remove_dir_all(&dir).ok();
    }
}
