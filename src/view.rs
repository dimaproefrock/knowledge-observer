//! `observer view` — a read-only, dependency-free browser visualization of a
//! project's knowledge graph.
//!
//! Two entry points:
//!
//! - [`run_view`] (`observer view [PROJECT_DIR]`) — the user/Claude-facing launcher.
//!   It resolves the project dir, reuses a live viewer for that project if one is
//!   already serving (TCP-liveness via the `.viewer` port-file), else spawns a
//!   **detached** background server (`observer __view-serve <PROJECT_DIR>`) that
//!   survives Claude's turn, then opens the browser and exits.
//! - [`run_view_serve`] (`observer __view-serve <PROJECT_DIR>`) — the hidden,
//!   detached child. It binds a loopback ephemeral port, writes the `.viewer`
//!   port-file, and runs the HTTP serve loop with a 30-minute idle shutdown. The
//!   page polls `/graph.json` every few seconds, so an open tab keeps it alive.
//!
//! The server serves one self-contained HTML page (inline CSS + vanilla JS + SVG,
//! NO external/CDN resources) and a `/graph.json` endpoint that **re-queries** the
//! `.md` store on every request so the page can poll for live updates. Loopback-only,
//! no auth (it's a local dev viewer, not a hardened server).
//!
//! std only — uses `std::net` for the server (mirroring `ipc.rs`) and `serde_json`
//! (already a dependency) for the graph payload + the `.viewer` port-file.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The self-contained viewer page (inline style + script, no external refs).
const INDEX_HTML: &str = include_str!("view/index.html");

/// Idle-shutdown window for the detached serve loop. The page polls every ~4s, so an
/// open tab keeps the server alive; once every tab is closed it exits after this long.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(30 * 60);
/// Accept poll cadence — the listener is non-blocking, so we sleep this long between
/// accept attempts to keep the idle-shutdown clock responsive (mirrors the daemon).
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long the launcher polls for the detached child's `.viewer` port-file.
const SPAWN_POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for the child's `.viewer` file.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Connect timeout for the liveness probe against an existing viewer's port.
const LIVENESS_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

// ===================== project-dir resolution =====================

/// Resolve the project dir with this priority:
/// 1. the explicit `PROJECT_DIR` arg (if present + non-empty),
/// 2. env `CLAUDE_PROJECT_DIR`,
/// 3. env `OBSERVER_PROJECT_DIR`,
/// 4. `std::env::current_dir()`.
///
/// Pure given `arg` + the two env reads, so the priority order is unit-testable.
fn resolve_project_dir_from(
    arg: Option<&str>,
    claude_env: Option<&str>,
    observer_env: Option<&str>,
    cwd: Option<PathBuf>,
) -> PathBuf {
    let nonempty = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };

    arg.and_then(nonempty)
        .or_else(|| claude_env.and_then(nonempty))
        .or_else(|| observer_env.and_then(nonempty))
        .map(PathBuf::from)
        .or(cwd)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the project dir from the real environment for the given explicit arg.
fn resolve_project_dir(arg: Option<String>) -> PathBuf {
    resolve_project_dir_from(
        arg.as_deref(),
        std::env::var("CLAUDE_PROJECT_DIR").ok().as_deref(),
        std::env::var("OBSERVER_PROJECT_DIR").ok().as_deref(),
        std::env::current_dir().ok(),
    )
}

/// The knowledge dir for a project, via the per-project [`Config`].
fn knowledge_dir_for(project_dir: &Path) -> PathBuf {
    let cfg = crate::config::Config::resolve(project_dir);
    cfg.knowledge_dir_abs(project_dir)
}

// ===================== .viewer port-file =====================

/// Advertises the running viewer's loopback endpoint so a later `view` for the same
/// project reuses it instead of spawning a second server. Written to
/// `<knowledge_dir>/.viewer`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ViewerFile {
    port: u16,
    pid: u32,
}

fn viewer_file_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join(".viewer")
}

