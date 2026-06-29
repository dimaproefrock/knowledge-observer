//! Persistent per-session Observer-Agent runner.
//!
//! A long-lived knowledge observer that "watches" a coding session: per session
//! the daemon owns ONE Claude conversation (its own `--session-id`) that is
//! resumed each turn via `claude --resume`, so the observer LLM keeps its own
//! context (memory) across turns and can de-duplicate knowledge semantically by
//! itself. Subscription-based (no API key); persistence lives in the observer's
//! own transcript, not in a long-running process.
//!
//! **Verified recipe (live spike 2026-06-18):**
//! - create:  `claude --session-id <observer-sid> --setting-sources user`
//! - resume:  `claude --resume     <observer-sid> --setting-sources user`
//! - prompt piped via **stdin**, NO `-p`; output is JSON (parsed by `contract`).
//! - `--setting-sources user` (or `--safe-mode`) suppresses the project's hooks
//!   so the observer's OWN run never self-observes. **NOT `--bare`** — `--bare`
//!   requires an API key and fails on the subscription ("Not logged in").
//! - NO `--dangerously-skip-permissions` (tool-free prompt → no permission hang).
//! - `CLAUDECODE` / `CLAUDE_CODE_ENTRYPOINT` / `CLAUDE_CODE_SESSION*` are removed
//!   from the child env (already handled by the `ephemeral` runner).

use std::time::Duration;

use crate::ephemeral;

/// Re-seed threshold: the observer conversation grows unbounded as turns are appended, so at
/// this many turns the daemon retires the observer-sid and starts a fresh one seeded from the
/// rolling summary. Tunable.
pub const OBS_MAX_TURNS: u32 = 30;

/// Per-turn timeout for the observer LLM. Generous because the headless CLI cold-starts and the
/// extraction prompt is non-trivial.
pub const OBS_TIMEOUT: Duration = Duration::from_secs(120);

/// Section labels used to keep the assembled stdin text legible to the observer LLM and to the
/// unit tests that assert on structure.
const SEC_PROMPT: &str = "=== TASK ===";
const SEC_DAG: &str = "=== EXISTING GRAPH (excerpt) ===";
const SEC_DELTA: &str = "=== NEW TURNS (delta) ===";
const SEC_SUMMARY: &str = "=== SESSION SO FAR (summary) ===";

/// Run one extraction turn for a session's persistent observer agent.
///
/// Thin wrapper over [`ephemeral::run_resumable_turn`]: `is_first == true` CREATES the observer
/// conversation (`--session-id <observer_sid>`), otherwise it RESUMES it (`--resume`). `input_text`
/// is whatever [`build_create_input`] / [`build_resume_input`] / [`build_reseed_input`] produced.
/// Returns the raw stdout; the caller parses it with `contract::parse`.
pub fn run_extraction(
    working_directory: &str,
    observer_sid: &str,
    is_first: bool,
    input_text: &str,
    timeout: Duration,
) -> Result<String, String> {
    ephemeral::run_resumable_turn(
        working_directory,
        observer_sid,
        !is_first,
        input_text,
        timeout,
    )
}

/// Assemble the stdin for the FIRST turn of an observer conversation: the 3rd-person extraction
/// prompt (pass `contract::EXTRACTION_PROMPT` unless overriding), then the existing-DAG excerpt,
/// then the new delta turns. The observer has no memory yet, so the full prompt must lead.
pub fn build_create_input(prompt: &str, delta_turns: &str, dag_excerpt: &str) -> String {
    format!(
        "{SEC_PROMPT}\n{prompt}\n\n{SEC_DAG}\n{dag}\n\n{SEC_DELTA}\n{delta}\n",
        prompt = prompt,
        dag = dag_excerpt,
        delta = delta_turns,
    )
}

/// Assemble the stdin for a FOLLOW-UP turn (`--resume`): just the new delta plus a small DAG
/// excerpt. The observer already remembers the prompt and the session arc, so we do NOT repeat the
/// full extraction prompt — only the fresh material to extract from.
pub fn build_resume_input(delta_turns: &str, dag_excerpt: &str) -> String {
    format!(
        "{SEC_DAG}\n{dag}\n\n{SEC_DELTA}\n{delta}\n",
        dag = dag_excerpt,
        delta = delta_turns,
    )
}

