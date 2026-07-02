//! Hook subcommands: `knowledge-index` (SessionStart) / `knowledge-hint`
//! (UserPromptSubmit). Config-driven.
//!
//! These run as Claude Code **hook** processes (a fresh `observer <subcommand>`
//! invocation). They read the project's knowledge files **directly** (the store is
//! file-based — no IPC needed) and print a Claude Code `hookSpecificOutput` JSON
//! envelope on stdout so the `additionalContext` reaches the model.
//!
//! **Injection mechanism (verified):** Claude Code only forwards hook output to the
//! model when stdout is a JSON envelope
//! `{"hookSpecificOutput":{"hookEventName":"<Event>","additionalContext":"<text>"}}`
//! (exit 0). Plain stdout does NOT inject. When there is nothing to inject we print
//! **nothing** and exit 0.
//!
//! ## knowledge_dir + enabled via Config
//! The per-project [`crate::config::Config`] decides both: `knowledge_dir_abs` (the
//! dir that DIRECTLY holds `nodes/`) and `enabled` (the gate). The index caps come
//! from `cfg.max_decisions` / `cfg.max_questions`.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::Config;
use crate::store::knowledge_store::{self, KnowledgeGraph, NodeType};

// ===================== hook stdin / resolution =====================

/// The subset of the Claude Code hook stdin JSON we care about.
struct HookInput {
    cwd: Option<String>,
    session_id: Option<String>,
}

/// Read a single JSON object from stdin **without blocking indefinitely on EOF**.
///
/// Claude writes the hook-input JSON to stdin but does not always close it promptly
/// (observed: the UserPromptSubmit hook's stdin was left open, so a plain
/// `read_to_string` blocked until Claude's 30s hook timeout). We read on a background
/// thread and return the instant the accumulated bytes parse as a complete JSON value —
/// never waiting for EOF. A bounded timeout caps the pathological "nothing parseable
/// arrives" case. Returns `Value::Null` if nothing usable arrives in time.
pub(crate) fn read_stdin_json(timeout: std::time::Duration) -> Value {
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stdin.read(&mut chunk) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Done the instant we have a complete JSON object — don't wait for EOF.
                    if let Ok(s) = std::str::from_utf8(&buf) {
                        if let Ok(v) = serde_json::from_str::<Value>(s.trim()) {
                            let _ = tx.send(v);
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // EOF / error: best-effort parse of whatever arrived (may be Null).
        let v = std::str::from_utf8(&buf)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
            .unwrap_or(Value::Null);
        let _ = tx.send(v);
    });
    rx.recv_timeout(timeout).unwrap_or(Value::Null)
}

/// Best-effort read+parse of the hook stdin JSON. Uses the bounded, EOF-independent
/// [`read_stdin_json`] so a hook can never hang if Claude leaves stdin open; any error
/// degrades to empty fields (we then fall back to env / current_dir).
fn read_hook_input() -> HookInput {
    let val = read_stdin_json(std::time::Duration::from_secs(3));
    HookInput {
        cwd: val.get("cwd").and_then(Value::as_str).map(str::to_string),
        session_id: val.get("session_id").and_then(Value::as_str).map(str::to_string),
    }
}

