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
    match std::env::args().nth(1).as_deref() {
        Some("knowledge-index") => cli::run_index(),
        Some("knowledge-hint") => cli::run_hint(),
        Some("observer-trigger") => observer::daemon::run_trigger_subcommand(),
        Some("observer-daemon") | Some("daemon") => observer::daemon::serve(),
        Some("mcp-serve") => crate::mcp::run_mcp_serve(),
        Some("view") => crate::view::run_view(),
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
