//! `observer view` — a read-only, dependency-free browser visualization of a
//! project's knowledge graph.
//!
//! Starts a tiny single-threaded HTTP/1.1 server on a loopback ephemeral port,
//! serves one self-contained HTML page (inline CSS + vanilla JS + SVG, NO
//! external/CDN resources) and a `/graph.json` endpoint that **re-queries** the
//! `.md` store on every request so the page can poll for live updates. It then
//! best-effort opens the system browser. Loopback-only, no auth (it's a local
//! dev viewer, not a hardened server).
//!
//! std only — uses `std::net` for the server (mirroring `ipc.rs`) and
//! `serde_json` (already a dependency) for the graph payload.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

/// The self-contained viewer page (inline style + script, no external refs).
const INDEX_HTML: &str = include_str!("view/index.html");

/// Entry point for the `view` subcommand.
pub fn run_view() {
    let project_dir = resolve_project_dir();
    let cfg = crate::config::Config::resolve(&project_dir);
    let kdir = cfg.knowledge_dir_abs(&project_dir);

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[observer] view: failed to bind loopback listener: {e}");
            std::process::exit(1);
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("[observer] view: failed to read local port: {e}");
            std::process::exit(1);
        }
    };

    let url = format!("http://127.0.0.1:{port}");
    println!("Knowledge viewer: {url}  (Ctrl-C to stop)");
    let _ = std::io::stdout().flush();

    open_browser(&url);

    // Serve loop: one request per connection, `Connection: close`. Single-threaded
    // is fine for a local dev viewer.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = handle_conn(stream, &kdir) {
                    eprintln!("[observer] view: connection error: {e}");
                }
            }
            Err(e) => eprintln!("[observer] view: accept error: {e}"),
        }
    }
}

/// Resolve the project dir: `CLAUDE_PROJECT_DIR` → `OBSERVER_PROJECT_DIR` → cwd.
fn resolve_project_dir() -> PathBuf {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("OBSERVER_PROJECT_DIR").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Handle a single HTTP connection: read the request line, route, respond, close.
fn handle_conn(mut stream: TcpStream, kdir: &std::path::Path) -> std::io::Result<()> {
    // We only need the request line (first line). A small buffer is plenty; we
    // don't parse headers or bodies (read-only GET routes).
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

/// Best-effort open the system browser at `url`. Errors are ignored — the URL is
/// always printed to stdout so the user can open it manually.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `cmd /C start "" <url>` — the empty "" is the window-title arg `start`
        // expects so the URL is treated as the target, not the title.
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
        // The page must not pull any external resource. We check for the markup
        // that would *load* something (src=/href= to a scheme), not for the bare
        // SVG namespace URI "http://www.w3.org/2000/svg" which is a required
        // identifier, not a fetched resource.
        assert!(INDEX_HTML.contains("<html"));
        assert!(!INDEX_HTML.contains("src=\"http"), "no external src= resources");
        assert!(!INDEX_HTML.contains("href=\"http"), "no external href= resources");
        assert!(!INDEX_HTML.contains("//cdn"), "no CDN references");
        assert!(INDEX_HTML.contains("/graph.json"), "page fetches the graph endpoint");
    }

    #[test]
    fn graph_json_serializes_for_a_tempdir_store() {
        // Build a tiny store in a temp dir and confirm query() -> JSON works
        // exactly as the /graph.json route does (no server started).
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
}
