//! Per-project Knowledge-Daemon.
//!
//! A long-lived, standalone process. One daemon per project. It
//! is the **single writer** of that project's knowledge store: it binds a loopback
//! IPC endpoint, advertises it via a port-file (`<knowledge_dir>/.daemon`), and
//! serves the knowledge protocol (the existing store ops via
//! [`crate::dispatch::dispatch_store`] plus daemon ops: `health` / `hint_pop` /
//! `trigger` / `subscribe`).
//!
//! ## knowledge_dir + project_dir resolution (plugin)
//! The per-project [`crate::config::Config`] decides the knowledge dir (default
//! `.claude/knowledge`, possibly absolute). The daemon therefore holds the
//! **project_dir** as its anchor
//! and derives the knowledge dir from it via `Config` ([`knowledge_dir_for`]). The
//! project_dir (not the kdir) is what `dispatch_store` / `apply_ops_store` /
//! `Config::resolve` all key off, so it is threaded through the serve loop directly
//! rather than reconstructed from the kdir (which may be an arbitrary absolute path).
//!
//! ## Lifecycle
//! - **Spawn-race:** whoever binds the loopback port first wins. A loser exits after
//!   a `health` connect-test.
//! - **Lazy-autostart:** [`ensure_daemon`] reads the port-file; if a live daemon
//!   answers `health` it returns `(port, token)`, otherwise it spawns
//!   `current_exe() observer-daemon` **detached** with `OBSERVER_PROJECT_DIR` set,
//!   then polls the port-file for the new endpoint.
//! - **Idle-shutdown:** if no request arrives for `Config::idle_daemon_secs`
//!   (default [`IDLE_DAEMON_SECS`]), the serve loop exits cleanly and removes the
//!   port-file.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::observer::agent::{self, OBS_MAX_TURNS, OBS_TIMEOUT};
use crate::store::observer_state::{self, ObserverState};

/// Idle-shutdown default window (seconds). The per-project `Config::idle_daemon_secs`
/// overrides this — the const is only the fallback default (matches the Config default).
pub const IDLE_DAEMON_SECS: u64 = 600;

/// How long [`ensure_daemon`] polls the port-file after spawning a new daemon
/// before giving up (best-effort autostart).
const SPAWN_POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for a freshly spawned daemon's port-file.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Per-connection read timeout inside the daemon's own accept loop.
const CONN_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Accept poll cadence — the listener is non-blocking, so we sleep this long
/// between accept attempts to keep the idle-shutdown clock responsive.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Top-N DAG nodes handed to the observer as dedup context.
const DAG_EXCERPT_LIMIT: usize = 40;

/// The port-file written to `<knowledge_dir>/.daemon`. Advertises the loopback
/// endpoint + token so clients (hooks, the GUI, `ensure_daemon`) can reach the
/// single-writer daemon. `pid`/`since` are informational (liveness diagnostics).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortFile {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// Unix seconds at daemon start.
    pub since: u64,
}

// ===================== project-dir + knowledge-dir resolution =====================

