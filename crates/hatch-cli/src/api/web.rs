//! Static hosting for the web client (`web/`).
//!
//! The API server also serves the hand-written web client so the
//! product API can be validated from a browser with no build step:
//! `GET /` → `web/index.html`, `GET /app.js`, `GET /style.css`.
//! Static paths never need the bearer token (the page itself is
//! where the user enters it); API routes (`/v1/...`, `/mcp`) always
//! win over static, and anything else still 404s.
//!
//! The directory is resolved once at daemon start
//! ([`resolve_web_dir`]): `HATCH_WEB_DIR` first, then walking up
//! from the daemon binary (a checkout's `target/debug/hatch` finds
//! `<repo>/web`), then `./web` from the working directory. When
//! nothing is found the static routes answer 404 with an explanation
//! and the API keeps working.

use hatch_mcp::http_proto::{write_json, write_response};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

/// Environment override for the web directory (tests, dev checkouts).
pub const WEB_DIR_ENV: &str = "HATCH_WEB_DIR";

/// Request paths served as static files (plus `/index.html`).
pub fn is_static_path(path: &str) -> bool {
    matches!(path, "/" | "/index.html" | "/app.js" | "/style.css")
}

/// Map a static request path to (file name, content type).
fn static_target(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("index.html", "text/html; charset=utf-8")),
        "/app.js" => Some(("app.js", "text/javascript; charset=utf-8")),
        "/style.css" => Some(("style.css", "text/css; charset=utf-8")),
        _ => None,
    }
}

/// Find the `web/` directory holding `index.html`, or `None`.
pub fn resolve_web_dir() -> Option<PathBuf> {
    // An explicitly set HATCH_WEB_DIR always wins, even if it is
    // wrong: a bad explicit path must fail visibly (per-file 404),
    // not silently serve some other checkout's client.
    if let Ok(dir) = std::env::var(WEB_DIR_ENV) {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..8 {
            let Some(d) = dir else { break };
            if d.join("web").join("index.html").is_file() {
                return Some(d.join("web"));
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    let cwd = std::env::current_dir().ok()?.join("web");
    if cwd.join("index.html").is_file() {
        return Some(cwd);
    }
    None
}

/// Serve a static request. Returns `Some(status)` when `path` is a
/// static path (handled), `None` otherwise (not our route).
pub fn serve_static(
    web_dir: Option<&Path>,
    path: &str,
    stream: &mut TcpStream,
) -> Option<u16> {
    let (file, content_type) = static_target(path)?;
    let Some(dir) = web_dir else {
        let _ = write_json(
            stream,
            404,
            "Not Found",
            &[],
            &serde_json::json!({"error": {"code": "not_found",
                "message": "网页客户端未找到：daemon 启动时没定位到 web/ 目录（设置 HATCH_WEB_DIR 或从仓库根目录启动）。API 不受影响。"}})
            .to_string(),
        );
        return Some(404);
    };
    let full = dir.join(file);
    match std::fs::read(&full) {
        Ok(body) => {
            let _ = write_response(
                stream,
                200,
                "OK",
                &[("Content-Type", content_type)],
                &body,
            );
            Some(200)
        }
        Err(_) => {
            let _ = write_json(
                stream,
                404,
                "Not Found",
                &[],
                &serde_json::json!({"error": {"code": "not_found",
                    "message": format!("网页文件缺失：{}", full.display())}})
                .to_string(),
            );
            Some(404)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    fn write_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    /// Serve one request through a loopback socket pair; returns
    /// (status line, headers, body).
    fn serve_once(web_dir: Option<&Path>, path: &str) -> Option<(u16, String, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = web_dir.map(|p| p.to_path_buf());
        let path = path.to_string();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            serve_static(dir.as_deref(), &path, &mut s)
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        handle.join().unwrap()?;
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let code: u16 = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        Some((code, head.to_string(), body.to_string()))
    }

    #[test]
    fn static_paths_only() {
        assert!(is_static_path("/"));
        assert!(is_static_path("/index.html"));
        assert!(is_static_path("/app.js"));
        assert!(is_static_path("/style.css"));
        assert!(!is_static_path("/v1/health"));
        assert!(!is_static_path("/favicon.ico"));
        assert!(!is_static_path("/app.js/extra"));
    }

    #[test]
    fn serves_files_with_content_type() {
        let dir = write_dir(&[
            ("index.html", "<title>hatch</title>"),
            ("app.js", "const x = 1;"),
            ("style.css", "body {}"),
        ]);
        let (code, head, body) = serve_once(Some(dir.path()), "/").unwrap();
        assert_eq!(code, 200);
        assert!(head.contains("text/html"), "{head}");
        assert!(body.contains("<title>hatch</title>"), "{body}");
        let (code, head, _) = serve_once(Some(dir.path()), "/app.js").unwrap();
        assert_eq!(code, 200);
        assert!(head.contains("text/javascript"), "{head}");
        let (code, head, _) = serve_once(Some(dir.path()), "/style.css").unwrap();
        assert_eq!(code, 200);
        assert!(head.contains("text/css"), "{head}");
    }

    #[test]
    fn missing_dir_and_file_are_404_json() {
        let (code, _, body) = serve_once(None, "/").unwrap();
        assert_eq!(code, 404);
        assert!(body.contains("not_found"), "{body}");
        let dir = write_dir(&[("index.html", "x")]);
        let (code, _, body) = serve_once(Some(dir.path()), "/app.js").unwrap();
        assert_eq!(code, 404);
        assert!(body.contains("not_found"), "{body}");
    }

    #[test]
    fn non_static_path_is_untouched() {
        let dir = write_dir(&[("index.html", "x")]);
        assert_eq!(serve_once(Some(dir.path()), "/v1/health"), None);
    }

    #[test]
    fn env_override_wins() {
        let dir = write_dir(&[("index.html", "x")]);
        std::env::set_var(WEB_DIR_ENV, dir.path());
        let found = resolve_web_dir().unwrap();
        assert_eq!(found, dir.path());
        std::env::remove_var(WEB_DIR_ENV);
    }
}
