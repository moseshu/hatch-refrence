//! Chat-turn API tests (moved out of `chat.rs` to keep it under 400 lines).

use super::chat::{cancel_turn, handle_message_stream, run_turn_json};
use super::ApiCtx;
use crate::app::App;
use hatch_agent::SessionStore;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_mcp::http_proto::parse_request;
use serde_json::json;
use std::io::{BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

    fn scripted_ctx(answer: &str) -> (tempfile::TempDir, Arc<ApiCtx>, String) {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        let session =
            SessionStore::new(&app.db).create("test", &hatch_core::time::now()).unwrap();
        let model = Arc::new(ScriptedModel::ok(
            "test",
            vec![ChatResponse::text(answer)],
        ));
        let ctx = Arc::new(
            ApiCtx::new(app).with_router_factory(Arc::new(move || {
                Ok(Router::single(model.clone()))
            })),
        );
        (dir, ctx, session)
    }

    #[test]
    fn json_turn_answers_with_scripted_model() {
        let (_dir, ctx, session) = scripted_ctx("你好，我是测试助手");
        let (status, body) =
            run_turn_json(&ctx, &session, &json!({"text": "你好"})).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["answer"], "你好，我是测试助手");
        assert_eq!(body["stop"], "answered");
        assert_eq!(body["model_calls"], 1);
        // The answer is persisted in the session history.
        let (_, history) = SessionStore::new(&ctx.app.db).active(&session).unwrap();
        assert!(history
            .iter()
            .any(|m| m.message.content.contains("测试助手")));
        assert!(!ctx.running.lock().unwrap().contains_key(&session));
    }

    #[test]
    fn missing_session_is_404_and_missing_text_is_400() {
        let (_dir, ctx, session) = scripted_ctx("x");
        let err = run_turn_json(&ctx, "no-such-session", &json!({"text": "hi"})).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "not_found");
        let err = run_turn_json(&ctx, &session, &json!({})).unwrap_err();
        assert_eq!(err.status, 400);
    }

    #[test]
    fn no_model_is_503() {
        let dir = tempfile::tempdir().unwrap();
        let app = Arc::new(App::open(Some(dir.path())).unwrap());
        let session =
            SessionStore::new(&app.db).create("test", &hatch_core::time::now()).unwrap();
        let ctx = Arc::new(ApiCtx::new(app));
        let err = run_turn_json(&ctx, &session, &json!({"text": "hi"})).unwrap_err();
        assert_eq!(err.status, 503);
        assert_eq!(err.code, "model_not_configured");
    }

    #[test]
    fn cancel_without_running_turn_reports_false() {
        let (_dir, ctx, session) = scripted_ctx("x");
        let (status, body) = cancel_turn(&ctx, &session).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["cancelled"], false);
        let err = cancel_turn(&ctx, "no-such-session").unwrap_err();
        assert_eq!(err.status, 404);
    }

    #[test]
    fn sse_turn_streams_message_and_done() {
        let (_dir, ctx, session) = scripted_ctx("你好，我是测试助手");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ctx2 = ctx.clone();
        let session2 = session.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let req = parse_request(&mut reader).unwrap();
            handle_message_stream(&ctx2, &mut stream, &req, &session2)
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let body = r#"{"text":"你好"}"#;
        let req = format!(
            "POST /v1/sessions/{session}/messages HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        client.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("Content-Type: text/event-stream"), "{text}");
        assert!(text.contains(": connected"), "{text}");
        assert!(text.contains("event: message"), "{text}");
        assert!(text.contains("测试助手"), "{text}");
        assert!(text.contains("event: done"), "{text}");
        assert!(text.contains(r#""stop":"answered""#), "{text}");
        assert_eq!(server.join().unwrap(), 200);
        assert!(!ctx.running.lock().unwrap().contains_key(&session));
    }
