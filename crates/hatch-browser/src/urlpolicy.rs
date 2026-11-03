//! Which destinations the browser may load.
//!
//! Applied to the first navigation, every redirect hop and every sub-request
//! the page makes while hatch is attached (CDP `Fetch` interception), and to
//! the final URL of every snapshot.

use crate::error::{BResult, BrowserError};
use hatch_core::netguard::{host_blocked, ip_blocked, ip_is_loopback, parse_http_url};
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Mutex;

pub struct UrlPolicy {
    pub allow_loopback: bool,
    pub blocklist: Vec<String>,
    /// A proxy resolves names for us: when local DNS fails, hostname rules alone apply.
    pub proxied: bool,
    dns: Mutex<HashMap<String, Result<Vec<IpAddr>, ()>>>,
}

fn pattern_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase();
    !p.is_empty() && (host == p || host.ends_with(&format!(".{p}")))
}

/// Request kinds the interceptor sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// Top-level or frame navigation (stricter: http/https only).
    Navigation,
    /// Images, scripts, XHR, …
    Subresource,
}

impl UrlPolicy {
    pub fn new(allow_loopback: bool, blocklist: Vec<String>, proxied: bool) -> Self {
        UrlPolicy { allow_loopback, blocklist, proxied, dns: Mutex::new(HashMap::new()) }
    }

    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, ()> {
        let mut cache = self.dns.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = cache.get(host) {
            return r.clone();
        }
        let r = (host, port).to_socket_addrs().map(|a| a.map(|s| s.ip()).collect::<Vec<_>>()).map_err(|_| ());
        cache.insert(host.to_string(), r.clone());
        r
    }

    fn ip_ok(&self, ip: IpAddr) -> bool {
        !ip_blocked(ip) || (self.allow_loopback && ip_is_loopback(ip))
    }

    /// `Ok(())` when the URL may be loaded.
    pub fn check(&self, url: &str, kind: RequestKind) -> BResult<()> {
        let lower = url.trim().to_ascii_lowercase();
        if lower == "about:blank" || (kind == RequestKind::Subresource && (lower.starts_with("data:") || lower.starts_with("blob:"))) {
            return Ok(());
        }
        let Some(p) = parse_http_url(url) else {
            return Err(BrowserError::PolicyDenied(format!("只允许 http/https 地址：{}", url.chars().take(120).collect::<String>())));
        };
        if p.has_credentials {
            return Err(BrowserError::PolicyDenied("URL 中不允许包含用户名或密码".into()));
        }
        if let Some(b) = self.blocklist.iter().find(|b| pattern_matches(b, &p.host)) {
            return Err(BrowserError::PolicyDenied(format!("{} 在屏蔽列表中（{b}）", p.host)));
        }
        let loopback_name = p.host == "localhost" || p.host.ends_with(".localhost");
        if let Ok(ip) = p.host.parse::<IpAddr>() {
            return if self.ip_ok(ip) { Ok(()) } else { Err(BrowserError::PolicyDenied(format!("不允许访问内网/保留地址 {ip}"))) };
        }
        if host_blocked(&p.host) && !(self.allow_loopback && loopback_name) {
            return Err(BrowserError::PolicyDenied(format!("不允许访问内网主机 {}", p.host)));
        }
        match self.resolve(&p.host, p.port) {
            Ok(ips) if ips.is_empty() => Err(BrowserError::Network(format!("无法解析 {}", p.host))),
            Ok(ips) => match ips.iter().find(|ip| !self.ip_ok(**ip)) {
                Some(bad) => Err(BrowserError::PolicyDenied(format!("{} 解析到内网/保留地址 {bad}", p.host))),
                None => Ok(()),
            },
            // Name resolution is the proxy's job; the hostname rules above already applied.
            Err(()) if self.proxied => Ok(()),
            Err(()) => Err(BrowserError::Network(format!("无法解析 {}", p.host))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_rules() {
        let p = UrlPolicy::new(false, vec!["*.blocked.example".into()], true);
        let nav = RequestKind::Navigation;
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "http://127.0.0.1:8080/",
            "http://10.0.0.1/admin",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://localhost/",
            "https://router.lan/",
            "https://user:pw@example.com/",
            "https://a.blocked.example/",
        ] {
            assert!(p.check(bad, nav).is_err(), "{bad} must be refused");
        }
        assert!(p.check("about:blank", nav).is_ok());
        assert!(p.check("data:text/html,hi", nav).is_err(), "no data: navigations");
        assert!(p.check("data:image/png;base64,AAAA", RequestKind::Subresource).is_ok());
        assert!(p.check("https://93.184.216.34/", nav).is_ok());
    }

    #[test]
    fn loopback_only_when_allowed_and_never_private() {
        let p = UrlPolicy::new(true, vec![], false);
        let nav = RequestKind::Navigation;
        assert!(p.check("http://127.0.0.1:4567/article", nav).is_ok());
        assert!(p.check("http://localhost:4567/", nav).is_ok());
        assert!(p.check("http://10.0.0.1/admin", nav).is_err(), "private stays blocked");
        assert!(p.check("http://169.254.169.254/", nav).is_err());
    }
}