/// Write `.viewer` atomically (temp + rename).
fn write_viewer_file(knowledge_dir: &Path, vf: &ViewerFile) -> std::io::Result<()> {
    std::fs::create_dir_all(knowledge_dir)?;
    let path = viewer_file_path(knowledge_dir);
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let body = serde_json::to_string(vf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Parse `.viewer`. `None` if missing or corrupt (caller treats as "no viewer").
fn read_viewer_file(knowledge_dir: &Path) -> Option<ViewerFile> {
    let body = std::fs::read_to_string(viewer_file_path(knowledge_dir)).ok()?;
    serde_json::from_str::<ViewerFile>(&body).ok()
}

/// Remove `.viewer` (on clean shutdown). Best-effort.
fn remove_viewer_file(knowledge_dir: &Path) {
    let _ = std::fs::remove_file(viewer_file_path(knowledge_dir));
}

/// Liveness probe: can we TCP-connect to `127.0.0.1:<port>`? A live serve loop accepts
/// the connection; a stale port-file's port refuses it.
fn viewer_alive(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, LIVENESS_CONNECT_TIMEOUT).is_ok()
}

// ===================== launcher: `observer view [PROJECT_DIR]` =====================

/// User/Claude-facing entry point. Resolves project_dir + kdir; reuses a live viewer
/// for this project if one exists, else spawns a detached server, then opens the
/// browser and exits.
pub fn run_view(project_dir_arg: Option<String>) {
    let project_dir = resolve_project_dir(project_dir_arg);
    let kdir = knowledge_dir_for(&project_dir);

    // --- Reuse: a live viewer already serves this project → just open the browser. ---
    if let Some(vf) = read_viewer_file(&kdir) {
        if viewer_alive(vf.port) {
            let url = format!("http://127.0.0.1:{}", vf.port);
            println!("Knowledge viewer already running: {url}");
            let _ = std::io::stdout().flush();
            open_browser(&url);
            return;
        }
        // Stale port-file (no live server) — fall through to spawn a fresh one.
    }

    // --- Else: spawn a detached `observer __view-serve <ABS_PROJECT_DIR>`. ---
    let abs_project_dir = std::fs::canonicalize(&project_dir).unwrap_or_else(|_| project_dir.clone());
    if spawn_detached_serve(&abs_project_dir).is_none() {
        eprintln!("[observer] view: failed to spawn the viewer server");
        std::process::exit(1);
    }

    // Poll for the child's `.viewer` file, then open the browser to its port. The CHILD
    // never opens the browser — only this launcher does — so there is no double-open.
    let deadline = Instant::now() + SPAWN_POLL_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(vf) = read_viewer_file(&kdir) {
            if viewer_alive(vf.port) {
                let url = format!("http://127.0.0.1:{}", vf.port);
                println!("Knowledge viewer: {url}");
                let _ = std::io::stdout().flush();
                open_browser(&url);
                return;
            }
        }
        std::thread::sleep(SPAWN_POLL_INTERVAL);
    }

    eprintln!(
        "[observer] view: the viewer server did not advertise a port within {}s — \
         it may still be starting. Re-run /knowledge-observer:view shortly.",
        SPAWN_POLL_TIMEOUT.as_secs()
    );
}

/// Spawn `current_exe() __view-serve <project_dir>` detached, passing the resolved
/// absolute project dir as the arg so the child resolves the SAME project. Returns
/// `Some(())` on a successful spawn (we don't wait — the launcher polls `.viewer`).
///
/// Detach mirrors the daemon (`observer::daemon::spawn_detached_daemon` / `detach`):
/// Windows `CREATE_NO_WINDOW | DETACHED_PROCESS`, Unix `setsid`, null std streams.
fn spawn_detached_serve(project_dir: &Path) -> Option<()> {
    let exe = std::env::current_exe().ok()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__view-serve")
        .arg(project_dir)
        // Don't inherit a nested-session marker into the detached server's env.
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
            eprintln!("[observer] view: detached spawn failed: {e}");
            None
        }
    }
}

/// Windows: no console window for the detached server, and detach from the parent
/// console so it survives Claude's turn.
#[cfg(target_os = "windows")]
fn detach(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

/// Unix: start a new session so the server survives the spawner's exit.
#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and only detaches the child into its own
    // session; no allocation or shared-state mutation in the child pre-exec.
    unsafe {
        cmd.pre_exec(|| {
            extern "C" {
                fn setsid() -> i32;
            }
            let _ = setsid();
            Ok(())
        });
    }
}
#[cfg(not(any(unix, target_os = "windows")))]
fn detach(_cmd: &mut std::process::Command) {}

// ===================== detached server: `observer __view-serve <DIR>` =====================