/// Resolve the daemon's project dir from the environment, the same way the hooks do
/// (plugin-aware): `CLAUDE_PROJECT_DIR` (native plugin env) → `OBSERVER_PROJECT_DIR`
/// → the first non-flag positional CLI arg → the process cwd. The returned dir is
/// NOT created here.
fn resolve_project_dir() -> PathBuf {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("OBSERVER_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .or_else(positional_project_override)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Scan the CLI args (skipping the exe + the `observer`/`daemon` subcommand tokens)
/// for the first value that looks like a project dir override.
fn positional_project_override() -> Option<String> {
    std::env::args()
        .skip(1)
        .find(|a| {
            let a = a.trim();
            !a.is_empty()
                && a != "observer"
                && a != "daemon"
                && a != "observer-daemon"
                && !a.starts_with('-')
        })
        .filter(|s| !s.trim().is_empty())
}

/// The project's knowledge dir, resolved via the per-project [`Config`] (default
/// `.claude/knowledge`, may be absolute). This is the dir that DIRECTLY holds
/// `nodes/`, `edges.json`, `observer-state/`, `observer-hints/`, and the `.daemon`
/// port-file.
pub fn knowledge_dir_for(project_dir: &Path) -> PathBuf {
    let cfg = Config::resolve(project_dir);
    cfg.knowledge_dir_abs(project_dir)
}

// ===================== port-file helpers (pure, testable) =====================

fn port_file_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join(".daemon")
}

/// Write the port-file atomically (temp + rename) with restrictive perms.
fn write_port_file(knowledge_dir: &Path, pf: &PortFile) -> std::io::Result<()> {
    std::fs::create_dir_all(knowledge_dir)?;
    let path = port_file_path(knowledge_dir);
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let body = serde_json::to_string_pretty(pf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.flush()?;
    }
    restrict_perms(&tmp);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Best-effort owner-only perms on the port-file (it carries the auth token).
#[cfg(unix)]
fn restrict_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_perms(_path: &Path) {
    // Windows: the file lives under the user's project dir; loopback + token auth
    // is the real guard. No portable mode bits to set here.
}

/// Parse the port-file. `None` if missing or corrupt (caller treats as "no daemon").
fn read_port_file(knowledge_dir: &Path) -> Option<PortFile> {
    let path = port_file_path(knowledge_dir);
    let body = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<PortFile>(&body).ok()
}

/// Remove the port-file (on clean shutdown). Best-effort.
fn remove_port_file(knowledge_dir: &Path) {
    let _ = std::fs::remove_file(port_file_path(knowledge_dir));
}

/// Connect-test a port-file's endpoint with a `health` request. `true` iff a live
/// daemon answers ok — used for staleness (a stale port-file → connect fails) and
/// for the spawn-race loser's decision.
fn health_ping(pf: &PortFile) -> bool {
    matches!(
        crate::ipc::call(pf.port, &pf.token, "", "health", json!({}), ""),
        Ok(resp) if resp.ok
    )
}

/// Decide whether `serve()` should give up as the spawn-race LOSER: a port-file
/// exists AND a live daemon answers its `health` probe. Pure given the probe fn,
/// so it is unit-testable without a second process.
fn is_existing_daemon_live(knowledge_dir: &Path, probe: impl Fn(&PortFile) -> bool) -> bool {
    read_port_file(knowledge_dir).map(|pf| probe(&pf)).unwrap_or(false)
}

// ===================== idle-shutdown decision (pure) =====================

/// Whether the daemon should idle-shut-down: `now - last_activity >= idle_secs`.
/// Pure (seconds in, bool out) so it is unit-testable.
fn should_idle_shutdown(last_activity_secs: u64, now_secs: u64, idle_secs: u64) -> bool {
    now_secs.saturating_sub(last_activity_secs) >= idle_secs
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ===================== request handler =====================

/// Route one authenticated request. `record/update/link/merge/query/get/
/// current_state/list_documents/add_fact/ping` delegate to the store dispatch
/// (the daemon is the single writer). New daemon ops are handled here.
///
/// `project_dir` is the daemon's project root (the anchor `dispatch_store` keys off).
/// `knowledge_dir` is the resolved kdir (where the hint store lives). `since` is the
/// daemon start time (Unix secs), reported by `health`.
fn handle_request(
    req: &IpcRequest,
    project_dir: &Path,
    knowledge_dir: &Path,
    since: u64,
) -> IpcResponse {
    match req.op.as_str() {
        // --- daemon-native ops ---
        "health" => IpcResponse::ok(json!({
            "ok": true,
            "since": since,
            // Per-session liveness is owned elsewhere; an empty list is honest here.
            "sessions": [],
        })),

        // UserPromptSubmit read-back: pop (read + clear) this session's hint. The
        // hint store takes the knowledge dir directly (kdir-direct convention).
        "hint_pop" => {
            let session_id = req
                .payload
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| req.session_id.clone());
            let hint = crate::store::observer_hint::take(knowledge_dir, &session_id);
            IpcResponse::ok(json!({ "hint": hint }))
        }

        // Stop-hook trigger: kick off a full extraction pass for the session on a
        // background thread (the Stop hook is async — keep the serve loop responsive).
        // A per-session in-flight guard coalesces concurrent triggers.
        "trigger" => {
            let session_id = req
                .payload
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let transcript = req
                .payload
                .get("transcript_path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if session_id.trim().is_empty() || transcript.trim().is_empty() {
                return IpcResponse::err("trigger requires session_id and transcript_path");
            }

            let project_dir = project_dir.to_path_buf();
            let transcript_path = PathBuf::from(&transcript);

            if !try_begin_pass(&session_id) {
                eprintln!("[observer-daemon] trigger: pass already in-flight for {session_id} — skipped");
                return IpcResponse::ok(json!({ "ok": true, "skipped": true }));
            }

            eprintln!("[observer-daemon] trigger: session={session_id} transcript={transcript}");
            let sid_for_thread = session_id.clone();
            std::thread::spawn(move || {
                run_trigger_pass(&project_dir, &sid_for_thread, &transcript_path);
                end_pass(&sid_for_thread);
            });

            IpcResponse::ok(json!({ "ok": true }))
        }

        // Long-lived change-event stream for the GUI. Plumbing stub: acknowledge
        // without blocking the (connection-per-request) loop.
        "subscribe" => IpcResponse::ok(json!({
            "ok": true,
            "subscribed": false,
            "note": "change-event stream wired when the GUI consumes it (Schritt 6)",
        })),

        // --- everything else → the store dispatch (single-writer path) ---
        _ => {
            // `dispatch_store` keys off `req.project_dir` (it resolves the kdir via
            // Config internally). The daemon serves ONE project, so stamp the
            // resolved project dir rather than trusting the (often-empty) field.
            let mut routed = req.clone();
            routed.project_dir = project_dir.to_string_lossy().to_string();
            crate::dispatch::dispatch_store(&routed).0
        }
    }
}

// ===================== per-session in-flight guard =====================

/// Set of session ids that currently have an extraction pass running on a
/// background thread. Process-global (one daemon per project), so a `Mutex<HashSet>`
/// is enough. Coalesces concurrent `trigger`s for the same session.
fn in_flight() -> &'static Mutex<HashSet<String>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Try to claim the in-flight slot for `session_id`. Returns `true` if the slot was
/// free (caller now owns it and must call [`end_pass`] when done), `false` if a pass
/// is already running for that session (caller should ack `skipped`).
fn try_begin_pass(session_id: &str) -> bool {
    let mut set = in_flight().lock().unwrap_or_else(|p| p.into_inner());
    set.insert(session_id.to_string())
}

/// Release the in-flight slot for `session_id` (called when the pass thread finishes).
fn end_pass(session_id: &str) {
    let mut set = in_flight().lock().unwrap_or_else(|p| p.into_inner());
    set.remove(session_id);
}

// ===================== extraction pass =====================

/// Which observer-agent turn mode this pass uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnMode {
    /// No observer conversation yet → create one (`--session-id <fresh>`).
    Create,
    /// An observer conversation exists and is below the re-seed threshold → `--resume`.
    Resume,
    /// The observer conversation hit `OBS_MAX_TURNS` → retire + re-seed a fresh one.
    Reseed,
}

/// Decide the turn mode from the persisted per-session state (pure, testable):
/// empty `observer_sid` → Create; else `Reseed` once `turn_count >= OBS_MAX_TURNS`;
/// otherwise `Resume`.
pub(crate) fn decide_turn_mode(state: &ObserverState) -> TurnMode {
    if state.observer_sid.trim().is_empty() {
        TurnMode::Create
    } else if agent::should_reseed(state.turn_count, OBS_MAX_TURNS) {
        TurnMode::Reseed
    } else {
        TurnMode::Resume
    }
}

/// One extraction pass for a session, driven by the Stop-hook trigger.
///
/// `project_dir` = the project root; `transcript_path` = the work session's JSONL
/// (from the hook). Best-effort: logs + returns on any error, always advancing the
/// watermark so the same turns aren't re-processed. The persistent per-session
/// observer agent (`agent::run_extraction`, create/resume/reseed) drives extraction;
/// ops are applied via `apply::apply_ops_store` directly (no AppHandle).
///
/// State stores take the knowledge dir DIRECTLY (kdir-direct convention), resolved
/// from the project root via `Config`.
pub(crate) fn run_trigger_pass(project_dir: &Path, session_id: &str, transcript_path: &Path) {
    use crate::observer::{apply, contract, tail};

    let project_dir_str = project_dir.to_string_lossy().to_string();
    let cfg = Config::resolve(project_dir);
    let kdir = cfg.knowledge_dir_abs(project_dir);

    let state = observer_state::load(&kdir, session_id);

    // THROTTLE: coalesce rapid Stop-hook triggers into one run. If the last observer
    // run for this session was less than `min_interval_secs` ago, skip this pass
    // entirely WITHOUT advancing the watermark or saving any state — the new turns
    // stay unconsumed and are picked up by the next trigger after the cooldown.
    // (First run: last_run_at == 0 → far in the past → not throttled.)
    if now_secs().saturating_sub(state.last_run_at) < cfg.min_interval_secs {
        eprintln!("[daemon] throttled (cooldown) — skipping pass for {session_id}");
        return;
    }

    // Read the transcript + tail new lines since the watermark, reconciling for
    // rotation/rewind (stem from the hook-provided transcript path).
    let buf = match std::fs::read(transcript_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[daemon] read transcript failed for {session_id}: {e}");
            return;
        }
    };
    let current_stem = transcript_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let watermark = tail::reconcile_watermark(
        &state.jsonl_stem,
        state.watermark,
        buf.len() as u64,
        &current_stem,
    );
    let (raw_lines, new_watermark) = tail::new_lines(&buf, watermark);
    if raw_lines.is_empty() {
        persist_trigger_state(&kdir, session_id, &current_stem, new_watermark, &state, None);
        return;
    }

    // Strip injected lines (empty sentinel = nothing) + large tool_results.
    let lines: Vec<String> = raw_lines
        .into_iter()
        .filter(|l| !tail::is_injected(l, ""))
        .map(|l| tail::strip_tool_results(&l, tail::MAX_TOOL_RESULT_LEN))
        .collect();
    if lines.is_empty() {
        persist_trigger_state(&kdir, session_id, &current_stem, new_watermark, &state, None);
        return;
    }

    let delta_turns = crate::observer::format_turns(&lines);
    if delta_turns.trim().is_empty() {
        persist_trigger_state(&kdir, session_id, &current_stem, new_watermark, &state, None);
        return;
    }
    let dag_excerpt = crate::observer::dag_excerpt(&kdir, DAG_EXCERPT_LIMIT);

    // Decide create/resume/reseed and build the matching observer-agent input.
    let mode = decide_turn_mode(&state);
    let prompt = contract::EXTRACTION_PROMPT;
    let (observer_sid, turn_count, is_first, input) = match mode {
        TurnMode::Create => {
            let sid = agent::new_observer_sid();
            let input = agent::build_create_input(prompt, &delta_turns, &dag_excerpt);
            (sid, 1u32, true, input)
        }
        TurnMode::Reseed => {
            let sid = agent::new_observer_sid();
            let input = agent::build_reseed_input(prompt, &state.rolling_summary, &dag_excerpt);
            (sid, 1u32, true, input)
        }
        TurnMode::Resume => {
            let input = agent::build_resume_input(&delta_turns, &dag_excerpt);
            (state.observer_sid.clone(), state.turn_count + 1, false, input)
        }
    };

    // Run the observer agent. On error: log, persist the advanced watermark AND the
    // observer_sid/turn_count we just set (so a Create/Reseed sticks), and return.
    let stdout = match agent::run_extraction(
        &project_dir_str,
        &observer_sid,
        is_first,
        &input,
        OBS_TIMEOUT,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[daemon] observer extraction failed for {session_id}: {e}");
            // A failed `claude` call still consumed subscription usage → record it.
            crate::store::observer_stats::record_run(&kdir, 0, false, cfg.min_interval_secs);
            persist_advanced_state(
                &kdir,
                session_id,
                &current_stem,
                new_watermark,
                &state.rolling_summary,
                &observer_sid,
                turn_count,
            );
            return;
        }
    };

    let result = contract::parse(&stdout);
    let applied = apply::apply_ops_store(&project_dir_str, session_id, &result);
    crate::store::observer_stats::record_run(&kdir, applied as i64, true, cfg.min_interval_secs);
    eprintln!("[daemon] applied {applied} op(s) for session {session_id}");

    // Persist state: keep the new rolling_summary if the extractor produced one.
    let new_summary = if result.rolling_summary.trim().is_empty() {
        state.rolling_summary.clone()
    } else {
        result.rolling_summary.clone()
    };
    persist_advanced_state(
        &kdir,
        session_id,
        &current_stem,
        new_watermark,
        &new_summary,
        &observer_sid,
        turn_count,
    );

    // Hints (reuse Slice-B logic): index-delta + LLM relevance nudge. Best-effort.
    generate_hints(&kdir, session_id, result.hint.trim());

    // TODO Schritt 6: emit change event to subscribers (the GUI consumes the
    // change-event stream then). No push here yet.
}

