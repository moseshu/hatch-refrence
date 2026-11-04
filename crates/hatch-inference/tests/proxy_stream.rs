//! Proxy streaming, health/method rules and upstream failure, end
//! to end against a scripted mock upstream.

#[path = "proxy/common.rs"]
mod common;
use common::*;

use std::net::SocketAddr;
use std::net::TcpListener;
#[test]
fn sse_stream_bytes_identical_and_usage_extracted() {
    let sse = "event: response.output_text.delta\ndata: {\"type\": \"response.output_text.delta\", \"delta\": \"你好\"}\n\nevent: response.completed\ndata: {\"type\": \"response.completed\", \"response\": {\"usage\": {\"input_tokens\": 42, \"output_tokens\": 7, \"input_tokens_details\": {\"cached_tokens\": 10}, \"output_tokens_details\": {\"reasoning_tokens\": 0}}}}\n\ndata: [DONE]\n\n";
    let mock = MockUpstream::start(vec![MockResponse {
        status: 200,
        content_type: "text/event-stream",
        body: sse.as_bytes().to_vec(),
    }]);
    let proxy = Proxy::start(mock.addr, 3, 60);
    let raw = raw_request(
        proxy.addr,
        &format!(
            "POST /responses HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nAuthorization: Bearer k\r\nContent-Length: {}\r\n\r\n{}",
            RESPONSES_BODY.len(),
            RESPONSES_BODY
        ),
    );
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("Transfer-Encoding: chunked"), "{text}");
    assert_eq!(
        dechunk(&raw),
        sse.as_bytes(),
        "stream payload is byte-identical"
    );
    let entries = wait_entries(&proxy.ledger, 1);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (
            entries[0].prompt_tokens,
            entries[0].completion_tokens,
            entries[0].cached_tokens
        ),
        (42, 7, 10)
    );
    mock.stop();
    proxy.stop();
}

#[test]
fn health_and_method_rules() {
    let mock = MockUpstream::start(vec![]);
    let proxy = Proxy::start(mock.addr, 3, 60);
    let raw = raw_request(proxy.addr, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    let raw = raw_request(proxy.addr, "GET /responses HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 405"),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(
        mock.requests().len(),
        0,
        "health/method checks never touch upstream"
    );
    mock.stop();
    proxy.stop();
}

#[test]
fn transport_failure_after_retries_is_502() {
    // Upstream address with nothing listening.
    let dead: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let proxy = Proxy::start(dead, 2, 60);
    let (status, body) = post_json(proxy.addr, "/responses", "Bearer k", RESPONSES_BODY);
    assert_eq!(status, 502);
    assert!(String::from_utf8(body)
        .unwrap()
        .contains("upstream_unavailable"));
    let entries = wait_entries(&proxy.ledger, 1);
    assert_eq!((entries[0].status, entries[0].ok), (502, false));
    proxy.stop();
}