/// Resolve the project dir for a hook (plugin-aware): hook-stdin `cwd` →
/// `CLAUDE_PROJECT_DIR` (native plugin env) → `OBSERVER_PROJECT_DIR` → the process
/// cwd.
fn resolve_project_dir(input: &HookInput) -> PathBuf {
    input
        .cwd
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("CLAUDE_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .or_else(|| std::env::var("OBSERVER_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the **host** session id for the hint file. Claude's stdin `session_id`
/// is the *claude* session id, which is not necessarily the host's — so prefer
/// `OBSERVER_SESSION_ID`, then the stdin field, else empty.
fn resolve_session_id(input: &HookInput) -> String {
    std::env::var("OBSERVER_SESSION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| input.session_id.clone().filter(|s| !s.trim().is_empty()))
        .unwrap_or_default()
}

// ===================== envelope helpers =====================

/// Wrap arbitrary text into the Claude Code `hookSpecificOutput` envelope for the
/// given hook event, serialized via serde_json (correct escaping of quotes/newlines).
fn envelope(event: &str, text: &str) -> String {
    envelope_msg(event, text, "")
}

/// Like [`envelope`], but also attaches an OPTIONAL top-level `"systemMessage"` — a
/// real Claude Code hook output field shown to the **user** in the TUI transcript (NOT
/// forwarded to the model, unlike `additionalContext`). The field is emitted only when
/// `system_message` is non-empty; `additionalContext` is unchanged either way.
fn envelope_msg(event: &str, text: &str, system_message: &str) -> String {
    let mut obj = json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    });
    if !system_message.trim().is_empty() {
        obj.as_object_mut()
            .unwrap()
            .insert("systemMessage".to_string(), Value::String(system_message.to_string()));
    }
    obj.to_string()
}

/// Unix seconds now (saturates to 0 on clock error); used for the per-session
/// user-visible-feedback marker.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ===================== index (SessionStart) =====================

/// Build the compact knowledge index text from a graph (in English; the recorded
/// node content stays in whatever language it was recorded). `None` when there is
/// nothing worth injecting (no active decisions AND no open questions). `max_decisions`
/// / `max_questions` cap the listed items (from `Config`).
///
/// On `Some`, also returns `(decisions_shown, open_questions_shown)` — the counts of
/// the items actually placed in the index — so the caller can build the user-visible
/// read-feedback `systemMessage` without recomputing the projection.
///
/// Replicates the `current_state` projection in `dispatch`: active decisions
/// (`Decision` with status `active`) and open questions (a `frage` whose leading
/// value's node is NOT `supported`).
fn build_index_text(
    g: &KnowledgeGraph,
    max_decisions: usize,
    max_questions: usize,
) -> Option<(String, usize, usize)> {
    let decisions: Vec<&str> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.typ, NodeType::Decision) && n.status == "active")
        .map(|n| n.inhalt.as_str())
        .take(max_decisions)
        .collect();

    // A question is "answered" once one value clearly leads (its fact is supported);
    // only genuinely contested/empty ones count as open.
    let mut open: Vec<(String, Option<String>)> = Vec::new();
    for fr in &g.fragen {
        let mut best: Option<(&str, f64, &str)> = None; // (wert, score, status)
        for n in &g.nodes {
            if n.frage_id.as_deref() == Some(fr.id.as_str()) {
                if let Some(w) = &n.wert {
                    if best.map_or(true, |(_, s, _)| n.score > s) {
                        best = Some((w, n.score, n.status.as_str()));
                    }
                }
            }
        }
        let answered = matches!(best, Some((_, _, st)) if st == "supported");
        if !answered {
            open.push((fr.inhalt.clone(), best.map(|(w, _, _)| w.to_string())));
        }
        if open.len() >= max_questions {
            break;
        }
    }

    if decisions.is_empty() && open.is_empty() {
        return None;
    }

    let mut out = String::from("Project knowledge (current state):\n");
    if !decisions.is_empty() {
        out.push_str("Active decisions:\n");
        for d in &decisions {
            out.push_str("- ");
            out.push_str(d);
            out.push('\n');
        }
    }
    if !open.is_empty() {
        out.push_str("Open questions:\n");
        for (frage, wert) in &open {
            out.push_str("- ");
            out.push_str(frage);
            out.push_str(" (leading: ");
            out.push_str(wert.as_deref().unwrap_or("–"));
            out.push_str(")\n");
        }
    }
    Some((out, decisions.len(), open.len()))
}

/// Build the SessionStart index envelope. `kdir` is the knowledge dir directly (holds
/// `nodes/`); `cfg` supplies the `enabled` gate + the index caps. `None` = nothing to
/// inject (knowledge disabled or empty/uninformative graph).
pub fn build_index_envelope(kdir: &Path, cfg: &Config) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let graph = knowledge_store::query(kdir);
    if graph.nodes.is_empty() {
        return None;
    }
    let (text, decisions, questions) =
        build_index_text(&graph, cfg.max_decisions, cfg.max_questions)?;
    // READ feedback: a user-visible marker of what knowledge was injected this session.
    let system_message = if cfg.feedback {
        format!("📖 Knowledge: {decisions} decision(s) · {questions} open question(s)")
    } else {
        String::new()
    };
    Some(envelope_msg("SessionStart", &text, &system_message))
}

// ===================== hint (UserPromptSubmit) =====================

