//! Per-client token auth for the product API: scope enforcement,
//! revoke-without-restart, legacy shared token, health/share-link
//! exemptions. Uses the in-process server harness from
//! `server_tests` (real TCP, real registry file on disk).

use super::client_auth::{registry_path, DaemonRegistry};
use super::server_tests::{env, raw, start_with_registry, Env};
use hatch_core::client_tokens::Scope;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn make_token(env: &Env, name: &str, scopes: Vec<Scope>) -> String {
    let path = registry_path(env._dir.path());
    let mut reg = DaemonRegistry::load(&path).unwrap();
    let (_, token) = reg.create(name, scopes).unwrap();
    reg.save(&path).unwrap();
    token
}

fn revoke(env: &Env, name_or_id: &str) {
    let path = registry_path(env._dir.path());
    let mut reg = DaemonRegistry::load(&path).unwrap();
    reg.revoke(name_or_id).unwrap();
    reg.save(&path).unwrap();
}

fn authed_refs(token: &str) -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {token}"))]
}

fn refs<'a>(h: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    h.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn shutdown(flag: &Arc<AtomicBool>) {
    flag.store(true, Ordering::SeqCst);
}

#[test]
fn daemon_tokens_use_their_own_prefix_and_file() {
    let env = env();
    let token = make_token(&env, "web", vec![Scope::Read]);
    assert!(token.starts_with("hatch_daemon_"), "{token}");
    // A token minted for MCP (different prefix) is not accepted here:
    // the registries and the token namespaces are separate.
    let path = registry_path(env._dir.path());
    assert_eq!(path.file_name().unwrap(), "daemon-clients.json");
    let reg = DaemonRegistry::load(&path).unwrap();
    assert!(reg.authenticate(&token).is_some());
    assert!(reg.authenticate("hatch_mcp_0000000000000000000000000000000000000000000000000000000000000000").is_none());
}

#[test]
fn read_token_can_get_but_not_post() {
    let env = env();
    let read = make_token(&env, "reader", vec![Scope::Read]);
    let (addr, flag) = start_with_registry(
        &env,
        None,
        Some(registry_path(env._dir.path())),
        20,
    );
    let auth = authed_refs(&read);
    let (status, _, _) = raw(addr, "GET", "/v1/status", &refs(&auth), "");
    assert_eq!(status, 200);
    let (status, _, _) = raw(addr, "GET", "/v1/sessions", &refs(&auth), "");
    assert_eq!(status, 200);
    let (status, _, body) = raw(addr, "POST", "/v1/sessions", &refs(&auth), r#"{"title":"t"}"#);
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("insufficient_scope"), "{body}");
    // The SSE chat stream is a write too, even though it streams.
    let sse: Vec<(&str, &str)> = vec![
        ("Authorization", auth[0].1.as_str()),
        ("Accept", "text/event-stream"),
    ];
    let (status, _, body) = raw(
        addr,
        "POST",
        "/v1/sessions/ses_x/messages",
        &sse,
        r#"{"content":"hi"}"#,
    );
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("insufficient_scope"), "{body}");
    // Share links skip bearer scope checks (the token in the path is
    // the credential); an unknown share still 404s, not 403s.
    let (status, _, _) = raw(addr, "GET", "/v1/shared/nope", &refs(&auth), "");
    assert_eq!(status, 404);
    shutdown(&flag);
}

#[test]
fn write_token_can_post_and_read_token_gets_401_without_one() {
    let env = env();
    let write = make_token(&env, "writer", vec![Scope::Write]);
    let (addr, flag) = start_with_registry(
        &env,
        None,
        Some(registry_path(env._dir.path())),
        20,
    );
    let auth = authed_refs(&write);
    let (status, _, body) = raw(addr, "POST", "/v1/sessions", &refs(&auth), r#"{"title":"t"}"#);
    assert_eq!(status, 201, "{body}");
    // No token at all: the configured registry means anonymous access
    // is gone, even on loopback.
    let (status, _, body) = raw(addr, "GET", "/v1/status", &[], "");
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("unauthorized"), "{body}");
    shutdown(&flag);
}

#[test]
fn revoked_token_is_rejected_on_the_next_request() {
    let env = env();
    let read = make_token(&env, "temp", vec![Scope::Read]);
    let (addr, flag) = start_with_registry(
        &env,
        None,
        Some(registry_path(env._dir.path())),
        20,
    );
    let auth = authed_refs(&read);
    let (status, _, _) = raw(addr, "GET", "/v1/status", &refs(&auth), "");
    assert_eq!(status, 200);
    // Revoke while the server keeps running: no restart needed.
    revoke(&env, "temp");
    let (status, _, body) = raw(addr, "GET", "/v1/status", &refs(&auth), "");
    assert_eq!(status, 401, "{body}");
    shutdown(&flag);
}

#[test]
fn legacy_shared_token_keeps_full_scope() {
    let env = env();
    let read = make_token(&env, "reader", vec![Scope::Read]);
    let (addr, flag) = start_with_registry(
        &env,
        Some("tok-legacy".into()),
        Some(registry_path(env._dir.path())),
        20,
    );
    // The legacy token still does everything (transitional).
    let legacy = authed_refs("tok-legacy");
    let (status, _, body) = raw(addr, "POST", "/v1/sessions", &refs(&legacy), r#"{"title":"t"}"#);
    assert_eq!(status, 201, "{body}");
    // …while the read client token is still read-only.
    let auth = authed_refs(&read);
    let (status, _, body) = raw(addr, "POST", "/v1/sessions", &refs(&auth), r#"{"title":"t"}"#);
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("insufficient_scope"), "{body}");
    shutdown(&flag);
}

#[test]
fn health_needs_no_token_even_with_a_registry() {
    let env = env();
    let _read = make_token(&env, "reader", vec![Scope::Read]);
    let (addr, flag) = start_with_registry(
        &env,
        None,
        Some(registry_path(env._dir.path())),
        20,
    );
    let (status, _, body) = raw(addr, "GET", "/v1/health", &[], "");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"],
        "ok"
    );
    shutdown(&flag);
}
