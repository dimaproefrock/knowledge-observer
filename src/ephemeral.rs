//! Headless one-shot / resumable `claude` runner for the observer.
//! Only the headless runner the observer needs (`run_resumable_turn`) plus its
//! shared helpers.
//!
//! **Mechanism (headless, no interactive TUI):** spawn `claude` with the instruction
//! piped to **stdin** and the result read from **stdout**. Subscription-compatible:
//! no API key, and it does **not** use the `-p` flag.
//!
//! Why not an interactive PTY session: Claude's TUI emits terminal queries (cursor
//! position, `ESC[6n`, …) and blocks until a terminal answers them. A hidden
//! background session has no terminal emulator to respond, so the TUI hangs before
//! rendering — verified 2026-06-03. Headless stdin/stdout sidesteps this entirely.
//!
//! Failure is always safe: on timeout / non-zero exit / empty output we return `Err`
//! and the caller aborts without touching the real transcript.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// GUI/test processes may not inherit the shell PATH that holds the npm-global `claude`
/// shim, so `cmd /C claude` can't find it. Prepend the likely npm/fnm bin dirs (mirrors
/// `pty_manager`).
#[cfg(target_os = "windows")]
fn apply_npm_path(cmd: &mut Command) {
    if let Ok(current_path) = std::env::var("PATH") {
        let mut extra = Vec::new();
        if let Ok(appdata) = std::env::var("APPDATA") {
            let npm_dir = format!("{appdata}\\npm");
            if !current_path.contains(&npm_dir) {
                extra.push(npm_dir);
            }
        }
        if let Ok(localappdata) = std::env::var("LOCALAPPDATA") {
            let fnm_dir = format!("{localappdata}\\fnm_multishells");
            if !current_path.contains("fnm_multishells") {
                if let Ok(entries) = std::fs::read_dir(&fnm_dir) {
                    if let Some(Ok(entry)) = entries.into_iter().last() {
                        extra.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        if !extra.is_empty() {
            cmd.env("PATH", format!("{};{}", extra.join(";"), current_path));
        }
    }
}
#[cfg(not(target_os = "windows"))]
fn apply_npm_path(_cmd: &mut Command) {}

/// Build the base `claude` `Command` shared by every headless runner: `cmd /C claude` on
/// Windows (npm shim), piped stdin/stdout/stderr, cwd, the nested-session env scrubbed, the
/// extra args + env applied, and the npm PATH fix. The CALLER appends the auth/permission flags
/// (`--dangerously-skip-permissions` vs `--setting-sources user`).
fn base_claude_command(
    working_directory: &str,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> Command {
    // `claude` is an npm shim on Windows → must go through cmd.exe.
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "claude"]);
        c
    } else {
        Command::new("claude")
    };
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(working_directory)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_SESSION");
    for (k, v) in env {
        cmd.env(k, v);
    }
    apply_npm_path(&mut cmd);
    // Suppress the console window: the observer agent runs in the background (under the
    // detached daemon), so `cmd /C claude` must not flash a console.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Run one turn of a resumable headless claude conversation (subscription, no API key,
/// hooks suppressed, tool-free, no `-p`). `session_id` is the dictated observer session
/// UUID; `resume=false` creates it (`--session-id`), `resume=true` continues it (`--resume`).
/// Prompt piped via stdin; stdout returned. Mirrors a normal headless run (cmd/C, env_remove of
/// nested-session vars, apply_npm_path, timeout-kill) but WITHOUT skip-permissions and WITH
/// `--setting-sources user`.
///
/// `--setting-sources user` suppresses the project's `.claude/settings.json` hooks so the
/// observer's own run never self-observes, while keeping the subscription auth (unlike `--bare`,
/// which demands an API key). The prompt is tool-free, so no `--dangerously-skip-permissions` is
/// needed and the run cannot hang on a permission prompt. Verified live 2026-06-18.
pub(crate) fn run_resumable_turn(
    working_directory: &str,
    session_id: &str,
    resume: bool,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    let flag = if resume { "--resume" } else { "--session-id" };
    let args = [flag, session_id, "--setting-sources", "user"];
    let mut cmd = base_claude_command(working_directory, &args, &[]);
    let child = cmd.spawn().map_err(|e| format!("spawn claude: {e}"))?;
    pump_and_collect(child, prompt, timeout)
}

/// Pipe `prompt` to the child's stdin, drain stdout/stderr on threads, poll for exit under
/// `timeout` (kill on overrun), and return stdout on success. Shared tail of every runner.
fn pump_and_collect(
    mut child: std::process::Child,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    // Hand over the instruction, then close stdin so Claude processes and exits.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.write_all(b"\n");
    }

    // Drain stdout on a thread so the pipe never blocks the child.
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout handle".to_string())?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        })
    });

    // Poll for exit, enforcing the timeout.
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = reader.join();
                    return Err("ephemeral Claude timed out".to_string());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                let _ = reader.join();
                return Err(format!("wait on claude: {e}"));
            }
        }
    };

    let out = reader.join().unwrap_or_default();
    let err = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "claude exited with failure (code {:?}); stderr: {}; stdout: {}",
            status.code(),
            err.trim(),
            out.trim()
        ));
    }
    Ok(out)
}
