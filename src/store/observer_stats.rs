//! Project-level observer run stats (transparency, not a circuit-breaker).
//!
//! Writes `<knowledge_dir>/observer-stats.json` — a flat, consumer-facing summary a
//! host (e.g. Enclade's status bar) can display to show how much subscription usage
//! the transparent observer is burning. This file is **purely informational**: it
//! never gates or stops a run. Every observer-agent run (success AND failure — a
//! failed `claude` call still consumed usage) is recorded here.
//!
//! Best-effort throughout: any IO/serde error is logged via `eprintln!` and swallowed;
//! `record_run` never panics and never propagates.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One recorded observer-agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRec {
    /// Unix seconds when the run completed.
    pub at: u64,
    /// Number of store ops applied by this run (0 on failure).
    pub ops: i64,
    /// Whether the observer-agent call succeeded.
    pub ok: bool,
}

/// The flat, consumer-facing stats document persisted to `observer-stats.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserverStats {
    pub schema: u32,
    pub total_runs: u64,
    pub last_run_at: u64,
    pub last_ops: i64,
    pub last_ok: bool,
    pub runs_10m: u64,
    pub runs_1h: u64,
    pub runs_24h: u64,
    pub min_interval_secs: u64,
    pub updated_at: u64,
    /// Circuit-breaker state: `true` when the daemon refused a spawn (too many recent
    /// runs) and marked the store paused. Consumed by the host status bar. Auto-clears
    /// on the next successful `record_run`. A blocked spawn is NOT recorded as a run,
    /// so the recent-window count decays and later triggers can resume.
    #[serde(default)]
    pub paused: bool,
    /// Human-readable reason the circuit-breaker paused (empty when not paused).
    /// Consumed by the host status bar alongside `paused`.
    #[serde(default)]
    pub paused_reason: String,
    pub recent: Vec<RunRec>,
}

impl Default for ObserverStats {
    fn default() -> Self {
        ObserverStats {
            schema: 1,
            total_runs: 0,
            last_run_at: 0,
            last_ops: 0,
            last_ok: false,
            runs_10m: 0,
            runs_1h: 0,
            runs_24h: 0,
            min_interval_secs: 0,
            updated_at: 0,
            paused: false,
            paused_reason: String::new(),
            recent: Vec::new(),
        }
    }
}

/// How long `recent` entries are kept (24h rolling window).
const KEEP_SECS: u64 = 24 * 60 * 60;

fn stats_path(knowledge_dir: &Path) -> std::path::PathBuf {
    knowledge_dir.join("observer-stats.json")
}

/// Unix seconds now (mirrors the daemon's `now_secs`; saturates to 0 on clock error).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Count records within `[now - window_secs, now]` (inclusive). Pure + testable.
fn count_within(recent: &[RunRec], now: u64, window_secs: u64) -> u64 {
    let cutoff = now.saturating_sub(window_secs);
    recent.iter().filter(|r| r.at >= cutoff).count() as u64
}

/// Drop records older than `keep_secs` (keep those within the rolling window). Pure.
fn prune(recent: Vec<RunRec>, now: u64, keep_secs: u64) -> Vec<RunRec> {
    let cutoff = now.saturating_sub(keep_secs);
    recent.into_iter().filter(|r| r.at >= cutoff).collect()
}

