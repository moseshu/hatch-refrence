//! Retry-after-temperature-400 for the Responses provider, tested against a
//! local stub gateway (no real model, no network beyond loopback).
//!
//! The gateway rejects `temperature` for reasoning models with HTTP 400;
//! the provider must retry once without the parameter instead of failing
//! the call. See `temperature_rejected` in
//! `crates/hatch-llm/src/provider/openai_responses.rs`.

use hatch_llm::provider::OpenAiResponses;
use hatch_llm::stream::Collected;
use hatch_llm::{ChatMessage, ChatModel, ChatRequest};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// Minimal completed Responses payload, as the stub gateway answers.
const COMPLETED: &str = r#"{"model":"gpt-6-luna","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":10,"output_tokens":2}}"#;
/// The exact 400 Bianxie's gateway returned in the 2026-11-04 live eval.
const TEMP_400: &str = r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model. (request id: a55380e0eb9373b3)","type":"rix_api_error","param":"temperature","code":"invalid_request_error"}}"#;

/// A stub gateway on loopback: the first request gets `first`, the
/// second gets `second`; records whether each request body carried a
/// `temperature` key. Returns the port and the recorded flags.
fn stub_gateway(
    first: (u16, &'static str),
    second: (u16, &'static str),
) -> (u16, mpsc::Receiver<Vec<bool>>) {
    // Keep any ambient proxy (this VM's egress proxy) out of the way.
    let prev = std::env::var("NO_PROXY").unwrap_or_default();
    let mut no_proxy = prev.clone();
    if !no_proxy.is_empty() {
        no_proxy.push(',');
    }
    no_proxy.push_str("127.0.0.1");
    std::env::set_var("NO_PROXY", no_proxy);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, payload) in [first, second] {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            seen.push(request_had_temperature(&mut stream));
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
        let _ = tx.send(seen);
    });
    (port, rx)
}

/// Reads one request and reports whether its JSON body had `temperature`.
fn request_had_temperature(stream: &mut TcpStream) -> bool {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return false;
    }
    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    serde_json::from_slice::<Value>(&body)
        .map(|v| v.get("temperature").is_some())
        .unwrap_or(false)
}

fn stub_provider(port: u16) -> OpenAiResponses {
    let mut p = OpenAiResponses::new(
        "test",
        &format!("http://127.0.0.1:{port}/v1"),
        "gpt-6-luna",
        "test-key".into(),
    );
    p.timeout = Duration::from_secs(10);
    p
}

fn temp_request() -> ChatRequest {
    let mut req = ChatRequest::new(vec![ChatMessage::user("hi")]);
    req.temperature = Some(0.0);
    req.max_tokens = Some(4000);
    req
}

#[test]
fn chat_retries_once_without_temperature_after_400() {
    let (port, rx) = stub_gateway((400, TEMP_400), (200, COMPLETED));
    let r = stub_provider(port).chat(&temp_request()).unwrap();
    assert_eq!(r.content.as_deref(), Some("ok"));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(15)).unwrap(),
        vec![true, false],
        "first attempt keeps temperature, retry drops it"
    );
}

#[test]
fn chat_stream_retries_once_without_temperature_after_400() {
    let (port, rx) = stub_gateway((400, TEMP_400), (200, COMPLETED));
    let mut collected = Collected::default();
    let r = stub_provider(port)
        .chat_stream(&temp_request(), &mut collected.sink())
        .unwrap();
    assert_eq!(r.content.as_deref(), Some("ok"));
    assert_eq!(collected.text(), "ok");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(15)).unwrap(),
        vec![true, false],
        "first attempt keeps temperature, retry drops it"
    );
}
