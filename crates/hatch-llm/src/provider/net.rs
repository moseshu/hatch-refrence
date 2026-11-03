// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

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
use std::sync::{Arc, OnceLock};

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!bypasses("api.bianxie.ai", np));
        assert!(!bypasses("evilanthropic.com", np));
        assert!(bypasses("anything", "*"));
    }
}