/// Persist state when nothing was extracted this pass (no observer run): advance the
/// watermark + stem, keep the prior summary, and carry the existing observer_sid/
/// turn_count unchanged. `_new_summary` is reserved for symmetry (always `None` here).
fn persist_trigger_state(
    knowledge_dir: &Path,
    session_id: &str,
    stem: &str,
    watermark: u64,
    prev: &ObserverState,
    _new_summary: Option<&str>,
) {
    let next = ObserverState {
        jsonl_stem: stem.to_string(),
        watermark,
        rolling_summary: prev.rolling_summary.clone(),
        observer_sid: prev.observer_sid.clone(),
        turn_count: prev.turn_count,
        // No observer run happened on this no-new-turns path → carry the prior value.
        last_run_at: prev.last_run_at,
    };
    if let Err(e) = observer_state::save(knowledge_dir, session_id, &next) {
        eprintln!("[daemon] persist state failed for {session_id}: {e}");
    }
}

/// Persist the fully-advanced state after an observer run (watermark + summary +
/// observer_sid/turn_count as set by the pass).
#[allow(clippy::too_many_arguments)]
fn persist_advanced_state(
    knowledge_dir: &Path,
    session_id: &str,
    stem: &str,
    watermark: u64,
    rolling_summary: &str,
    observer_sid: &str,
    turn_count: u32,
) {
    let next = ObserverState {
        jsonl_stem: stem.to_string(),
        watermark,
        rolling_summary: rolling_summary.to_string(),
        observer_sid: observer_sid.to_string(),
        turn_count,
        // This is only called after a real observer-agent run → stamp it now so the
        // next trigger's throttle window starts from here.
        last_run_at: now_secs(),
    };
    if let Err(e) = observer_state::save(knowledge_dir, session_id, &next) {
        eprintln!("[daemon] persist state failed for {session_id}: {e}");
    }
}