/// Build the UserPromptSubmit hint envelope for a session, consuming (read+clear)
/// the pending hint. `kdir` is the knowledge dir directly. When `cfg.feedback` is on,
/// a user-visible `systemMessage` (write-delta + hint markers) may accompany — or stand
/// in for — the hint. `None` = nothing to emit (no hint AND no feedback message).
pub fn build_hint_envelope(kdir: &Path, session_id: &str, cfg: &Config) -> Option<String> {
    let hint = crate::store::observer_hint::take(kdir, session_id);
    let hint = hint.trim();
    let system_message = if cfg.feedback {
        hint_feedback_message(kdir, session_id, !hint.is_empty())
    } else {
        String::new()
    };
    build_hint_output(hint, &system_message)
}

/// Assemble the UserPromptSubmit hook JSON from a (possibly empty) hint + a
/// (possibly empty) user-visible `systemMessage`. `hookSpecificOutput.additionalContext`
/// is included only when the hint is non-empty (behavior unchanged); `systemMessage` only
/// when non-empty. `None` when both are empty (nothing to print).
fn build_hint_output(hint: &str, system_message: &str) -> Option<String> {
    let mut obj = serde_json::Map::new();
    if !hint.is_empty() {
        obj.insert(
            "hookSpecificOutput".to_string(),
            json!({
                "hookEventName": "UserPromptSubmit",
                "additionalContext": hint,
            }),
        );
    }
    if !system_message.trim().is_empty() {
        obj.insert(
            "systemMessage".to_string(),
            Value::String(system_message.to_string()),
        );
    }
    if obj.is_empty() {
        None
    } else {
        Some(Value::Object(obj).to_string())
    }
}

/// Compute the user-visible feedback `systemMessage` for a hint turn (WRITE-delta +
/// HINT markers), maintaining the per-session `last_feedback_at` baseline marker.
///
/// Side effects (best-effort — never breaks the hook): loads the per-session hint
/// state, on the FIRST turn (`last_feedback_at == 0`) seeds the marker to now WITHOUT
/// reporting any write-delta (baseline), otherwise sums store ops recorded since the
/// marker; then ALWAYS advances the marker to now and saves. Any error is logged to
/// stderr and treated as "no write-delta".
fn hint_feedback_message(kdir: &Path, session_id: &str, hint_delivered: bool) -> String {
    use crate::store::{observer_hint, observer_stats};
    let now = now_secs();
    let mut state = observer_hint::load_state(kdir, session_id);
    // First turn this session → seed baseline, do NOT report a write-delta.
    let write_ops = if state.last_feedback_at == 0 {
        0
    } else {
        observer_stats::ops_since(kdir, state.last_feedback_at)
    };
    state.last_feedback_at = now;
    if let Err(e) = observer_hint::save_state(kdir, session_id, &state) {
        eprintln!("[observer-feedback] save_state failed: {e}");
    }
    compose_hint_system_message(write_ops, hint_delivered)
}

/// Pure combiner for the hint-turn `systemMessage`: up to two parts joined by `" · "` —
/// a write-delta (`✍️ {n} recorded`, only when `write_ops > 0`) and a hint marker
/// (`💡 hint`, only when a hint was actually delivered). Empty string when neither.
fn compose_hint_system_message(write_ops: i64, hint_delivered: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if write_ops > 0 {
        parts.push(format!("✍️ {write_ops} recorded"));
    }
    if hint_delivered {
        parts.push("💡 hint".to_string());
    }
    parts.join(" · ")
}

// ===================== thin stdin/stdout wrappers =====================

/// `observer knowledge-index` — SessionStart hook entry point. Always exits 0.
pub fn run_index() {
    let input = read_hook_input();
    let project_dir = resolve_project_dir(&input);
    let cfg = Config::resolve(&project_dir);
    let kdir = cfg.knowledge_dir_abs(&project_dir);
    if let Some(s) = build_index_envelope(&kdir, &cfg) {
        println!("{s}");
    }
}

