#![allow(dead_code)]

//! `observer` — the host-independent Knowledge-Observer plugin binary.
//!
//! Subcommands (no `mcp-serve` / GUI here — this is the standalone plugin binary):
//! - `knowledge-index`  — SessionStart hook: inject the compact knowledge index.
//! - `knowledge-hint`   — UserPromptSubmit hook: deliver the pending observer hint.
//! - `observer-trigger` — Stop hook: fire a fire-and-forget extraction trigger.
//! - `observer-daemon` / `daemon` — the per-project single-writer daemon serve loop.
//! - `mcp-serve` — read-only MCP stdio server: lets the work agent actively pull
//!   knowledge (query/get/current/list_documents) within a session.
//!
//! Each handler resolves its own project dir (hook-stdin cwd / `CLAUDE_PROJECT_DIR` /
//! `OBSERVER_PROJECT_DIR` / cwd) and the knowledge dir via `Config`.

mod cli;
mod config;
mod dispatch;
mod ephemeral;
mod error;
mod ipc;
mod mcp;
mod observer;
mod store;
mod view;

fn main() {
    let sub = std::env::args().nth(1);

    // --- Re-entrancy guard (CRITICAL) ---
    // The observer agent is itself a `claude --resume` process. When the plugin is
    // installed at user scope, that agent's `claude` ALSO has the plugin's hooks active,
    // so without this guard its Stop hook would fire `observer-trigger` → spawn another
    // observer agent → which fires its Stop hook → ... an unbounded self-observation
    // loop, each pass a full subscription `claude` call (burns the usage limit fast).
    // The daemon spawns the agent with KNOWLEDGE_OBSERVER_INTERNAL=1; the agent's hook
    // subprocesses inherit it, so every hook bails here and the loop can never start.
    if hook_should_bail(
        std::env::var_os("KNOWLEDGE_OBSERVER_INTERNAL").is_some(),
        sub.as_deref(),
    ) {
        return;
    }

    match sub.as_deref() {
        Some("knowledge-index") => cli::run_index(),
        Some("knowledge-hint") => cli::run_hint(),
        Some("observer-trigger") => observer::daemon::run_trigger_subcommand(),
        Some("observer-daemon") | Some("daemon") => observer::daemon::serve(),
        Some("mcp-serve") => crate::mcp::run_mcp_serve(),
        Some("view") => crate::view::run_view(std::env::args().nth(2)),
        // Hidden: the detached background server spawned by `view`. Not advertised in
        // the usage string. Arg is the resolved absolute project dir.
        Some("__view-serve") => crate::view::run_view_serve(std::env::args().nth(2)),
        other => {
            eprintln!(
                "observer {}: unknown/missing subcommand {:?}\n\
                 usage: observer <knowledge-index|knowledge-hint|observer-trigger|observer-daemon|mcp-serve|view>",
                env!("CARGO_PKG_VERSION"),
                other
            );
            std::process::exit(2);
        }
    }
}

/// A hook subcommand must do nothing when it runs inside the observer's own agent
/// (`KNOWLEDGE_OBSERVER_INTERNAL` set) — otherwise the observer observes itself in an
/// unbounded loop. Only the three hook entry points are guarded; daemon/mcp/view are not
/// hooks and are never spawned by the agent.
fn hook_should_bail(internal: bool, sub: Option<&str>) -> bool {
    internal
        && matches!(
            sub,
            Some("knowledge-index" | "knowledge-hint" | "observer-trigger")
        )
}

#[cfg(test)]
mod tests {
    use super::hook_should_bail;

    #[test]
    fn hooks_bail_only_inside_the_observer_agent() {
        // inside the agent: every hook bails
        for s in ["knowledge-index", "knowledge-hint", "observer-trigger"] {
            assert!(hook_should_bail(true, Some(s)), "{s} must bail when internal");
        }
        // non-hooks keep running even inside the agent (none are spawned by it anyway)
        for s in ["observer-daemon", "daemon", "mcp-serve", "view", "__view-serve"] {
            assert!(!hook_should_bail(true, Some(s)), "{s} must not bail");
        }
        // normal sessions (not internal): nothing bails
        for s in ["knowledge-index", "knowledge-hint", "observer-trigger"] {
            assert!(!hook_should_bail(false, Some(s)), "{s} must run when not internal");
        }
        assert!(!hook_should_bail(true, None));
    }
}
