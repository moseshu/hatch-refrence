//! Web access for tools: a guarded https fetch and a pluggable search backend.

pub mod guard;
pub mod html;

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FetchLimits {
    pub max_bytes: u64,
    pub max_redirects: u32,
    pub timeout_secs: u64,
}

impl Default for FetchLimits {
    fn default() -> Self {
        FetchLimits { max_bytes: 512 * 1024, max_redirects: 5, timeout_secs: 20 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub text: String,
    pub truncated: bool,
    /// Every hop of the redirect chain was checked; `via_proxy` = DNS was left to the proxy.
    pub via_proxy: bool,
}

fn absolutize(base: &str, location: &str) -> String {
    if location.starts_with("https://") || location.starts_with("http://") {
        return location.to_string();
    }
    let origin_end = base[8..].find('/').map(|i| i + 8).unwrap_or(base.len());
    if location.starts_with('/') {
        format!("{}{location}", &base[..origin_end])
    } else {
        let dir_end = base.rfind('/').filter(|i| *i >= origin_end).unwrap_or(origin_end);
        format!("{}/{location}", &base[..dir_end])
    }
}

/// GET an https URL, re-checking the destination on every redirect hop.
pub fn fetch(url: &str, limits: &FetchLimits) -> Result<Fetched> {
    fetch_with(url, limits, None)
}

/// [`fetch`] that stops between redirect hops and while reading the body
/// once `cancel` fires.
pub fn fetch_with(url: &str, limits: &FetchLimits, cancel: Option<&hatch_core::CancelToken>) -> Result<Fetched> {
    let check = || cancel.map(|c| c.check()).unwrap_or(Ok(()));
    let mut current = url.trim().to_string();
    let mut via_proxy = false;
    for _hop in 0..=limits.max_redirects {
        check()?;
        let target = guard::parse_https(&current)?;
        let proxied = hatch_llm::uses_proxy(&current);
        if guard::check_destination(&target, proxied)? == guard::Checked::ViaProxy {
            via_proxy = true;
        }
        let agent = hatch_llm::http_agent(&current, Duration::from_secs(limits.timeout_secs), 0).map_err(Error::Connector)?;
        let resp = match agent.get(&current).set("User-Agent", "hatch-web-fetch/0.1").set("Accept", "text/html,text/plain,application/json;q=0.9,*/*;q=0.5").call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                return Err(Error::Connector(format!("HTTP {code} {}", r.status_text())));
            }
            Err(e) => return Err(Error::Connector(format!("fetch {current}: {e}"))),
        };
        let status = resp.status();
        if (300..400).contains(&status) {
            let loc = resp.header("location").ok_or_else(|| Error::Connector(format!("HTTP {status} without Location")))?;
            current = absolutize(&current, loc);
            continue;
        }
        let content_type = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
        if !(content_type.is_empty() || content_type.contains("text/") || content_type.contains("json") || content_type.contains("xml")) {
            return Err(Error::Invalid(format!("不支持的内容类型 {content_type}")));
        }
        let mut body = Vec::new();
        let mut reader = resp.into_reader().take(limits.max_bytes + 1);
        let mut buf = [0u8; 16 * 1024];
        loop {
            check()?;
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        let truncated = body.len() as u64 > limits.max_bytes;
        body.truncate(limits.max_bytes as usize);
        let raw = String::from_utf8_lossy(&body);
        let text = if content_type.contains("html") || raw.trim_start().starts_with('<') { html::to_text(&raw) } else { raw.into_owned() };
        return Ok(Fetched { final_url: current, status, content_type, text, truncated, via_proxy });
    }
    Err(Error::Connector(format!("超过 {} 次重定向", limits.max_redirects)))
}

/// A search backend. Only registered when the user configured one — hatch
/// never invents search results.
pub trait SearchBackend: Send + Sync {
    fn name(&self) -> String;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// SearXNG-compatible JSON API: `GET {base}/search?q=...&format=json`.
pub struct SearxBackend {
    pub base_url: String,
}

impl SearchBackend for SearxBackend {
    fn name(&self) -> String {
        format!("searxng:{}", self.base_url)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let agent = hatch_llm::http_agent(&url, Duration::from_secs(20), 2).map_err(Error::Connector)?;
        let v: serde_json::Value = agent
            .get(&url)
            .query("q", query)
            .query("format", "json")
            .call()
            .map_err(|e| Error::Connector(format!("search: {e}")))?
            .into_json()?;
        Ok(v["results"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(limit)
                    .map(|r| SearchHit {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str().unwrap_or("").to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_targets_are_absolutised() {
        assert_eq!(absolutize("https://a.com/x/y", "/z"), "https://a.com/z");
        assert_eq!(absolutize("https://a.com/x/y", "z"), "https://a.com/x/z");
        assert_eq!(absolutize("https://a.com", "z"), "https://a.com/z");
        assert_eq!(absolutize("https://a.com/x", "http://b.com/"), "http://b.com/");
    }

    #[test]
    fn plain_http_redirect_target_is_refused_on_the_next_hop() {
        assert!(guard::parse_https(&absolutize("https://a.com/", "http://10.0.0.1/")).is_err());
    }
}