/// Assemble the stdin for a RE-SEEDED observer (a fresh CREATE after the previous conversation hit
/// [`OBS_MAX_TURNS`]). Like [`build_create_input`] but seeded from the carried-over rolling summary
/// instead of raw delta turns, so the new observer inherits the session arc without the full
/// transcript history.
pub fn build_reseed_input(prompt: &str, rolling_summary: &str, dag_excerpt: &str) -> String {
    format!(
        "{SEC_PROMPT}\n{prompt}\n\n{SEC_SUMMARY}\n{summary}\n\n{SEC_DAG}\n{dag}\n",
        prompt = prompt,
        summary = rolling_summary,
        dag = dag_excerpt,
    )
}

/// Whether the observer conversation has grown enough to retire + re-seed. `false` below the
/// threshold, `true` at or over it.
pub fn should_reseed(turn_count: u32, max_turns: u32) -> bool {
    turn_count >= max_turns
}

/// A fresh observer-session UUID. The daemon normally owns sid assignment; this exists for tests
/// and convenience. Deterministic only in that it is a valid v4 UUID.
pub fn new_observer_sid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::contract;

    #[test]
    fn create_input_contains_prompt_delta_and_excerpt_with_labels() {
        let prompt = contract::EXTRACTION_PROMPT;
        let input = build_create_input(prompt, "USER: hi\nAGENT: hello", "node-1: SQLite chosen");
        // Full prompt leads.
        assert!(input.contains("You are observing"));
        assert!(input.contains(prompt));
        // Delta + excerpt present.
        assert!(input.contains("USER: hi"));
        assert!(input.contains("node-1: SQLite chosen"));
        // Sections are labeled.
        assert!(input.contains(SEC_PROMPT));
        assert!(input.contains(SEC_DAG));
        assert!(input.contains(SEC_DELTA));
    }

    #[test]
    fn resume_input_omits_full_prompt_but_keeps_delta_and_excerpt() {
        let input = build_resume_input("USER: next\nAGENT: ok", "node-2: WAL mode");
        assert!(!input.contains("You are observing"));
        assert!(!input.contains(SEC_PROMPT));
        assert!(input.contains("USER: next"));
        assert!(input.contains("node-2: WAL mode"));
        assert!(input.contains(SEC_DELTA));
        assert!(input.contains(SEC_DAG));
    }

    #[test]
    fn reseed_input_carries_summary_and_prompt() {
        let prompt = contract::EXTRACTION_PROMPT;
        let input = build_reseed_input(prompt, "So far: DB + terminal set up.", "node-3: x");
        assert!(input.contains(prompt));
        assert!(input.contains("You are observing"));
        assert!(input.contains("So far: DB + terminal set up."));
        assert!(input.contains("node-3: x"));
        assert!(input.contains(SEC_SUMMARY));
        assert!(input.contains(SEC_PROMPT));
        // A re-seed leads with the summary, not raw delta turns.
        assert!(!input.contains(SEC_DELTA));
    }

    #[test]
    fn should_reseed_boundary() {
        assert!(!should_reseed(0, OBS_MAX_TURNS));
        assert!(!should_reseed(OBS_MAX_TURNS - 1, OBS_MAX_TURNS));
        assert!(should_reseed(OBS_MAX_TURNS, OBS_MAX_TURNS));
        assert!(should_reseed(OBS_MAX_TURNS + 1, OBS_MAX_TURNS));
    }

    #[test]
    fn new_observer_sid_is_valid_uuid() {
        let sid = new_observer_sid();
        assert!(uuid::Uuid::parse_str(&sid).is_ok());
        assert_ne!(new_observer_sid(), new_observer_sid());
    }

    // --- LIVE tests (real `claude`, subscription) ---------------------------------------------
    // Run: cargo test -- --ignored --nocapture observer_

    /// The `~/.claude/projects/<slug>` dir Claude uses for a session whose cwd is `work_str`.
    #[cfg(test)]
    fn live_subdir(work_str: &str) -> std::path::PathBuf {
        let slug: String = work_str
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
            .collect();
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .expect("home dir");
        std::path::PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(slug)
    }

    /// LIVE: persistent memory across processes. Turn 1 (create) plants a codeword; turn 2
    /// (`--resume`) asks for it back → proves the observer's own context survives between separate
    /// headless processes.
    ///   cargo test -- --ignored --nocapture observer_resume_remembers_live
    #[test]
    #[ignore = "spawns real Claude (subscription); run on demand"]
    fn observer_resume_remembers_live() {
        let sid = new_observer_sid();
        let work = std::env::temp_dir().join(format!("ko-obslive-{sid}"));
        std::fs::create_dir_all(&work).unwrap();
        let work_str = work.to_string_lossy().to_string();

        let t1 = run_extraction(
            &work_str,
            &sid,
            true,
            "Remember the codeword BANANA47 for the rest of this conversation. Reply with just: OK.",
            OBS_TIMEOUT,
        );
        eprintln!("--- create turn -> {t1:?}");
        let r = run_extraction(
            &work_str,
            &sid,
            false,
            "What was the codeword I asked you to remember? Reply with only the codeword.",
            OBS_TIMEOUT,
        );

        let _ = std::fs::remove_dir_all(&work);

        let out = r.expect("resume turn must not error/timeout");
        eprintln!("=== OBSERVER RESUME PROBE ({} bytes) ===\n{out}\n===============", out.len());
        assert!(
            out.contains("BANANA47"),
            "observer should recall the codeword across processes — got: {out:?}"
        );
    }

    /// LIVE: `--setting-sources user` suppresses the project's hooks, so the observer's own run
    /// never fires the project's SessionStart hook (no self-observation). We install a hook that
    /// would create a marker file and assert it stays absent.
    ///
    /// Self-contained: the SessionStart hook is written inline into a tempdir's
    /// `.claude/settings.json` (no external session-start-hook dependency).
    ///   cargo test -- --ignored --nocapture observer_no_self_hooks_live
    #[test]
    #[ignore = "spawns real Claude (subscription); run on demand"]
    fn observer_no_self_hooks_live() {
        let sid = new_observer_sid();
        let work = std::env::temp_dir().join(format!("ko-obshook-{sid}"));
        std::fs::create_dir_all(&work).unwrap();
        let work_str = work.to_string_lossy().to_string();

        // A SessionStart hook that drops a marker file when it fires.
        let claude_dir = work.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let marker = work.join("selfhook.txt");
        let marker_fwd = marker.to_string_lossy().replace('\\', "/");
        let settings = format!(
            r#"{{
  "hooks": {{
    "SessionStart": [
      {{ "matcher": "startup|resume", "hooks": [
        {{ "type": "command", "command": "echo X > \"{marker}\"" }}
      ] }}
    ]
  }}
}}"#,
            marker = marker_fwd
        );
        std::fs::write(claude_dir.join("settings.json"), settings).unwrap();

        let r = run_extraction(
            &work_str,
            &sid,
            true,
            "Reply with just: OK.",
            OBS_TIMEOUT,
        );
        eprintln!("--- observer run -> {r:?}");

        let fired = marker.exists();
        let _ = std::fs::remove_dir_all(&work);

        r.expect("observer run must not error/timeout");
        assert!(
            !fired,
            "project SessionStart hook MUST NOT fire for the observer run (--setting-sources user)"
        );
    }

    /// LIVE: no `--dangerously-skip-permissions` on a tool-free prompt → the run completes within
    /// the timeout (does not hang on a permission prompt) and returns non-empty, parseable-ish
    /// output.
    ///   cargo test -- --ignored --nocapture observer_no_skip_perms_live
    #[test]
    #[ignore = "spawns real Claude (subscription); run on demand"]
    fn observer_no_skip_perms_live() {
        let sid = new_observer_sid();
        let work = std::env::temp_dir().join(format!("ko-obsperm-{sid}"));
        std::fs::create_dir_all(&work).unwrap();
        let work_str = work.to_string_lossy().to_string();

        let start = std::time::Instant::now();
        let r = run_extraction(
            &work_str,
            &sid,
            true,
            "Return ONLY this JSON object and nothing else: {\"ops\": [], \"rolling_summary\": \"ok\"}",
            OBS_TIMEOUT,
        );
        let elapsed = start.elapsed();
        // Touch live_subdir so it is exercised (and to locate the transcript dir if debugging).
        let _ = live_subdir(&work_str);

        let _ = std::fs::remove_dir_all(&work);

        let out = r.expect("tool-free run without skip-permissions must not hang/error");
        eprintln!("=== NO-SKIP-PERMS OUTPUT ({} bytes, {:?}) ===\n{out}\n===============", out.len(), elapsed);
        assert!(!out.trim().is_empty(), "output should be non-empty");
        assert!(
            elapsed < OBS_TIMEOUT,
            "run should finish well within the timeout, took {elapsed:?}"
        );
        // Parseable-ish: contract::parse never panics and should pull our rolling_summary.
        let parsed = contract::parse(&out);
        eprintln!("parsed rolling_summary = {:?}", parsed.rolling_summary);
    }
}
