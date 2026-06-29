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

/// Best-effort read+parse of the hook stdin JSON. Claude writes it immediately, so a
/// plain blocking read returns promptly; any error degrades to empty fields (we then
/// fall back to env / current_dir).
fn read_hook_input() -> HookInput {
    let mut buf = String::new();
    // Read to EOF — Claude closes stdin after writing the JSON object.
    let _ = std::io::stdin().read_to_string(&mut buf);
    let val: Value = serde_json::from_str(buf.trim()).unwrap_or(Value::Null);
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
    json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    })
    .to_string()
}

// ===================== index (SessionStart) =====================

/// Build the compact knowledge index text from a graph (in English; the recorded
/// node content stays in whatever language it was recorded). `None` when there is
/// nothing worth injecting (no active decisions AND no open questions). `max_decisions`
/// / `max_questions` cap the listed items (from `Config`).
///
/// Replicates the `current_state` projection in `dispatch`: active decisions
/// (`Decision` with status `active`) and open questions (a `frage` whose leading
/// value's node is NOT `supported`).
fn build_index_text(g: &KnowledgeGraph, max_decisions: usize, max_questions: usize) -> Option<String> {
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
    Some(out)
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
    let text = build_index_text(&graph, cfg.max_decisions, cfg.max_questions)?;
    Some(envelope("SessionStart", &text))
}

// ===================== hint (UserPromptSubmit) =====================

/// Build the UserPromptSubmit hint envelope for a session, consuming (read+clear)
/// the pending hint. `kdir` is the knowledge dir directly. `None` = no pending hint.
pub fn build_hint_envelope(kdir: &Path, session_id: &str) -> Option<String> {
    let text = crate::store::observer_hint::take(kdir, session_id);
    if text.trim().is_empty() {
        return None;
    }
    Some(envelope("UserPromptSubmit", &text))
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
    if let Some(s) = build_hint_envelope(&kdir, &session_id) {
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
        Config::default() // enabled = true by default
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
        assert!(build_hint_envelope(&dir, "sess-1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hint_some_then_cleared() {
        let dir = fresh_kdir("hint-some");
        observer_hint::append(&dir, "sess-1", "Index aktualisiert: Foo").unwrap();

        // First call yields the envelope and consumes (clears) the pending hint.
        let s = build_hint_envelope(&dir, "sess-1").expect("expected Some");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str(),
            Some("UserPromptSubmit")
        );
        let ctx = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(ctx.contains("Index aktualisiert: Foo"), "hint text missing: {ctx}");

        // Consumed → second call returns None (cleared).
        assert!(
            build_hint_envelope(&dir, "sess-1").is_none(),
            "hint should be cleared after take"
        );
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
