//! Append-only, ring-limited trace of hook invocations (`<knowledge_dir>/hook-trace.log`).
//!
//! ## Why this exists
//! The `Stop` hook is registered with `"async": true`, so the host discards its stdout
//! and stderr. Together with the re-entrancy guard in `main` and three further early
//! returns in `run_trigger_subcommand`, a hook that fires but bails leaves **no evidence
//! anywhere**. Observed 2026-08-04: a session completed three turns while the daemon was
//! alive, throttle expired and breaker clear — yet no trigger arrived, and nothing on disk
//! could say whether the host had invoked the hook at all.
//!
//! ## How to read it
//! - **No line** for a turn → the host never invoked the hook (registration / plugin scope).
//! - **Line with `outcome != delivered`** → our side; the outcome names the exact branch.
//!
//! ## Guarantees
//! Best-effort and non-intrusive: every I/O error is swallowed (a diagnostic must never
//! break a hook), and the file is only written when the knowledge dir **already exists** —
//! a hook firing in a project without a store must not conjure one just to log.

use std::io::Write;
use std::path::Path;

/// Keep the log bounded: on overflow the file is rewritten to its newest `MAX_LINES`.
const MAX_LINES: usize = 500;

const FILE: &str = "hook-trace.log";

/// Outcome markers. Kept as `&str` constants so callers cannot typo a branch name that
/// the reader is grepping for.
pub mod outcome {
    /// The re-entrancy guard fired (running inside the observer's own agent).
    pub const BAILED: &str = "bailed";
    /// Stop hook: the hook payload carried no `transcript_path` → nothing to extract.
    pub const NO_TRANSCRIPT: &str = "no-transcript";
    /// Stop hook: the per-project daemon could not be started.
    pub const AUTOSTART_FAILED: &str = "autostart-failed";
    /// Stop hook: the daemon was reachable but the trigger IPC failed.
    pub const IPC_ERROR: &str = "ipc-error";
    /// The hook did its job (trigger delivered / context injected).
    pub const DELIVERED: &str = "delivered";
    /// The hook ran fine but had nothing to contribute (no index / no pending hint).
    pub const EMPTY: &str = "empty";
}

/// Append one trace line for a hook invocation.
///
/// `session` may be empty (rendered as `-`); `internal` reflects the
/// `KNOWLEDGE_OBSERVER_INTERNAL` guard flag.
pub fn record(knowledge_dir: &Path, sub: &str, session: &str, internal: bool, outcome: &str) {
    // Never create the store just to write a diagnostic (see module docs).
    if !knowledge_dir.is_dir() {
        return;
    }
    let session = session.trim();
    let line = format!(
        "{} {} pid={} session={} internal={} outcome={}\n",
        crate::store::now(),
        sub,
        std::process::id(),
        if session.is_empty() { "-" } else { session },
        u8::from(internal),
        outcome,
    );
    let path = knowledge_dir.join(FILE);
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if appended.is_ok() {
        trim(&path);
    }
}

/// Rewrite the file to its newest `MAX_LINES` once it grows past that. Best-effort:
/// on any read/write error the file is left as-is (an oversized log beats a lost one).
fn trim(path: &Path) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= MAX_LINES {
        return;
    }
    let keep = lines[lines.len() - MAX_LINES..].join("\n");
    let _ = std::fs::write(path, format!("{keep}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "observer-trace-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn records_one_line_per_call_with_all_fields() {
        let d = tmp_dir("basic");
        record(&d, "observer-trigger", "sess-1", false, outcome::DELIVERED);
        let body = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert_eq!(body.lines().count(), 1);
        let l = body.lines().next().unwrap();
        assert!(l.contains("observer-trigger"), "{l}");
        assert!(l.contains("session=sess-1"), "{l}");
        assert!(l.contains("internal=0"), "{l}");
        assert!(l.contains("outcome=delivered"), "{l}");
        assert!(l.contains(&format!("pid={}", std::process::id())), "{l}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn empty_session_renders_as_dash_and_internal_is_flagged() {
        let d = tmp_dir("dash");
        record(&d, "knowledge-index", "   ", true, outcome::BAILED);
        let body = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(body.contains("session=-"), "{body}");
        assert!(body.contains("internal=1"), "{body}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn appends_rather_than_overwrites() {
        let d = tmp_dir("append");
        record(&d, "observer-trigger", "s", false, outcome::NO_TRANSCRIPT);
        record(&d, "observer-trigger", "s", false, outcome::DELIVERED);
        let body = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert_eq!(body.lines().count(), 2, "{body}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ring_limits_to_the_newest_lines() {
        let d = tmp_dir("ring");
        for i in 0..(MAX_LINES + 25) {
            record(&d, "observer-trigger", &format!("s{i}"), false, outcome::DELIVERED);
        }
        let body = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert_eq!(body.lines().count(), MAX_LINES, "ring not enforced");
        // Oldest dropped, newest kept.
        assert!(!body.contains("session=s0 "), "oldest line survived");
        assert!(
            body.contains(&format!("session=s{}", MAX_LINES + 24)),
            "newest line missing"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_knowledge_dir_is_a_noop_and_creates_nothing() {
        let d = tmp_dir("absent");
        let absent = d.join("no-store-here");
        record(&absent, "knowledge-index", "s", false, outcome::EMPTY);
        assert!(!absent.exists(), "a diagnostic must not conjure a store dir");
        std::fs::remove_dir_all(&d).ok();
    }
}