/// Hidden entry point (the detached child). Resolves project_dir + kdir, binds a
/// loopback ephemeral port, writes `.viewer`, and runs the serve loop with idle
/// shutdown. Does NOT open the browser (the launcher does). On loop exit, removes
/// `.viewer`.
pub fn run_view_serve(project_dir_arg: Option<String>) {
    let project_dir = resolve_project_dir(project_dir_arg);
    let kdir = knowledge_dir_for(&project_dir);

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[observer] __view-serve: failed to bind loopback listener: {e}");
            std::process::exit(1);
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("[observer] __view-serve: failed to read local port: {e}");
            std::process::exit(1);
        }
    };

    let vf = ViewerFile { port, pid: std::process::id() };
    if let Err(e) = write_viewer_file(&kdir, &vf) {
        eprintln!("[observer] __view-serve: .viewer write failed: {e}");
        std::process::exit(1);
    }

    eprintln!("[observer] __view-serve: serving {} on 127.0.0.1:{port}", kdir.display());

    run_serve_loop(listener, &kdir, IDLE_SHUTDOWN);

    remove_viewer_file(&kdir);
    eprintln!("[observer] __view-serve: idle shutdown — .viewer removed");
}

/// The accept loop with idle-shutdown. Non-blocking listener polled on a short cadence
/// so the idle clock stays responsive even with no traffic (mirrors the daemon). Each
/// accepted connection is handled inline (connection-per-request). Returns when idle.
fn run_serve_loop(listener: TcpListener, kdir: &Path, idle: Duration) {
    listener
        .set_nonblocking(true)
        .unwrap_or_else(|e| eprintln!("[observer] __view-serve: set_nonblocking: {e}"));

    let last_activity = Arc::new(AtomicU64::new(now_secs()));
    let idle_secs = idle.as_secs();

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                last_activity.store(now_secs(), Ordering::SeqCst);
                if let Err(e) = handle_conn(stream, kdir) {
                    eprintln!("[observer] view: connection error: {e}");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let last = last_activity.load(Ordering::SeqCst);
                if now_secs().saturating_sub(last) >= idle_secs {
                    return;
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => {
                eprintln!("[observer] view: accept error: {e}");
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Handle a single HTTP connection: read the request line, route, respond, close.
fn handle_conn(mut stream: TcpStream, kdir: &Path) -> std::io::Result<()> {
    // We only need the request line (first line). A small buffer is plenty; we don't
    // parse headers or bodies (read-only GET routes).
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = parse_request_path(&head);

    match path.as_deref() {
        Some("/") | Some("/index.html") => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes())
        }
        Some("/graph.json") => {
            let graph = crate::store::knowledge_store::query(kdir);
            let body = serde_json::to_string(&graph)
                .unwrap_or_else(|_| "{\"nodes\":[],\"edges\":[],\"fragen\":[],\"quellen\":[]}".to_string());
            write_response(&mut stream, "200 OK", "application/json", body.as_bytes())
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain; charset=utf-8", b"Not Found"),
    }
}

/// Extract the path from an HTTP request's first line: `GET <path> HTTP/1.1`.
/// Returns `None` if it's not a recognizable request line.
fn parse_request_path(head: &str) -> Option<String> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    let raw = parts.next()?;
    // Strip any query string (we don't use one, but be robust).
    let path = raw.split('?').next().unwrap_or(raw);
    Some(path.to_string())
}

/// Write a minimal, well-formed HTTP/1.1 response and close the connection.
fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        len = body.len(),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Best-effort open the system browser at `url`. Errors are ignored — the URL is always
/// printed to stdout so the user can open it manually.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `cmd /C start "" <url>` — the empty "" is the window-title arg `start` expects
        // so the URL is treated as the target, not the title.
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::knowledge_store::{self, NodeType};

    #[test]
    fn parse_request_path_basic() {
        assert_eq!(parse_request_path("GET / HTTP/1.1\r\nHost: x\r\n").as_deref(), Some("/"));
        assert_eq!(
            parse_request_path("GET /graph.json HTTP/1.1\r\n").as_deref(),
            Some("/graph.json")
        );
        assert_eq!(
            parse_request_path("GET /index.html?x=1 HTTP/1.1\r\n").as_deref(),
            Some("/index.html")
        );
    }

    #[test]
    fn parse_request_path_rejects_non_get_and_garbage() {
        assert_eq!(parse_request_path("POST / HTTP/1.1\r\n"), None);
        assert_eq!(parse_request_path("garbage"), None);
        assert_eq!(parse_request_path(""), None);
    }

    #[test]
    fn index_html_is_embedded_and_self_contained() {
        // The page must not pull any external resource. We check for the markup that
        // would *load* something (src=/href= to a scheme), not for the bare SVG
        // namespace URI "http://www.w3.org/2000/svg" which is a required identifier,
        // not a fetched resource.
        assert!(INDEX_HTML.contains("<html"));
        assert!(!INDEX_HTML.contains("src=\"http"), "no external src= resources");
        assert!(!INDEX_HTML.contains("href=\"http"), "no external href= resources");
        assert!(!INDEX_HTML.contains("//cdn"), "no CDN references");
        assert!(INDEX_HTML.contains("/graph.json"), "page fetches the graph endpoint");
    }

    #[test]
    fn graph_json_serializes_for_a_tempdir_store() {
        // Build a tiny store in a temp dir and confirm query() -> JSON works exactly as
        // the /graph.json route does (no server started).
        let dir = std::env::temp_dir().join(format!("observer-view-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let n = knowledge_store::add_node(
            &dir,
            NodeType::Fact,
            "Test-Inhalt".to_string(),
            "weil Quelle X",
            None,
            "manual",
        )
        .unwrap();

        let graph = knowledge_store::query(&dir);
        let json = serde_json::to_string(&graph).unwrap();
        assert!(json.contains("Test-Inhalt"));
        assert!(json.contains(&n.id));
        // Top-level shape the page expects.
        assert!(json.contains("\"nodes\""));
        assert!(json.contains("\"edges\""));
        assert!(json.contains("\"fragen\""));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graph_json_empty_store_is_valid() {
        let dir = std::env::temp_dir().join(format!("observer-view-empty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let graph = knowledge_store::query(&dir);
        let json = serde_json::to_string(&graph).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["nodes"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- project-dir resolution priority (pure) ----------------

    #[test]
    fn project_dir_resolution_priority() {
        // 1. Explicit arg wins over everything.
        assert_eq!(
            resolve_project_dir_from(
                Some("C:/arg"),
                Some("C:/claude"),
                Some("C:/observer"),
                Some(PathBuf::from("C:/cwd")),
            ),
            PathBuf::from("C:/arg")
        );
        // Blank arg is skipped → CLAUDE_PROJECT_DIR.
        assert_eq!(
            resolve_project_dir_from(
                Some("   "),
                Some("C:/claude"),
                Some("C:/observer"),
                Some(PathBuf::from("C:/cwd")),
            ),
            PathBuf::from("C:/claude")
        );
        // 2. CLAUDE_PROJECT_DIR over OBSERVER_PROJECT_DIR.
        assert_eq!(
            resolve_project_dir_from(
                None,
                Some("C:/claude"),
                Some("C:/observer"),
                Some(PathBuf::from("C:/cwd")),
            ),
            PathBuf::from("C:/claude")
        );
        // 3. OBSERVER_PROJECT_DIR over cwd.
        assert_eq!(
            resolve_project_dir_from(None, None, Some("C:/observer"), Some(PathBuf::from("C:/cwd"))),
            PathBuf::from("C:/observer")
        );
        // 4. cwd is the last resort.
        assert_eq!(
            resolve_project_dir_from(None, None, None, Some(PathBuf::from("C:/cwd"))),
            PathBuf::from("C:/cwd")
        );
        // Nothing at all → ".".
        assert_eq!(resolve_project_dir_from(None, None, None, None), PathBuf::from("."));
    }

    // ---------------- .viewer port-file round-trip ----------------

    #[test]
    fn viewer_file_roundtrip_is_identical() {
        let dir = std::env::temp_dir().join(format!("observer-viewer-rt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let vf = ViewerFile { port: 54321, pid: 4242 };
        write_viewer_file(&dir, &vf).unwrap();
        assert_eq!(read_viewer_file(&dir).expect("read back"), vf);
        remove_viewer_file(&dir);
        assert!(read_viewer_file(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn viewer_file_missing_or_corrupt_is_none() {
        let dir = std::env::temp_dir().join(format!("observer-viewer-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_viewer_file(&dir).is_none());
        std::fs::write(viewer_file_path(&dir), b"{ not valid json ]]]").unwrap();
        assert!(read_viewer_file(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn viewer_alive_false_for_dead_port() {
        // Bind then drop a listener to obtain a port nothing is serving.
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(!viewer_alive(port), "a closed port must not be reported alive");
    }
}
