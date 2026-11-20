//! Buffered-response error classification, tested against a local stub
//! gateway (no real model, no network beyond loopback).
//!
//! A response body that cannot be read to the end — the connection drops
//! mid-body, chunked framing is truncated — is a network failure and must
//! classify as transient so the router retries it. A body that arrives
//! whole but is not JSON stays permanent. Regression test for the
//! 2026-11-20 WorkArena replan failure ("response is not JSON: Error
//! while decoding chunks" after the gateway connection dropped at
//! 1817 bytes).

use hatch_llm::provider::OpenAiResponses;
use hatch_llm::{ChatMessage, ChatModel, ChatRequest, LlmErrorKind};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Start a one-request stub gateway that answers with `response` verbatim
/// (status line, headers and body exactly as given). Returns the port.
fn stub_gateway(response: &'static str) -> u16 {
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
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            read_request(&mut stream);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            // Dropping the stream closes the connection; for the truncated
            // case that is exactly the failure under test.
        }
    });
    port
}

/// Consume one HTTP request (headers + Content-Length body).
fn read_request(stream: &mut TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
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
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }
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

fn plain_request() -> ChatRequest {
    // No temperature: keeps the provider's temperature-retry path out of
    // these tests.
    ChatRequest::new(vec![ChatMessage::user("hi")])
}

#[test]
fn truncated_chunked_body_is_transient() {
    // One chunk, then the connection closes without the terminating
    // 0-chunk: the body cannot be read to the end.
    let port = stub_gateway(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n{\"partial\":1234\r\n",
    );
    let err = stub_provider(port).chat(&plain_request()).unwrap_err();
    assert_eq!(
        err.kind,
        LlmErrorKind::Transient,
        "mid-body disconnect must be retryable, got: {err}"
    );
}

#[test]
fn whole_non_json_body_is_permanent() {
    let port = stub_gateway(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot json",
    );
    let err = stub_provider(port).chat(&plain_request()).unwrap_err();
    assert_eq!(
        err.kind,
        LlmErrorKind::Permanent,
        "a complete but unparsable body must stay permanent, got: {err}"
    );
    assert!(
        err.message.contains("response is not JSON"),
        "unexpected message: {err}"
    );
}
