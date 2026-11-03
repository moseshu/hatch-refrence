//! Destination rules shared by every component that reaches the network on
//! the model's behalf (`web_fetch`, the browser): no private, loopback,
//! link-local, metadata or otherwise reserved destinations.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Hostnames refused regardless of what they resolve to.
pub const BLOCKED_HOSTS: &[&str] = &["localhost", "metadata.google.internal", "metadata", "instance-data"];
/// Suffixes of names that only exist on private networks.
pub const BLOCKED_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".lan", ".home.arpa", ".corp"];

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

/// Is this address private, loopback, link-local, metadata or reserved?
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

/// Is this address loopback (127/8, ::1)?
pub fn ip_is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// Hostname-only rule (no DNS): private-network names and literal private IPs.
pub fn host_blocked(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if BLOCKED_HOSTS.contains(&h) || BLOCKED_SUFFIXES.iter().any(|s| h.ends_with(s)) {
        return true;
    }
    h.parse::<IpAddr>().map(ip_blocked).unwrap_or(false)
}

/// The pieces of an `http(s)` URL that destination rules care about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub has_credentials: bool,
}

/// Parse an `http://` or `https://` URL. Other schemes return `None`.
pub fn parse_http_url(url: &str) -> Option<UrlParts> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let has_credentials = authority.contains('@');
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, tail) = v6.split_once(']')?;
        (h.to_string(), tail.strip_prefix(':').map(|p| p.parse::<u16>()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>())),
            None => (hostport.to_string(), None),
        }
    };
    let port = match port {
        Some(Ok(p)) => p,
        Some(Err(_)) => return None,
        None => default_port,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(UrlParts { scheme, host, port, has_credentials })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        for bad in ["127.0.0.1", "10.1.2.3", "169.254.169.254", "::1", "fd00::1", "100.64.0.1", "::ffff:192.168.1.1", "0.0.0.0"] {
            assert!(ip_blocked(bad.parse().unwrap()), "{bad}");
        }
        assert!(!ip_blocked("93.184.216.34".parse().unwrap()));
        assert!(ip_is_loopback("127.0.0.2".parse().unwrap()));
        assert!(!ip_is_loopback("10.0.0.1".parse().unwrap()));
        assert!(host_blocked("db.internal") && host_blocked("LOCALHOST.") && host_blocked("[::1]") && host_blocked("10.0.0.1"));
        assert!(!host_blocked("example.com"));
    }

    #[test]
    fn urls() {
        let p = parse_http_url("HTTP://User:pw@Example.COM:8080/x?y").unwrap();
        assert_eq!((p.scheme.as_str(), p.host.as_str(), p.port, p.has_credentials), ("http", "example.com", 8080, true));
        assert_eq!(parse_http_url("https://[::1]/").unwrap().host, "::1");
        assert_eq!(parse_http_url("https://a.com").unwrap().port, 443);
        assert!(parse_http_url("file:///etc/passwd").is_none());
        assert!(parse_http_url("javascript:alert(1)").is_none());
        assert!(parse_http_url("https://a.com:99999/").is_none());
    }
}
