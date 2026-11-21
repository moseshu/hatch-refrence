//! Network environment for model calls: proxy selection and TLS trust roots.
//!
//! Many real deployments (corporate networks, sandboxes) only reach the
//! internet through an HTTP proxy that may also re-sign TLS with its own CA.
//! We therefore honour the usual variables:
//!
//! * `HTTPS_PROXY` / `https_proxy` (https URLs), `HTTP_PROXY` / `http_proxy`
//!   (http URLs), `ALL_PROXY` / `all_proxy` (either) — `http://host:port`;
//! * `NO_PROXY` / `no_proxy` — comma list of hosts, `.suffix`, `*.suffix`,
//!   `*`, or exact IPs that bypass the proxy;
//! * `HATCH_CA_BUNDLE`, then `SSL_CERT_FILE` — extra PEM roots added on top of
//!   the bundled Mozilla roots.

use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

/// Host part of an `http(s)://host[:port]/...` URL, lower-cased.
pub(crate) fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` matches a `NO_PROXY` list.
pub(crate) fn bypasses(host: &str, no_proxy: &str) -> bool {
    no_proxy.split(',').map(str::trim).filter(|e| !e.is_empty()).any(|entry| {
        let entry = entry.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        let suffix = entry.trim_start_matches('*').trim_start_matches('.');
        host == suffix || host.ends_with(&format!(".{suffix}"))
    })
}

/// Proxy URL to use for `url`, if any.
pub(crate) fn proxy_for(url: &str) -> Option<String> {
    let host = url_host(url)?;
    if let Some(np) = env(&["NO_PROXY", "no_proxy"]) {
        if bypasses(&host, &np) {
            return None;
        }
    }
    let specific = if url.starts_with("https://") {
        env(&["HTTPS_PROXY", "https_proxy"])
    } else {
        env(&["HTTP_PROXY", "http_proxy"])
    };
    specific.or_else(|| env(&["ALL_PROXY", "all_proxy"]))
}

/// Shared TLS configuration: Mozilla roots plus an optional extra CA bundle.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            if let Some(path) = env(&["HATCH_CA_BUNDLE", "SSL_CERT_FILE"]) {
                if let Ok(iter) = CertificateDer::pem_file_iter(&path) {
                    for cert in iter.flatten() {
                        let _ = roots.add(cert);
                    }
                }
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Timeout shape of a cached agent: one overall deadline, or the
/// stream bounds (30 s connect / 300 s idle read) that let a slow
/// generation run long without hanging forever on a dead socket.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentTimeout {
    Overall(Duration),
    Stream,
}

/// Long-lived agents keyed by (proxy, timeout shape, redirects).
/// A `ureq::Agent` owns the connection pool, so rebuilding one per
/// request throws away keep-alive and redoes TCP + TLS handshakes
/// on every turn of a multi-step task. Clones share the pool.
type AgentKey = (Option<String>, AgentTimeout, Option<u32>);
static AGENTS: OnceLock<Mutex<HashMap<AgentKey, ureq::Agent>>> = OnceLock::new();

#[cfg(test)]
fn cached_count_for(timeout: AgentTimeout) -> usize {
    let cache = AGENTS.get_or_init(|| Mutex::new(HashMap::new()));
    cache.lock().expect("agent cache poisoned").keys().filter(|k| k.1 == timeout).count()
}

pub(crate) fn cached_agent(url: &str, timeout: AgentTimeout, redirects: Option<u32>) -> Result<ureq::Agent, String> {
    let key = (proxy_for(url), timeout, redirects);
    let cache = AGENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().expect("agent cache poisoned");
    if let Some(agent) = map.get(&key) {
        return Ok(agent.clone());
    }
    let mut b = ureq::AgentBuilder::new().tls_config(tls_config());
    b = match timeout {
        AgentTimeout::Overall(t) => b.timeout(t),
        AgentTimeout::Stream => b
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(300)),
    };
    if let Some(r) = redirects {
        b = b.redirects(r);
    }
    if let Some(p) = &key.0 {
        b = b.proxy(ureq::Proxy::new(p).map_err(|e| format!("invalid proxy setting: {e}"))?);
    }
    let agent = b.build();
    map.insert(key, agent.clone());
    Ok(agent)
}

/// A blocking HTTP agent for `url` that honours the proxy and CA settings —
/// for non-model callers such as a web-fetch tool. Redirects are not followed
/// automatically (`redirects` = 0) so the caller can re-check every hop.
pub fn http_agent(url: &str, timeout: Duration, redirects: u32) -> Result<ureq::Agent, String> {
    cached_agent(url, AgentTimeout::Overall(timeout), Some(redirects))
}

/// A blocking HTTP agent for long-lived upstream streams (model SSE):
/// 30 s to connect, 300 s of idle read — the same bounds the model
/// providers use — instead of one overall timeout, which would kill a
/// legitimate slow generation. Used by the inference proxy to forward
/// model calls.
pub fn stream_agent(url: &str) -> Result<ureq::Agent, String> {
    cached_agent(url, AgentTimeout::Stream, None)
}

/// Whether requests to `url` go through a configured proxy.
pub fn uses_proxy(url: &str) -> bool {
    proxy_for(url).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_reuses_one_cached_agent() {
        // A timeout no other test uses, so parallel tests cannot
        // pollute the count.
        let t = AgentTimeout::Overall(Duration::from_secs(12345));
        let url = "http://127.0.0.1:9/x";
        assert_eq!(cached_count_for(t), 0);
        let _a1 = cached_agent(url, t, Some(0)).unwrap();
        assert_eq!(cached_count_for(t), 1);
        let _a2 = cached_agent(url, t, Some(0)).unwrap();
        assert_eq!(cached_count_for(t), 1, "second call must reuse, not rebuild");
        // A different redirect policy is a different agent.
        let _a3 = cached_agent(url, t, Some(5)).unwrap();
        assert_eq!(cached_count_for(t), 2);
    }

    #[test]
    fn host_extraction() {
        assert_eq!(url_host("https://api.example.com/v1/chat").as_deref(), Some("api.example.com"));
        assert_eq!(url_host("http://User:pw@Host.Example:8080/x").as_deref(), Some("host.example"));
        assert_eq!(url_host("http://[::1]:9000/").as_deref(), Some("::1"));
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn no_proxy_matching() {
        let np = "localhost,127.0.0.1, .internal.example ,*.svc.local,api.anthropic.com";
        assert!(bypasses("localhost", np));
        assert!(bypasses("127.0.0.1", np));
        assert!(bypasses("a.internal.example", np));
        assert!(bypasses("internal.example", np));
        assert!(bypasses("x.svc.local", np));
        assert!(!bypasses("api.deepseek.com", np));
        assert!(!bypasses("evilanthropic.com", np));
        assert!(bypasses("anything", "*"));
    }
}