/// `observer knowledge-hint` — UserPromptSubmit hook entry point. Always exits 0.
pub fn run_hint() {
    let input = read_hook_input();
    let project_dir = resolve_project_dir(&input);
    let cfg = Config::resolve(&project_dir);
    let kdir = cfg.knowledge_dir_abs(&project_dir);
    let session_id = resolve_session_id(&input);
    if let Some(s) = build_hint_envelope(&kdir, &session_id, &cfg) {
        println!("{s}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::observer_hint;
    use serde_json::Value;

    fn fresh_kdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "observer-cli-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn enabled_cfg() -> Config {
        // enabled = true by default; feedback OFF so the core behaviour tests assert the
        // envelope without user-visible systemMessage noise (feedback tests use feedback_cfg()).
        let mut c = Config::default();
        c.feedback = false;
        c
    }

    fn disabled_cfg() -> Config {
        let mut c = Config::default();
        c.enabled = false;
        c
    }

    // ---------------- index ----------------

    #[test]
    fn index_some_with_decision_and_open_question() {
        let dir = fresh_kdir("idx-some");
        knowledge_store::add_node(
            &dir,
            NodeType::Decision,
            "Wir nutzen Tauri 2".into(),
            "damit wir eine Desktop-App haben",
            None,
            "session",
        )
        .unwrap();
        // Two competing values on the same question → contested, no value is clearly
        // `supported` → the question stays "open".
        knowledge_store::add_fact(
            &dir,
            "Welche DB?",
            "SQLite",
            "Wir verwenden SQLite".into(),
            "aus dem Code",
            None,
            "session",
        )
        .unwrap();
        knowledge_store::add_fact(
            &dir,
            "Welche DB?",
            "Postgres",
            "Vorschlag Postgres".into(),
            "aus einem Kommentar",
            None,
            "session",
        )
        .unwrap();

        let s = build_index_envelope(&dir, &enabled_cfg()).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("SessionStart")
        );
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(ctx.contains("Wir nutzen Tauri 2"), "decision text missing: {ctx}");
        assert!(ctx.contains("Active decisions:"), "header missing: {ctx}");
        assert!(ctx.contains("Welche DB?"), "open question missing: {ctx}");
        assert!(ctx.contains("leading: SQLite"), "leading value missing: {ctx}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_none_when_disabled() {
        let dir = fresh_kdir("idx-off");
        knowledge_store::add_node(
            &dir,
            NodeType::Decision,
            "Egal".into(),
            "damit",
            None,
            "session",
        )
        .unwrap();
        assert!(build_index_envelope(&dir, &disabled_cfg()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_none_when_empty_graph() {
        let dir = fresh_kdir("idx-empty");
        assert!(build_index_envelope(&dir, &enabled_cfg()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_text_none_when_only_uninformative() {
        // Empty graph → no active decision and no open question → text None.
        let g = KnowledgeGraph::default();
        assert!(build_index_text(&g, 15, 15).is_none());
    }

    // ---------------- hint ----------------

    #[test]
    fn hint_none_when_no_pending() {
        let dir = fresh_kdir("hint-none");
        assert!(build_hint_envelope(&dir, "sess-1", &enabled_cfg()).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hint_some_then_cleared() {
        let dir = fresh_kdir("hint-some");
        observer_hint::append(&dir, "sess-1", "Index aktualisiert: Foo").unwrap();

        // First call yields the envelope and consumes (clears) the pending hint.
        let s = build_hint_envelope(&dir, "sess-1", &enabled_cfg()).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("UserPromptSubmit")
        );
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(ctx.contains("Index aktualisiert: Foo"), "hint text missing: {ctx}");
        // Feedback OFF (default) → no systemMessage.
        assert!(v.get("systemMessage").is_none(), "no systemMessage when feedback off");

        // Consumed → second call returns None (cleared).
        assert!(
            build_hint_envelope(&dir, "sess-1", &enabled_cfg()).is_none(),
            "hint should be cleared after take"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- user-visible feedback ----------------

    fn feedback_cfg() -> Config {
        let mut c = Config::default();
        c.feedback = true;
        c
    }

    #[test]
    fn compose_hint_message_variants() {
        // Neither part → empty.
        assert_eq!(compose_hint_system_message(0, false), "");
        // Write-delta only.
        assert_eq!(compose_hint_system_message(3, false), "✍️ 3 recorded");
        // Hint only.
        assert_eq!(compose_hint_system_message(0, true), "💡 hint");
        // Both, joined by " · ".
        assert_eq!(compose_hint_system_message(2, true), "✍️ 2 recorded · 💡 hint");
        // Negative/zero write ops never reported.
        assert_eq!(compose_hint_system_message(-5, false), "");
    }

    #[test]
    fn index_feedback_adds_system_message() {
        let dir = fresh_kdir("idx-fb");
        knowledge_store::add_node(
            &dir,
            NodeType::Decision,
            "Wir nutzen Tauri 2".into(),
            "damit wir eine Desktop-App haben",
            None,
            "session",
        )
        .unwrap();
        let s = build_index_envelope(&dir, &feedback_cfg()).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        let sm = v["systemMessage"].as_str().expect("systemMessage present");
        assert!(sm.contains("📖 Knowledge:"), "read marker missing: {sm}");
        assert!(sm.contains("1 decision(s)"), "decision count wrong: {sm}");
        assert!(sm.contains("0 open question(s)"), "question count wrong: {sm}");
        // additionalContext unchanged.
        assert!(v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Wir nutzen Tauri 2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hint_feedback_first_turn_seeds_no_report_then_reports() {
        let dir = fresh_kdir("hint-fb-seed");
        let sess = "sess-fb";

        // First turn: marker is 0 → baseline seeded, NO systemMessage even if ops exist.
        crate::store::observer_stats::record_run(&dir, 4, true, 45);
        assert!(
            build_hint_envelope(&dir, sess, &feedback_cfg()).is_none(),
            "first turn: no hint + baseline seed → nothing emitted"
        );
        let st = observer_hint::load_state(&dir, sess);
        assert!(st.last_feedback_at > 0, "marker seeded to now on first turn");

        // Force the marker into the past so a subsequent ok-run counts as "since".
        let mut st2 = observer_hint::load_state(&dir, sess);
        st2.last_feedback_at = 1; // far in the past
        observer_hint::save_state(&dir, sess, &st2).unwrap();

        // Second turn: an ok run recorded after the marker → write-delta reported.
        crate::store::observer_stats::record_run(&dir, 6, true, 45);
        let s = build_hint_envelope(&dir, sess, &feedback_cfg()).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        let sm = v["systemMessage"].as_str().expect("systemMessage present");
        assert!(sm.contains("✍️"), "write marker missing: {sm}");
        assert!(sm.contains("recorded"), "write text missing: {sm}");
        // No pending hint → no additionalContext, systemMessage stands alone.
        assert!(v.get("hookSpecificOutput").is_none(), "no hint context this turn");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hint_feedback_with_pending_hint_combines() {
        let dir = fresh_kdir("hint-fb-both");
        let sess = "sess-both";
        // Seed the marker in the past so ops_since sees the run.
        let mut st = observer_hint::load_state(&dir, sess);
        st.last_feedback_at = 1;
        observer_hint::save_state(&dir, sess, &st).unwrap();
        crate::store::observer_stats::record_run(&dir, 2, true, 45);
        observer_hint::append(&dir, sess, "Neues Wissen zu Foo").unwrap();

        let s = build_hint_envelope(&dir, sess, &feedback_cfg()).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        let sm = v["systemMessage"].as_str().expect("systemMessage present");
        assert!(sm.contains("✍️"), "write part missing: {sm}");
        assert!(sm.contains("💡 hint"), "hint part missing: {sm}");
        // The hint itself still reaches the model via additionalContext.
        assert!(v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Neues Wissen zu Foo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- envelope JSON shape ----------------

    #[test]
    fn envelope_roundtrips_text_with_quotes_and_newlines() {
        let text = "Zeile1 mit \"Anführung\"\nZeile2\tTab und \\ Backslash";
        let s = envelope("UserPromptSubmit", text);
        let v: Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("UserPromptSubmit")
        );
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"].as_str(),
            Some(text),
            "text must round-trip verbatim (escaping correct)"
        );
    }

    // ---------------- index caps respected ----------------

    #[test]
    fn index_respects_decision_cap() {
        let dir = fresh_kdir("idx-cap");
        for i in 0..5 {
            knowledge_store::add_node(
                &dir,
                NodeType::Decision,
                format!("Decision {i}"),
                "damit",
                None,
                "session",
            )
            .unwrap();
        }
        let mut cfg = enabled_cfg();
        cfg.max_decisions = 2;
        let s = build_index_envelope(&dir, &cfg).expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        let count = ctx.matches("- Decision").count();
        assert_eq!(count, 2, "decision cap should limit to 2, ctx: {ctx}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
