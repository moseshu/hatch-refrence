//! Proxy forwarding, retry and rate limiting, end to end against a
//! scripted mock upstream.

#[path = "proxy/common.rs"]
mod common;
use common::*;

#[test]
fn forward_passes_body_replaces_auth_and_records_usage() {
    let mock = MockUpstream::start(vec![MockResponse::json(200, RESPONSES_OK)]);
    let proxy = Proxy::start(mock.addr, 3, 60);
    let (status, body) = post_json(
        proxy.addr,
        "/responses",
        "Bearer client-key",
        RESPONSES_BODY,
    );
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8(body).unwrap(), RESPONSES_OK);
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(
        reqs[0].path, "/v1/responses",
        "upstream base /v1 + request path"
    );
    assert_eq!(
        reqs[0].auth.as_deref(),
        Some("Bearer proxy-key-123"),
        "proxy key replaces the client's"
    );
    assert_eq!(
        String::from_utf8(reqs[0].body.clone()).unwrap(),
        RESPONSES_BODY
    );
    let (entries, skipped) = (wait_entries(&proxy.ledger, 1), 0);
    assert_eq!((entries.len(), skipped), (1, 0));
    let e = &entries[0];
    assert_eq!(e.model, "gpt-6-luna");
    assert_eq!(
        (
            e.prompt_tokens,
            e.completion_tokens,
            e.cached_tokens,
            e.reasoning_tokens
        ),
        (50, 20, 32, 12)
    );
    assert_eq!((e.status, e.ok, e.retries), (200, true, 0));
    mock.stop();
    proxy.stop();
}

#[test]
fn retry_on_429_then_success_and_400_passes_through_once() {
    let mock = MockUpstream::start(vec![
        MockResponse::json(429, "{\"error\":\"slow down\"}"),
        MockResponse::json(429, "{\"error\":\"slow down\"}"),
        MockResponse::json(200, RESPONSES_OK),
    ]);
    let proxy = Proxy::start(mock.addr, 3, 60);
    let (status, _) = post_json(
        proxy.addr,
        "/responses",
        "Bearer client-key",
        RESPONSES_BODY,
    );
    assert_eq!(status, 200);
    assert_eq!(mock.requests().len(), 3, "two 429s retried");
    let entries = wait_entries(&proxy.ledger, 1);
    assert_eq!(entries[0].retries, 2);
    mock.stop();
    proxy.stop();

    // A 400 naming temperature passes through verbatim with exactly
    // one upstream attempt: the provider client owns that retry.
    let temp_400 = "{\"error\":{\"message\":\"Unsupported parameter: 'temperature' is not supported with this model.\",\"type\":\"rix_api_error\"}}";
    let mock = MockUpstream::start(vec![MockResponse::json(400, temp_400)]);
    let proxy = Proxy::start(mock.addr, 3, 60);
    let (status, body) = post_json(
        proxy.addr,
        "/responses",
        "Bearer client-key",
        RESPONSES_BODY,
    );
    assert_eq!(status, 400);
    assert_eq!(String::from_utf8(body).unwrap(), temp_400);
    assert_eq!(
        mock.requests().len(),
        1,
        "400 is never retried by the proxy"
    );
    let entries = wait_entries(&proxy.ledger, 1);
    assert_eq!((entries[0].status, entries[0].ok), (400, false));
    mock.stop();
    proxy.stop();
}

#[test]
fn rate_limit_rejects_over_cap_with_retry_after() {
    let mock = MockUpstream::start(vec![
        MockResponse::json(200, RESPONSES_OK),
        MockResponse::json(200, RESPONSES_OK),
    ]);
    let proxy = Proxy::start(mock.addr, 3, 2);
    let (s1, _) = post_json(proxy.addr, "/responses", "Bearer k", RESPONSES_BODY);
    let (s2, _) = post_json(proxy.addr, "/responses", "Bearer k", RESPONSES_BODY);
    assert_eq!((s1, s2), (200, 200));
    let raw = raw_request(
        proxy.addr,
        &format!(
            "POST /responses HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAuthorization: Bearer k\r\nContent-Length: {}\r\n\r\n{}",
            RESPONSES_BODY.len(),
            RESPONSES_BODY
        ),
    );
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 429"), "{text}");
    assert!(text.contains("Retry-After: 60"), "{text}");
    assert!(text.contains("rate_limited"), "{text}");
    assert_eq!(
        mock.requests().len(),
        2,
        "the rejected request never reached upstream"
    );
    mock.stop();
    proxy.stop();
}
