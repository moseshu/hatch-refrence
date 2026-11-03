//! Parsing DAV multistatus responses into [`ServerEvent`]s.
//!
//! Only what sync needs is extracted: `href`, `getetag`, `sync-token`,
//! `calendar-data`, per-response `status`. Anything malformed is an
//! error, never silently treated as "no changes".

use super::client::ServerEvent;
use super::xml::parse_xml;
use hatch_core::{Error, Result};

/// Join a possibly relative DAV `href` against a base URL.
pub fn join_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(path) = href.strip_prefix('/') {
        let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
        let host = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{host}/{path}");
    }
    format!("{}{href}", base.strip_suffix('/').unwrap_or(base))
}


/// UID from an event href (`/cal/u1.ics` → `u1`), percent-decoded.
pub fn uid_from_href(href: &str) -> String {
    let last = href.trim_end_matches('/').rsplit('/').next().unwrap_or(href);
    let last = last.strip_suffix(".ics").unwrap_or(last);
    percent_decode(last)
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}


/// Parse a 207 multistatus body into server events + sync-token.
pub fn parse_multistatus(body: &str) -> Result<(Vec<ServerEvent>, Option<String>)> {
    let root = parse_xml(body)?;
    if root.local() != "multistatus" {
        return Err(Error::Invalid(format!(
            "expected <multistatus>, got <{}>",
            root.local()
        )));
    }
    let token = root.child_text("sync-token");
    let mut events = Vec::new();
    for resp in root.descendants("response") {
        let Some(href) = resp.child_text("href") else { continue };
        // Response-level status wins; otherwise the first propstat
        // status (a 404 propstat means "deleted").
        let status = resp
            .child_text("status")
            .or_else(|| resp.descendant("propstat").and_then(|p| p.child_text("status")))
            .and_then(|s| parse_status(&s))
            .unwrap_or(200);
        let etag = resp.descendant("getetag").map(|n| n.text.clone()).unwrap_or_default();
        let ics = resp.descendant("calendar-data").map(|n| n.text.clone());
        events.push(ServerEvent {
            uid: uid_from_href(&href),
            href: href.clone(),
            etag,
            status,
            ics,
        });
    }
    Ok((events, token))
}

/// `HTTP/1.1 404 Not Found` → 404.
fn parse_status(s: &str) -> Option<u16> {
    s.split_whitespace().nth(1)?.parse().ok()
}

