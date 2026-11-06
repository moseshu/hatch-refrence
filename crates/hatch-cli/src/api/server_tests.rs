//! In-process API server tests: harness + health/auth/rate-limit/
//! sessions/memory. Resource tests live in `server_tests_resources`.

use super::server::{ApiServer, ApiServerConfig};
use super::ApiCtx;
use crate::app::App;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;


    pub(crate) struct Env {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) ctx: Arc<ApiCtx>,
    }

    pub(crate) fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        Env {
            ctx: Arc::new(ApiCtx::new(app)),
            _dir: dir,
        }
    }

    pub(crate) fn start(env: &Env, token: Option<String>, rate: u32) -> (SocketAddr, Arc<AtomicBool>) {
        start_with_registry(env, token, None, rate)
    }

    /// Start the server with a per-client token registry (reloaded
    /// from disk on every request).
    pub(crate) fn start_with_registry(
        env: &Env,
        token: Option<String>,
        registry_path: Option<std::path::PathBuf>,
        rate: u32,
    ) -> (SocketAddr, Arc<AtomicBool>) {
        let config = ApiServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token,
            registry_path,
            rate_limit_per_sec: rate,
        };
        let server = ApiServer::bind(config, env.ctx.clone()).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        std::thread::spawn(move || {
            let _ = server.run(&flag);
        });
        // Give the accept loop a moment to start.
        std::thread::sleep(Duration::from_millis(50));
        (addr, shutdown)
    }

    /// One raw HTTP request; returns (status, headers, body).
    pub(crate) fn raw(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, Vec<(String, String)>, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        req.push_str(body);
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let mut lines = head.lines();
        let status: u16 = lines
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        (status, headers, body.to_string())
    }

    pub(crate) fn authed(token: &str) -> Vec<(&'static str, String)> {
        vec![("Authorization", format!("Bearer {token}"))]
    }

    #[test]
    fn health_needs_no_token_and_status_reports_shape() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let (status, _, body) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"], "ok");
        let (status, _, _) = raw(addr, "GET", "/v1/status", &[], "");
        assert_eq!(status, 401, "status requires the token");
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/status", &auth_refs, "");
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model_configured"], false);
        assert_eq!(v["enabled_jobs"], 0);
        assert_eq!(v["pending_approvals"], 0);
        assert_eq!(v["daemon"], "");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn wrong_token_is_401_and_unknown_paths_are_404() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let bad = vec![("Authorization", "Bearer nope")];
        let (status, _, body) = raw(addr, "GET", "/v1/status", &bad, "");
        assert_eq!(status, 401);
        assert!(body.contains("unauthorized"), "{body}");
        let auth = authed("tok-123");
        let auth_refs: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, body) = raw(addr, "GET", "/v1/nope", &auth_refs, "");
        assert_eq!(status, 404);
        assert!(body.contains("not_found"), "{body}");
        let (status, _, _) = raw(addr, "GET", "/other", &auth_refs, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn wrong_method_is_405_and_rate_limit_is_429() {
        let env = env();
        let (addr, shutdown) = start(&env, None, 2);
        let (status, _, body) = raw(addr, "POST", "/v1/health", &[], "");
        assert_eq!(status, 405);
        assert!(body.contains("method_not_allowed"), "{body}");
        // Rate limit: 2/sec; the POST above already used one slot, so
        // the next two health checks exhaust the window.
        let (s1, _, _) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(s1, 200);
        let (s2, headers, body) = raw(addr, "GET", "/v1/health", &[], "");
        assert_eq!(s2, 429);
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("retry-after") && v == "1"));
        assert!(body.contains("rate_limited"), "{body}");
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn sessions_crud_roundtrip() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // Creating without a token is rejected.
        let (status, _, _) = raw(addr, "POST", "/v1/sessions", &[], r#"{"title":"t1"}"#);
        assert_eq!(status, 401);
        let (status, _, body) = raw(addr, "POST", "/v1/sessions", &auth, r#"{"title":"t1"}"#);
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["title"], "t1");
        let (status, _, body) = raw(addr, "GET", "/v1/sessions", &auth, "");
        assert_eq!(status, 200);
        assert!(body.contains(&id), "{body}");
        let (status, _, body) = raw(addr, "GET", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["title"], "t1");
        assert_eq!(v["messages"], serde_json::json!([]));
        let (status, _, body) =
            raw(addr, "POST", &format!("/v1/sessions/{id}/fork"), &auth, "");
        assert_eq!(status, 201, "{body}");
        let fork = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_ne!(fork["id"].as_str().unwrap(), id);
        let (status, _, _) =
            raw(addr, "POST", &format!("/v1/sessions/{id}/rewind"), &auth, "{}");
        assert_eq!(status, 400, "rewind needs to_seq");
        let (status, _, body) = raw(
            addr,
            "POST",
            &format!("/v1/sessions/{id}/rewind"),
            &auth,
            r#"{"to_seq":0}"#,
        );
        assert_eq!(status, 200, "{body}");
        let (status, _, _) = raw(addr, "GET", "/v1/sessions/ses_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "POST", "/v1/sessions/ses_nope/fork", &auth, "");
        assert_eq!(status, 404);
        let (status, _, _) = raw(addr, "DELETE", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 200);
        let (status, _, _) = raw(addr, "GET", &format!("/v1/sessions/{id}"), &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }

    #[test]
    fn memory_search_add_explain_people() {
        let env = env();
        let (addr, shutdown) = start(&env, Some("tok-123".into()), 20);
        let auth = authed("tok-123");
        let auth: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, _, _) = raw(addr, "GET", "/v1/memory/search", &auth, "");
        assert_eq!(status, 400, "search needs q");
        let (status, _, _) =
            raw(addr, "POST", "/v1/memory/claims", &auth, r#"{"subject":"x"}"#);
        assert_eq!(status, 400, "add needs text");
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/memory/claims",
            &auth,
            r#"{"subject":"api-test-subject","text":"the api test claim","tags":["t1"]}"#,
        );
        assert_eq!(status, 201, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["duplicate"], false);
        let (status, _, body) = raw(
            addr,
            "POST",
            "/v1/memory/claims",
            &auth,
            r#"{"subject":"api-test-subject","text":"the api test claim"}"#,
        );
        assert_eq!(status, 201, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["duplicate"],
            true
        );
        let (status, _, body) =
            raw(addr, "GET", "/v1/memory/search?q=api-test-subject", &auth, "");
        assert_eq!(status, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["id"], id);
        let (status, _, body) =
            raw(addr, "GET", &format!("/v1/memory/claims/{id}"), &auth, "");
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["claim"]["id"],
            id
        );
        let (status, _, _) = raw(addr, "GET", "/v1/memory/claims/claim_nope", &auth, "");
        assert_eq!(status, 404);
        let (status, _, body) = raw(addr, "GET", "/v1/memory/people", &auth, "");
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["people"],
            serde_json::json!([])
        );
        let (status, _, _) =
            raw(addr, "GET", "/v1/memory/people/nobody@example.com", &auth, "");
        assert_eq!(status, 404);
        shutdown.store(true, Ordering::SeqCst);
    }
