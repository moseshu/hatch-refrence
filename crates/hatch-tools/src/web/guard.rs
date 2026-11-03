//! SSRF guard for `web_fetch`: only public https endpoints.

use hatch_core::{Error, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Parsed pieces of an https URL that matter to the guard.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Hostnames refused regardless of what they resolve to.
const BLOCKED_HOSTS: &[&str] = &["localhost", "metadata.google.internal", "metadata", "instance-data"];
const BLOCKED_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".lan", ".home.arpa", ".corp"];

pub fn parse_https(url: &str) -> Result<Target> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| Error::Denied(format!("只允许 https 地址：{url}")))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(Error::Denied("URL 中不允许包含用户名或密码".into()));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, tail) = v6.split_once(']').ok_or_else(|| Error::Invalid(format!("bad URL {url}")))?;
        (h.to_string(), tail.strip_prefix(':').map(|p| p.parse::<u16>()).transpose().map_err(|_| Error::Invalid("bad port".into()))?)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>().map_err(|_| Error::Invalid("bad port".into()))?)),
            None => (authority.to_string(), None),
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(Error::Invalid(format!("URL 没有主机名：{url}")));
    }
    Ok(Target { host, port: port.unwrap_or(443) })
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
        || o[0] >= 240
}

pub fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_blocked(v4);
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80 // link local
                || v6 == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254) // AWS IMDS v6
        }
    }
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
pub fn check_destination(t: &Target, proxied: bool) -> Result<Checked> {
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