/// Drop at most one Index-Delta line and one LLM-relevance line as a pending hint
/// for this session (reuses the Slice-B helpers from `observer::mod`). Best-effort.
/// `knowledge_dir` is the kdir directly (where the hint/state stores live).
fn generate_hints(knowledge_dir: &Path, session_id: &str, llm_hint: &str) {
    use crate::observer::{compute_index_delta, format_delta_hint, seed_baseline_datum};
    use crate::store::observer_hint;

    let mut hs = observer_hint::load_state(knowledge_dir, session_id);
    let g = crate::store::knowledge_store::query(knowledge_dir);

    // (a) Index-Delta — seed the baseline on first run, else compute the delta.
    if hs.last_informed_datum.is_empty() {
        hs.last_informed_datum = seed_baseline_datum(&g);
        if let Err(e) = observer_hint::save_state(knowledge_dir, session_id, &hs) {
            eprintln!("[daemon] save hint-state (seed) failed: {e}");
        }
    } else {
        let delta = compute_index_delta(&g, session_id, &hs);
        if !delta.is_empty() {
            let line = format_delta_hint(&delta);
            if let Err(e) = observer_hint::append(knowledge_dir, session_id, &line) {
                eprintln!("[daemon] append index-delta hint failed: {e}");
            }
            for n in &delta {
                hs.mark_informed(&n.id, &n.datum);
            }
            if let Err(e) = observer_hint::save_state(knowledge_dir, session_id, &hs) {
                eprintln!("[daemon] save hint-state (delta) failed: {e}");
            }
        }
    }

    // (b) LLM relevance nudge — append the trimmed pointer if present.
    if !llm_hint.is_empty() {
        let line = format!("[Knowledge] {llm_hint}");
        if let Err(e) = observer_hint::append(knowledge_dir, session_id, &line) {
            eprintln!("[daemon] append llm hint failed: {e}");
        }
    }
}

// ===================== serve loop =====================

