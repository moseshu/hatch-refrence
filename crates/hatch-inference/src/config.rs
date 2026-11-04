//! Proxy configuration: where to listen, where to forward, how hard
//! to retry, how fast to allow calls, and what a token costs.
//!
//! The proxy is off by default: nothing in the agent changes unless
//! `[inference] proxy_url` is set, and the proxy process only runs
//! when explicitly started (`hatch inference proxy`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Default listen address: loopback only, never a public interface.
pub const DEFAULT_BIND: &str = "127.0.0.1:8199";
/// Environment variable holding the upstream API key, unless the
/// config names another one.
pub const DEFAULT_API_KEY_ENV: &str = "HATCH_LLM_API_KEY";
/// Default retries after the first attempt (3 retries = 4 attempts).
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Base backoff between retries in milliseconds (doubles each time,
/// capped at 8 s).
pub const DEFAULT_BACKOFF_BASE_MS: u64 = 500;
/// Default per-minute request cap (`0` = unlimited).
pub const DEFAULT_RATE_LIMIT_PER_MIN: u32 = 60;
/// Default currency for cost estimates.
pub const DEFAULT_CURRENCY: &str = "CNY";

/// Configuration for [`crate::ProxyServer`].
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    /// Address to listen on. Must be a loopback address.
    pub bind: SocketAddr,
    /// Upstream base URL, e.g. `https://api.bianxie.ai/v1`. The
    /// request path is appended as-is.
    pub upstream_base_url: String,
    /// Upstream API key. When set it replaces the client's
    /// `Authorization` header; when `None` the client's header is
    /// passed through unchanged.
    pub api_key: Option<String>,
    /// Retries after the first attempt (default 3).
    pub max_retries: u32,
    /// Base backoff in milliseconds (default 500, doubles, capped 8 s).
    pub backoff_base_ms: u64,
    /// Requests allowed per minute (default 60; 0 = unlimited).
    pub max_requests_per_min: u32,
    /// Usage ledger file (`<home>/inference/usage.jsonl`).
    pub ledger_path: PathBuf,
    /// Price per million input tokens, in [`ProxyConfig::currency`].
    /// `None` = no estimate.
    pub price_input_per_million: Option<f64>,
    /// Price per million output tokens. `None` = no estimate.
    pub price_output_per_million: Option<f64>,
    /// Currency of the prices (default `CNY`).
    pub currency: String,
}

impl ProxyConfig {
    pub fn new(bind: SocketAddr, upstream_base_url: String, ledger_path: PathBuf) -> Self {
        ProxyConfig {
            bind,
            upstream_base_url,
            api_key: None,
            max_retries: DEFAULT_MAX_RETRIES,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            max_requests_per_min: DEFAULT_RATE_LIMIT_PER_MIN,
            ledger_path,
            price_input_per_million: None,
            price_output_per_million: None,
            currency: DEFAULT_CURRENCY.to_string(),
        }
    }

    /// Refuse anything that is not a safe proxy setup: a non-loopback
    /// bind, a missing or non-HTTP(S) upstream.
    pub fn validate(&self) -> Result<(), String> {
        if !self.bind.ip().is_loopback() {
            return Err(format!(
                "inference proxy 只允许绑定回环地址，拒绝 {}",
                self.bind
            ));
        }
        if !(self.upstream_base_url.starts_with("http://")
            || self.upstream_base_url.starts_with("https://"))
        {
            return Err(format!(
                "upstream_base_url 必须是 http(s) URL：{}",
                self.upstream_base_url
            ));
        }
        Ok(())
    }

    /// Full upstream URL for a request path (and query, when present).
    pub fn upstream_url(&self, path: &str, query: &str) -> String {
        let base = self.upstream_base_url.trim_end_matches('/');
        if query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query}")
        }
    }
}

/// The usage ledger for a home directory: `<home>/inference/usage.jsonl`.
pub fn ledger_path(home: &Path) -> PathBuf {
    home.join("inference").join("usage.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_non_loopback_and_bad_upstream() {
        let ok = ProxyConfig::new(
            "127.0.0.1:8199".parse().unwrap(),
            "https://api.example.com/v1".into(),
            PathBuf::from("/tmp/x.jsonl"),
        );
        assert!(ok.validate().is_ok());
        assert_eq!(
            ok.upstream_url("/responses", "a=1"),
            "https://api.example.com/v1/responses?a=1"
        );
        assert_eq!(
            ok.upstream_url("/responses", ""),
            "https://api.example.com/v1/responses"
        );
        let mut bad = ok.clone();
        bad.bind = "0.0.0.0:8199".parse().unwrap();
        assert!(bad.validate().is_err());
        let mut bad = ok.clone();
        bad.upstream_base_url = "api.example.com/v1".into();
        assert!(bad.validate().is_err());
    }
}