/// Load the existing stats file, or `ObserverStats::default()` if missing/corrupt.
fn load(knowledge_dir: &Path) -> ObserverStats {
    std::fs::read_to_string(stats_path(knowledge_dir))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Record one observer-agent run (success or failure) into `observer-stats.json`.
///
/// Best-effort: on any IO/serde error, logs via `eprintln!` and returns — never panics,
/// never propagates. Increments `total_runs`, appends a `RunRec`, prunes `recent` to the
/// last 24h, recomputes the rolling windows, and writes atomically (temp + rename).
pub fn record_run(knowledge_dir: &Path, ops: i64, ok: bool, min_interval_secs: u64) {
    let now = now_secs();
    let mut stats = load(knowledge_dir);

    stats.total_runs += 1;
    stats.recent.push(RunRec { at: now, ops, ok });
    stats.recent = prune(std::mem::take(&mut stats.recent), now, KEEP_SECS);

    stats.runs_10m = count_within(&stats.recent, now, 10 * 60);
    stats.runs_1h = count_within(&stats.recent, now, 60 * 60);
    stats.runs_24h = count_within(&stats.recent, now, 24 * 60 * 60);

    stats.last_run_at = now;
    stats.last_ops = ops;
    stats.last_ok = ok;
    stats.updated_at = now;
    stats.min_interval_secs = min_interval_secs;
    stats.schema = 1;
    // A successful (recorded) run means the circuit-breaker is not tripped: clear paused.
    stats.paused = false;
    stats.paused_reason = String::new();

    if let Err(e) = write_atomic(knowledge_dir, &stats) {
        eprintln!("[observer-stats] write failed: {e}");
    }
}

/// Count recorded runs within the last `window_secs` (rolling window ending now).
/// Reads `observer-stats.json` (missing/corrupt → 0) and reuses [`count_within`].
/// Used by the daemon's circuit-breaker gate to decide whether to refuse a spawn.
pub fn count_in_window(knowledge_dir: &Path, window_secs: u64) -> usize {
    let stats = load(knowledge_dir);
    count_within(&stats.recent, now_secs(), window_secs) as usize
}

/// Mark the store paused (circuit-breaker tripped) with a human-readable `reason`.
///
/// Loads-or-defaults the stats file, sets `paused=true` + `paused_reason=reason`,
/// refreshes `updated_at`, and writes atomically. Deliberately does NOT push a
/// `RunRec`: a blocked spawn is not a run, so the recent-window count keeps decaying
/// and a later trigger auto-resumes once it drops below the cap. Best-effort: logs +
/// returns on any IO/serde error, never panics.
pub fn set_paused(knowledge_dir: &Path, reason: &str) {
    let mut stats = load(knowledge_dir);
    stats.paused = true;
    stats.paused_reason = reason.to_string();
    stats.updated_at = now_secs();
    stats.schema = 1;

    if let Err(e) = write_atomic(knowledge_dir, &stats) {
        eprintln!("[observer-stats] set_paused write failed: {e}");
    }
}

/// Write the stats file atomically (temp + rename), mirroring `daemon::write_port_file`.
fn write_atomic(knowledge_dir: &Path, stats: &ObserverStats) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(knowledge_dir)?;
    let path = stats_path(knowledge_dir);
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let body = serde_json::to_string_pretty(stats)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("obs-stats-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rec(at: u64) -> RunRec {
        RunRec { at, ops: 1, ok: true }
    }

    #[test]
    fn count_within_windows() {
        let now = 1_000_000u64;
        let recent = vec![
            rec(now),                 // 0s ago
            rec(now - 5 * 60),        // 5m ago
            rec(now - 30 * 60),       // 30m ago
            rec(now - 2 * 60 * 60),   // 2h ago
            rec(now - 23 * 60 * 60),  // 23h ago
        ];
        assert_eq!(count_within(&recent, now, 10 * 60), 2); // now + 5m
        assert_eq!(count_within(&recent, now, 60 * 60), 3); // + 30m
        assert_eq!(count_within(&recent, now, 24 * 60 * 60), 5); // all
    }

    #[test]
    fn count_within_handles_clock_skew() {
        // now < record.at must not panic; saturating cutoff → all counted.
        let recent = vec![rec(100), rec(200)];
        assert_eq!(count_within(&recent, 50, 10), 2);
    }

    #[test]
    fn prune_drops_old_keeps_recent() {
        let now = 1_000_000u64;
        let keep = 24 * 60 * 60;
        let recent = vec![
            rec(now),                       // keep
            rec(now - 60),                  // keep
            rec(now - keep + 1),            // keep (just inside)
            rec(now - keep - 1),            // drop (just outside)
            rec(now - 48 * 60 * 60),        // drop (2 days)
        ];
        let pruned = prune(recent, now, keep);
        assert_eq!(pruned.len(), 3);
        assert!(pruned.iter().all(|r| r.at >= now - keep));
    }

    #[test]
    fn record_run_roundtrip_two_calls() {
        let dir = temp_dir();
        record_run(&dir, 3, true, 45);
        record_run(&dir, 0, false, 45);

        let stats = load(&dir);
        assert_eq!(stats.schema, 1);
        assert_eq!(stats.total_runs, 2);
        assert_eq!(stats.recent.len(), 2);
        assert_eq!(stats.runs_1h, 2);
        assert_eq!(stats.runs_24h, 2);
        // last_* reflect the most recent (failure) call.
        assert_eq!(stats.last_ops, 0);
        assert!(!stats.last_ok);
        assert_eq!(stats.min_interval_secs, 45);
        assert!(stats.last_run_at > 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_record_excluded_from_windows() {
        // Synthetic timestamps: an old record is pruned/excluded from the windows.
        let now = 1_000_000u64;
        let recent = vec![
            rec(now),
            rec(now - 25 * 60 * 60), // older than 24h → pruned out
        ];
        let pruned = prune(recent, now, KEEP_SECS);
        assert_eq!(pruned.len(), 1, "the >24h record is pruned");
        assert_eq!(count_within(&pruned, now, 24 * 60 * 60), 1);
        assert_eq!(count_within(&pruned, now, 60 * 60), 1);
    }

    #[test]
    fn count_in_window_counts_recent_runs() {
        let dir = temp_dir();
        // Three runs recorded now → all within a 10m window.
        record_run(&dir, 1, true, 45);
        record_run(&dir, 1, true, 45);
        record_run(&dir, 1, true, 45);
        assert_eq!(count_in_window(&dir, 10 * 60), 3);
        // A 0-second window counts only runs at exactly `now` — still all 3 here.
        assert!(count_in_window(&dir, 0) <= 3);
        // Missing file → 0 (no panic).
        let empty = temp_dir();
        std::fs::remove_dir_all(&empty).ok();
        assert_eq!(count_in_window(&empty, 10 * 60), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_paused_then_record_run_clears_it() {
        let dir = temp_dir();
        set_paused(&dir, "circuit breaker tripped");
        let s = load(&dir);
        assert!(s.paused, "set_paused marks paused=true");
        assert_eq!(s.paused_reason, "circuit breaker tripped");
        assert!(s.updated_at > 0, "updated_at refreshed");
        // set_paused must NOT push a run record.
        assert_eq!(s.total_runs, 0, "a blocked spawn is not a run");
        assert!(s.recent.is_empty(), "set_paused records no RunRec");

        // A successful run clears the paused state.
        record_run(&dir, 2, true, 45);
        let s2 = load(&dir);
        assert!(!s2.paused, "record_run clears paused");
        assert_eq!(s2.paused_reason, "");
        assert_eq!(s2.total_runs, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn schema_roundtrips_paused_fields() {
        let dir = temp_dir();
        set_paused(&dir, "too many runs");
        // Re-read from disk → the two new fields round-trip.
        let s = load(&dir);
        assert!(s.paused);
        assert_eq!(s.paused_reason, "too many runs");

        // A stats file predating the fields (missing paused/paused_reason) still loads
        // via serde defaults → paused=false, paused_reason="".
        let legacy = r#"{
            "schema": 1, "total_runs": 5, "last_run_at": 100, "last_ops": 2,
            "last_ok": true, "runs_10m": 1, "runs_1h": 3, "runs_24h": 5,
            "min_interval_secs": 45, "updated_at": 100, "recent": []
        }"#;
        std::fs::write(stats_path(&dir), legacy).unwrap();
        let s2 = load(&dir);
        assert_eq!(s2.total_runs, 5);
        assert!(!s2.paused, "legacy file → paused defaults false");
        assert_eq!(s2.paused_reason, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_run_missing_dir_is_created() {
        // knowledge_dir doesn't exist yet → write_atomic creates it; no panic.
        let base = temp_dir();
        let kdir = base.join("nested").join("knowledge");
        record_run(&kdir, 2, true, 30);
        let stats = load(&kdir);
        assert_eq!(stats.total_runs, 1);
        std::fs::remove_dir_all(&base).ok();
    }
}
