//! File-based knowledge store (`.md`-per-node) + migration + per-session state.
//!
//! ## `knowledge_dir` convention
//! Every store function here takes a `knowledge_dir` that **directly** holds
//! `nodes/`, `edges.json`, `fragen.json`, `quellen.json`, `observer-state/`, and
//! `observer-hints/`. The caller resolves the directory.

pub mod knowledge_store;
pub mod migrate;
pub mod observer_hint;
pub mod observer_state;
pub mod observer_stats;

use std::path::Path;

use crate::error::Result;

/// Ensure a directory exists, creating it (and parents) if necessary.
pub(crate) fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

/// RFC3339 UTC timestamp used for a node's `datum`.
pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
