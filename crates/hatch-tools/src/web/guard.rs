//! SSRF guard for `web_fetch`: only public https endpoints.
//!
//! Test seam: `HATCH_WEB_FETCH_ALLOW_LOCAL=1` lifts the scheme and
//! destination checks for local fixtures (http://127.0.0.1, …) so a
//! drill can serve an unseen page on loopback and prove the ReAct
//! path really reads it. Off by default; never set it in production
//! — it exists only so the local-fixture acceptance test can run
//! without a real site. Credentials in the URL are refused either
//! way.

use hatch_core::{Error, Result};
use std::net::{IpAddr, ToSocketAddrs};

/// Whether the local-fixture seam is on (test only, off by default).
pub fn local_fetch_allowed() -> bool {
    std::env::var("HATCH_WEB_FETCH_ALLOW_LOCAL").map(|v| v == "1").unwrap_or(false)
}

/// Parsed pieces of an https URL that matter to the guard.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

pub use hatch_core::netguard::ip_blocked;
use hatch_core::netguard::{BLOCKED_HOSTS, BLOCKED_SUFFIXES};

/// Parse an https URL (only https is accepted; credentials in the URL are refused).
pub fn parse_https(url: &str) -> Result<Target> {
    let lower = url.trim().to_ascii_lowercase();
    if local_fetch_allowed() && lower.starts_with("http://") {
        // Local fixture: plain http is allowed; the destination
        // check below still applies unless the seam is on.
    } else if !lower.starts_with("https://") {
        return Err(Error::Denied(format!("只允许 https 地址：{url}")));
    }
    let p = hatch_core::netguard::parse_http_url(url).ok_or_else(|| Error::Invalid(format!("bad URL {url}")))?;
    if p.has_credentials {
        return Err(Error::Denied("URL 中不允许包含用户名或密码".into()));
    }
    Ok(Target { host: p.host, port: p.port })
}

/// How the destination was checked.
#[derive(Clone, Debug, PartialEq)]
pub enum Checked {
    /// Every resolved address is public.
    Resolved(Vec<IpAddr>),
    /// Local DNS is unavailable but a proxy will resolve the name; only the
    /// hostname rules applied.
    ViaProxy,
}

/// Refuse private, loopback, link-local and metadata destinations.
/// The local-fixture seam (`HATCH_WEB_FETCH_ALLOW_LOCAL=1`) skips
/// these checks; credentials are refused in `parse_https` either way.
pub fn check_destination(t: &Target, proxied: bool) -> Result<Checked> {
    if local_fetch_allowed() {
        return Ok(Checked::Resolved(vec![]));
    }
    if BLOCKED_HOSTS.contains(&t.host.as_str()) || BLOCKED_SUFFIXES.iter().any(|s| t.host.ends_with(s)) {
        return Err(Error::Denied(format!("不允许访问内网主机 {}", t.host)));
    }
    if let Ok(ip) = t.host.parse::<IpAddr>() {
        if ip_blocked(ip) {
            return Err(Error::Denied(format!("不允许访问内网/保留地址 {ip}")));
        }
        return Ok(Checked::Resolved(vec![ip]));
    }
    match (t.host.as_str(), t.port).to_socket_addrs() {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            if ips.is_empty() {
                return Err(Error::NotFound(format!("无法解析 {}", t.host)));
            }
            if let Some(bad) = ips.iter().find(|ip| ip_blocked(**ip)) {
                return Err(Error::Denied(format!("{} 解析到内网/保留地址 {bad}", t.host)));
            }
            Ok(Checked::Resolved(ips))
        }
        Err(_) if proxied => Ok(Checked::ViaProxy),
        Err(e) => Err(Error::NotFound(format!("无法解析 {}：{e}", t.host))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rules() {
        assert_eq!(parse_https("https://Example.com/a?b").unwrap(), Target { host: "example.com".into(), port: 443 });
        assert_eq!(parse_https("https://[::1]:8443/").unwrap().port, 8443);
        assert!(parse_https("http://example.com").is_err());
        assert!(parse_https("file:///etc/passwd").is_err());
        assert!(parse_https("https://user:pw@example.com/").is_err());
    }

    #[test]
    fn private_destinations_are_refused() {
        for bad in ["https://127.0.0.1/", "https://10.1.2.3/", "https://169.254.169.254/latest", "https://[::1]/", "https://[fd00::1]/", "https://100.64.0.1/", "https://localhost/", "https://db.internal/", "https://[::ffff:192.168.1.1]/"] {
            let t = parse_https(bad).unwrap();
            assert!(check_destination(&t, true).is_err(), "{bad} should be refused");
        }
        let ok = parse_https("https://93.184.216.34/").unwrap();
        assert!(check_destination(&ok, false).is_ok());
    }
}