/// Daemon entry point. Resolves the project dir (env/arg/cwd), derives the knowledge
/// dir via `Config`, binds the loopback endpoint, writes the port-file, and runs the
/// serve loop with idle-shutdown. On a spawn-race loss it exits cleanly. Best-effort:
/// any fatal setup error logs + returns.
pub fn serve() {
    let project_dir = resolve_project_dir();
    let cfg = Config::resolve(&project_dir);
    let knowledge_dir = cfg.knowledge_dir_abs(&project_dir);

    // Spawn-race: if a live daemon already owns this project, we are the loser.
    if is_existing_daemon_live(&knowledge_dir, health_ping) {
        eprintln!(
            "[observer-daemon] another daemon already serves {} — exiting (race loser)",
            knowledge_dir.display()
        );
        return;
    }

    let (listener, port, token) = match crate::ipc::bind() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[observer-daemon] bind failed: {e}");
            return;
        }
    };

    let since = now_secs();
    let pf = PortFile { port, token: token.clone(), pid: std::process::id(), since };
    if let Err(e) = write_port_file(&knowledge_dir, &pf) {
        eprintln!("[observer-daemon] port-file write failed: {e}");
        return;
    }

    eprintln!(
        "[observer-daemon] serving {} on 127.0.0.1:{port} (pid {})",
        knowledge_dir.display(),
        pf.pid
    );

    run_serve_loop(
        listener,
        token,
        project_dir.clone(),
        knowledge_dir.clone(),
        since,
        cfg.idle_daemon_secs,
    );
    remove_port_file(&knowledge_dir);
    eprintln!("[observer-daemon] idle shutdown — port-file removed");
}

/// The accept loop with idle-shutdown. Non-blocking listener polled on a short
/// cadence so the idle clock stays responsive even with no traffic. Each accepted
/// connection is handled inline (connection-per-request). Returns when idle.
fn run_serve_loop(
    listener: TcpListener,
    token: String,
    project_dir: PathBuf,
    knowledge_dir: PathBuf,
    since: u64,
    idle_secs: u64,
) {
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|e| eprintln!("[observer-daemon] set_nonblocking: {e}"));

    let last_activity = Arc::new(AtomicU64::new(now_secs()));

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                last_activity.store(now_secs(), Ordering::SeqCst);
                if let Err(e) = handle_conn(stream, &token, &project_dir, &knowledge_dir, since) {
                    eprintln!("[observer-daemon] connection error: {e}");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if should_idle_shutdown(last_activity.load(Ordering::SeqCst), now_secs(), idle_secs) {
                    return;
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => {
                eprintln!("[observer-daemon] accept error: {e}");
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

/// Read one newline-delimited JSON request, authenticate by token, route it, and
/// write one JSON response back. Mirrors `ipc::handle_conn`'s framing.
fn handle_conn(
    stream: TcpStream,
    expected_token: &str,
    project_dir: &Path,
    knowledge_dir: &Path,
    since: u64,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CONN_READ_TIMEOUT))?;

    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut line)?;
    }

    let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
        Ok(req) if req.token == expected_token => {
            handle_request(&req, project_dir, knowledge_dir, since)
        }
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

// ===================== lazy autostart =====================

/// Ensure a daemon is serving `knowledge_dir`, returning its `(port, token)`.
///
/// 1. If the port-file points at a live daemon (`health` ok) → return it.
/// 2. Otherwise spawn `current_exe() observer-daemon` **detached** with
///    `OBSERVER_PROJECT_DIR` set, then poll the port-file (bounded) for the endpoint.
///
/// Best-effort: `None` on any failure (the caller degrades gracefully). `project_dir`
/// is the daemon's anchor (set on the spawned child so it resolves the same kdir).
pub fn ensure_daemon(project_dir: &Path, knowledge_dir: &Path) -> Option<(u16, String)> {
    if let Some(pf) = read_port_file(knowledge_dir) {
        if health_ping(&pf) {
            return Some((pf.port, pf.token));
        }
    }

    spawn_detached_daemon(project_dir)?;

    // Poll the port-file for the freshly spawned daemon's endpoint.
    let deadline = std::time::Instant::now() + SPAWN_POLL_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Some(pf) = read_port_file(knowledge_dir) {
            if health_ping(&pf) {
                return Some((pf.port, pf.token));
            }
        }
        std::thread::sleep(SPAWN_POLL_INTERVAL);
    }
    None
}

/// Spawn `current_exe() observer-daemon` detached, with `OBSERVER_PROJECT_DIR` set to
/// the project root. Returns `Some(())` on a successful spawn (we don't wait — the
/// caller polls the port-file).
fn spawn_detached_daemon(project_dir: &Path) -> Option<()> {
    let exe = std::env::current_exe().ok()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("observer-daemon")
        .env("OBSERVER_PROJECT_DIR", project_dir)
        // Don't inherit a nested-session marker into the daemon's own env.
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_SESSION")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    detach(&mut cmd);

    match cmd.spawn() {
        Ok(_child) => Some(()),
        Err(e) => {
            eprintln!("[observer-daemon] autostart spawn failed: {e}");
            None
        }
    }
}

/// Windows: no console window for the detached daemon (`CREATE_NO_WINDOW`).
#[cfg(target_os = "windows")]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

/// Unix: start a new session so the daemon survives the spawner's exit.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and only detaches the child into its
    // own session; no allocation or shared-state mutation in the child pre-exec.
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
}
#[cfg(not(any(unix, target_os = "windows")))]
fn detach(_cmd: &mut std::process::Command) {}

/// `setsid(2)` without pulling in the `libc` crate: it has no arguments and a
/// stable syscall ABI. Best-effort (ignore the return value).
#[cfg(unix)]
fn libc_setsid() {
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        let _ = setsid();
    }
}

// ===================== Stop-hook trigger subcommand =====================

/// The subset of the Claude Code `Stop`-hook stdin JSON the trigger cares about.
/// `session_id`/`transcript_path`/`cwd` are best-effort (any missing field stays
/// `None`); we never block or panic on a malformed/empty stdin.
#[derive(Debug, Default, PartialEq)]
struct TriggerInput {
    session_id: Option<String>,
    transcript_path: Option<String>,
    cwd: Option<String>,
}

/// Best-effort read+parse of the Stop-hook stdin JSON. Claude writes the object then
/// closes stdin, so a plain read-to-EOF returns promptly; any error degrades to all-`None`.
fn read_trigger_input() -> TriggerInput {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    parse_trigger_input(&buf)
}

/// Pure parse of the Stop-hook stdin JSON → [`TriggerInput`] (unit-testable).
fn parse_trigger_input(raw: &str) -> TriggerInput {
    let val: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
    let s = |k: &str| {
        val.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    TriggerInput { session_id: s("session_id"), transcript_path: s("transcript_path"), cwd: s("cwd") }
}

/// Resolve the trigger's session id: prefer `OBSERVER_SESSION_ID`, else the stdin
/// `session_id` (plugin/standalone), else empty. Pure given the env value.
fn resolve_trigger_session_id(input: &TriggerInput, env_session_id: Option<&str>) -> String {
    env_session_id
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| input.session_id.clone())
        .unwrap_or_default()
}

/// Resolve the project dir for the trigger (plugin-aware): hook-stdin `cwd` →
/// `CLAUDE_PROJECT_DIR` (plugin env) → `OBSERVER_PROJECT_DIR` → the process cwd.
fn resolve_trigger_project_dir(input: &TriggerInput) -> PathBuf {
    input
        .cwd
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("CLAUDE_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .or_else(|| std::env::var("OBSERVER_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `observer observer-trigger` — the `Stop`-hook entry point.
///
/// Reads the hook stdin JSON, resolves the session id (env-over-stdin) + the
/// transcript path (absolute, from the hook), lazy-autostarts the per-project
/// daemon, and fires a fire-and-forget `trigger` IPC. **Always exits 0**; never
/// hangs, never panics. The actual extraction runs in the daemon's background.
pub fn run_trigger_subcommand() {
    let input = read_trigger_input();

    // No transcript → nothing to extract.
    let transcript_path = match input.transcript_path.clone() {
        Some(t) => t,
        None => return,
    };

    let session_id =
        resolve_trigger_session_id(&input, std::env::var("OBSERVER_SESSION_ID").ok().as_deref());
    let project_dir = resolve_trigger_project_dir(&input);
    let knowledge_dir = knowledge_dir_for(&project_dir);

    let (port, token) = match ensure_daemon(&project_dir, &knowledge_dir) {
        Some(pt) => pt,
        None => {
            eprintln!("[observer] observer-trigger: daemon autostart failed — skipping");
            return;
        }
    };

    // Fire-and-forget: the daemon runs the pass on a background thread (the Stop
    // hook is async), so we just deliver the trigger and exit. Any IPC error logs.
    if let Err(e) = crate::ipc::call(
        port,
        &token,
        "",
        "trigger",
        json!({ "session_id": session_id, "transcript_path": transcript_path }),
        &session_id,
    ) {
        eprintln!("[observer] observer-trigger: IPC failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_knowledge_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("observer-daemon-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ---------------- port-file round-trip ----------------

    #[test]
    fn port_file_roundtrip_is_identical() {
        let dir = tmp_knowledge_dir();
        let pf = PortFile { port: 54321, token: "tok-abc".into(), pid: 4242, since: 1_700_000_000 };
        write_port_file(&dir, &pf).unwrap();
        let read = read_port_file(&dir).expect("read back");
        assert_eq!(read, pf);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn port_file_missing_is_none() {
        let dir = tmp_knowledge_dir();
        assert!(read_port_file(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn port_file_corrupt_is_none() {
        let dir = tmp_knowledge_dir();
        std::fs::write(port_file_path(&dir), b"{ not valid json ]]]").unwrap();
        assert!(read_port_file(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- spawn-race decision (pure) ----------------

    #[test]
    fn spawn_race_loser_when_live_port_file_present() {
        let dir = tmp_knowledge_dir();
        let pf = PortFile { port: 1, token: "t".into(), pid: 1, since: 0 };
        write_port_file(&dir, &pf).unwrap();
        assert!(is_existing_daemon_live(&dir, |_| true));
        assert!(!is_existing_daemon_live(&dir, |_| false));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawn_race_no_port_file_is_not_loser() {
        let dir = tmp_knowledge_dir();
        assert!(!is_existing_daemon_live(&dir, |_| true));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- idle-shutdown decision (pure) ----------------

    #[test]
    fn idle_shutdown_decision() {
        assert!(!should_idle_shutdown(1000, 1000 + IDLE_DAEMON_SECS - 1, IDLE_DAEMON_SECS));
        assert!(should_idle_shutdown(1000, 1000 + IDLE_DAEMON_SECS, IDLE_DAEMON_SECS));
        assert!(should_idle_shutdown(1000, 1000 + IDLE_DAEMON_SECS + 5, IDLE_DAEMON_SECS));
        // Clock skew (now < last) must not underflow into a shutdown.
        assert!(!should_idle_shutdown(2000, 1000, IDLE_DAEMON_SECS));
    }

    // ---------------- localhost round-trip ----------------

    /// Start the daemon's own accept loop on an ephemeral port in a thread, returning
    /// `(port, token)`. `project_dir` is the routing anchor; `knowledge_dir` is where
    /// the hint store lives. A huge idle window keeps the loop up for the test.
    fn start_test_daemon(project_dir: &Path, knowledge_dir: &Path) -> (u16, String) {
        let (listener, port, token) = crate::ipc::bind().expect("bind");
        let token_srv = token.clone();
        let pd = project_dir.to_path_buf();
        let kd = knowledge_dir.to_path_buf();
        std::thread::spawn(move || {
            run_serve_loop(listener, token_srv, pd, kd, 0, u64::MAX);
        });
        (port, token)
    }

    #[test]
    fn daemon_health_roundtrip() {
        let dir = tmp_knowledge_dir();
        let (port, token) = start_test_daemon(&dir, &dir);
        let resp = crate::ipc::call(port, &token, "", "health", json!({}), "").expect("call");
        assert!(resp.ok, "health should be ok: {resp:?}");
        assert!(resp.data["since"].is_u64());
        assert!(resp.data["sessions"].is_array());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daemon_hint_pop_returns_and_clears() {
        // The project dir is the tempdir; the knowledge dir is the resolved kdir
        // (default `.claude/knowledge`), where the hint store keys off directly.
        let project = tmp_knowledge_dir();
        let knowledge_dir = knowledge_dir_for(&project);
        std::fs::create_dir_all(&knowledge_dir).unwrap();
        crate::store::observer_hint::append(&knowledge_dir, "sess-9", "Index updated: Foo").unwrap();

        let (port, token) = start_test_daemon(&project, &knowledge_dir);
        let resp = crate::ipc::call(
            port,
            &token,
            "",
            "hint_pop",
            json!({ "session_id": "sess-9" }),
            "",
        )
        .expect("call");
        assert!(resp.ok);
        assert_eq!(resp.data["hint"].as_str(), Some("Index updated: Foo"));

        // Second pop is empty (cleared).
        let resp2 = crate::ipc::call(
            port,
            &token,
            "",
            "hint_pop",
            json!({ "session_id": "sess-9" }),
            "",
        )
        .expect("call");
        assert_eq!(resp2.data["hint"].as_str(), Some(""));
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn daemon_trigger_acked_and_spawns_pass() {
        // A trigger with both fields is acked `{ok}` and spawns a background pass.
        // The transcript path is bogus → the pass logs a read error + returns; the
        // ack is unaffected (best-effort).
        let dir = tmp_knowledge_dir();
        let (port, token) = start_test_daemon(&dir, &dir);
        let resp = crate::ipc::call(
            port,
            &token,
            "",
            "trigger",
            json!({ "session_id": "s1", "transcript_path": "C:/t/does-not-exist.jsonl" }),
            "",
        )
        .expect("call");
        assert!(resp.ok);
        assert_eq!(resp.data["skipped"], json!(null));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daemon_trigger_missing_fields_errors() {
        let dir = tmp_knowledge_dir();
        let (port, token) = start_test_daemon(&dir, &dir);
        let resp = crate::ipc::call(
            port,
            &token,
            "",
            "trigger",
            json!({ "session_id": "s1" }), // no transcript_path
            "",
        )
        .expect("call");
        assert!(!resp.ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- in-flight guard (pure) ----------------

    #[test]
    fn in_flight_guard_coalesces_same_session_not_others() {
        let s = format!("sess-guard-{}", uuid::Uuid::new_v4());
        let other = format!("sess-other-{}", uuid::Uuid::new_v4());
        assert!(try_begin_pass(&s));
        assert!(!try_begin_pass(&s));
        assert!(try_begin_pass(&other));
        end_pass(&s);
        assert!(try_begin_pass(&s));
        end_pass(&s);
        end_pass(&other);
    }

    // ---------------- decide_turn_mode (pure) ----------------

    #[test]
    fn decide_turn_mode_create_resume_reseed() {
        let s0 = ObserverState::default();
        assert_eq!(decide_turn_mode(&s0), TurnMode::Create);

        let s1 = ObserverState {
            observer_sid: "obs-1".into(),
            turn_count: OBS_MAX_TURNS - 1,
            ..Default::default()
        };
        assert_eq!(decide_turn_mode(&s1), TurnMode::Resume);

        let s2 = ObserverState {
            observer_sid: "obs-1".into(),
            turn_count: OBS_MAX_TURNS,
            ..Default::default()
        };
        assert_eq!(decide_turn_mode(&s2), TurnMode::Reseed);
        let s3 = ObserverState {
            observer_sid: "obs-1".into(),
            turn_count: OBS_MAX_TURNS + 5,
            ..Default::default()
        };
        assert_eq!(decide_turn_mode(&s3), TurnMode::Reseed);
    }

    #[test]
    fn daemon_subscribe_acks_without_blocking() {
        let dir = tmp_knowledge_dir();
        let (port, token) = start_test_daemon(&dir, &dir);
        let resp = crate::ipc::call(port, &token, "", "subscribe", json!({}), "").expect("call");
        assert!(resp.ok);
        assert_eq!(resp.data["subscribed"], json!(false));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daemon_routes_store_ops_through_dispatch_store() {
        // The project dir is the tempdir; dispatch_store resolves the kdir via Config.
        let project = tmp_knowledge_dir();
        let knowledge_dir = knowledge_dir_for(&project);
        std::fs::create_dir_all(&knowledge_dir).unwrap();
        let (port, token) = start_test_daemon(&project, &knowledge_dir);

        // A `record` op must flow through dispatch_store and persist.
        let rec = crate::ipc::call(
            port,
            &token,
            "",
            "record",
            json!({ "typ": "observation", "inhalt": "daemon-routed node", "begruendung": "why?" }),
            "",
        )
        .expect("record call");
        assert!(rec.ok, "record should succeed: {rec:?}");

        // current_state should now reflect the store (routes through dispatch_store).
        let cur = crate::ipc::call(port, &token, "", "current_state", json!({}), "")
            .expect("current_state call");
        assert!(cur.ok);
        assert!(cur.data.get("active_decisions").is_some());

        // And the store file actually exists under the resolved knowledge dir.
        let g = crate::store::knowledge_store::query(&knowledge_dir);
        assert!(
            g.nodes.iter().any(|n| n.inhalt == "daemon-routed node"),
            "recorded node should be persisted via dispatch_store"
        );
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn daemon_unauthorized_request_rejected() {
        let dir = tmp_knowledge_dir();
        let (port, _token) = start_test_daemon(&dir, &dir);
        let resp = crate::ipc::call(port, "WRONG", "", "health", json!({}), "").expect("call");
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("unauthorized"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- LIVE: full trigger pass (real claude) ----------------
    // Run: cargo test -- --ignored --nocapture daemon_trigger_pass_extracts_live

    /// LIVE: seed a tempdir project, write a small fake work-session transcript, then
    /// call `run_trigger_pass` directly. Asserts the store gained ≥1 node, the
    /// persisted watermark advanced, the observer_sid is set, and rolling_summary is
    /// non-empty.
    #[test]
    #[ignore = "spawns real Claude (subscription); run on demand"]
    fn daemon_trigger_pass_extracts_live() {
        let project = std::env::temp_dir().join(format!("observer-daemon-live-{}", uuid::Uuid::new_v4()));
        let knowledge_dir = knowledge_dir_for(&project);
        std::fs::create_dir_all(&knowledge_dir).unwrap();

        let transcript = project.join("work.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"We will use SQLite with WAL mode for the project's local store, and the terminal will be xterm.js. Please confirm."}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":"Confirmed: the project uses SQLite in WAL mode for local storage, and xterm.js for the embedded terminal."}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Good. That is decided."}]}}"#,
        ];
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();

        run_trigger_pass(&project, "sess-live", &transcript);

        let g = crate::store::knowledge_store::query(&knowledge_dir);
        eprintln!("=== EXTRACTED {} node(s) ===", g.nodes.len());
        for n in &g.nodes {
            eprintln!("- [{}] ({:?}/{}) {}", n.id, n.typ, n.status, n.inhalt);
        }

        let state = observer_state::load(&knowledge_dir, "sess-live");
        eprintln!(
            "=== STATE: watermark={} observer_sid={:?} turn_count={} rolling_summary={:?} ===",
            state.watermark, state.observer_sid, state.turn_count, state.rolling_summary
        );

        let _ = std::fs::remove_dir_all(&project);

        assert!(!g.nodes.is_empty(), "expected ≥1 extracted node from the fake transcript");
        assert!(state.watermark > 0, "watermark should have advanced past the consumed lines");
        assert!(!state.observer_sid.is_empty(), "observer_sid should be set after the pass");
        assert_eq!(state.turn_count, 1, "first pass is a Create → turn_count 1");
        assert!(!state.rolling_summary.trim().is_empty(), "rolling_summary should be non-empty");
    }

    // ---------------- trigger resolution (pure) ----------------

    #[test]
    fn trigger_parse_extracts_fields() {
        let raw = r#"{"session_id":"s-1","transcript_path":"C:/p/work.jsonl","cwd":"C:/p","extra":1}"#;
        let t = parse_trigger_input(raw);
        assert_eq!(t.session_id.as_deref(), Some("s-1"));
        assert_eq!(t.transcript_path.as_deref(), Some("C:/p/work.jsonl"));
        assert_eq!(t.cwd.as_deref(), Some("C:/p"));
    }

    #[test]
    fn trigger_parse_blank_and_malformed_are_none() {
        let t = parse_trigger_input(r#"{"session_id":"  ","transcript_path":""}"#);
        assert_eq!(t, TriggerInput::default());
        assert_eq!(parse_trigger_input("not json at all"), TriggerInput::default());
        assert_eq!(parse_trigger_input(""), TriggerInput::default());
    }

    #[test]
    fn trigger_session_id_env_over_stdin() {
        let input = TriggerInput { session_id: Some("from-stdin".into()), ..Default::default() };
        assert_eq!(resolve_trigger_session_id(&input, Some("from-env")), "from-env");
        assert_eq!(resolve_trigger_session_id(&input, Some("   ")), "from-stdin");
        assert_eq!(resolve_trigger_session_id(&input, None), "from-stdin");
        assert_eq!(resolve_trigger_session_id(&TriggerInput::default(), None), "");
    }

    #[test]
    fn trigger_missing_transcript_is_noop() {
        let input = TriggerInput { session_id: Some("s".into()), ..Default::default() };
        assert!(input.transcript_path.is_none(), "no transcript → run_trigger_subcommand exits early");
    }

    #[test]
    fn trigger_project_dir_prefers_stdin_cwd() {
        // stdin cwd wins over everything (it is the most specific).
        let input = TriggerInput { cwd: Some("C:/from-stdin".into()), ..Default::default() };
        assert_eq!(resolve_trigger_project_dir(&input), PathBuf::from("C:/from-stdin"));
    }

    #[test]
    fn knowledge_dir_for_default_is_claude_knowledge() {
        // With no per-project config file, the default knowledge dir is
        // `<project>/.claude/knowledge`.
        let project = tmp_knowledge_dir();
        let kd = knowledge_dir_for(&project);
        assert_eq!(kd, project.join(".claude").join("knowledge"));
        std::fs::remove_dir_all(&project).ok();
    }
}
