//! CalDAV client (RFC 4791 + RFC 6578 sync-collection).
//!
//! HTTP goes through the vault helper ([`VaultHttp`]): the token value
//! lives in the helper process and is only ever a `Bearer` header
//! there — this crate never holds it, logs it, or stores it. The
//! client speaks PROPFIND / REPORT / PUT / DELETE and parses DAV XML
//! with the hand-rolled reader in [`crate::caldav::xml`]. Retry policy:
//! at most **one** retry, only on transient 5xx (500/502/503/504);
//! 4xx, auth and transport errors fail immediately — never a storm.

use super::response::{join_url, parse_multistatus};
use super::xml::{parse_xml, XmlNode};
use crate::calendar::ics::parse_ics;
use crate::calendar::Event;
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_vault::VaultClient;

/// One HTTP round trip.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// How the client performs HTTP. The production implementation is
/// [`VaultHttp`]; tests use a scripted in-memory fake.
pub trait CaldavHttp: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse>;
}

/// HTTP through the vault helper: the token is named, never valued.
pub struct VaultHttp {
    client: VaultClient,
    token_ref: String,
}

impl VaultHttp {
    pub fn new(client: VaultClient, token_ref: String) -> Self {
        VaultHttp { client, token_ref }
    }
}

impl CaldavHttp for VaultHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse> {
        let out = self.client.request_raw(&self.token_ref, method, url, body, headers)?;
        Ok(HttpResponse { status: out.status, body: out.body })
    }
}

/// A calendar collection found by discovery.
#[derive(Clone, Debug, PartialEq)]
pub struct CalendarInfo {
    pub url: String,
    pub display_name: String,
    pub ctag: String,
}

/// One event as the server reported it. `ics` is `None` for a deleted
/// event (the multistatus entry carries a 404 status instead).
#[derive(Clone, Debug)]
pub struct ServerEvent {
    pub href: String,
    pub uid: String,
    pub etag: String,
    pub status: u16,
    pub ics: Option<String>,
}

impl ServerEvent {
    /// Parse the embedded .ics into a local event (deleted events
    /// have no .ics and return `None`).
    pub fn to_event(&self, tz: &Tz) -> Result<Option<Event>> {
        let Some(ics) = &self.ics else { return Ok(None) };
        let mut evs = parse_ics(ics, tz, "caldav")?;
        if evs.is_empty() {
            return Err(Error::Invalid(format!(
                "server event {} has calendar-data with no VEVENT",
                self.uid
            )));
        }
        let mut e = evs.remove(0);
        // Trust the href/UID the sync state is keyed on over a stray
        // UID inside the .ics body.
        e.uid = self.uid.clone();
        Ok(Some(e))
    }
}

/// Validate a CalDAV server URL. `https://` is required; plain `http://`
/// is only allowed when `allow_http` is set **and** the host is
/// loopback (127.0.0.1 / localhost / ::1) — the local mock server.
/// Anything else is rejected before any credential is involved.
pub fn validate_server_url(url: &str, allow_http: bool) -> Result<()> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::Config(format!("CalDAV server URL {url:?} must start with https://"))
    })?;
    let host = host_of(rest);
    match scheme {
        "https" => Ok(()),
        "http" if allow_http && matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") => {
            Ok(())
        }
        "http" => Err(Error::Config(
            "CalDAV over plain http:// needs allow_http = true and a loopback host (local mock only)".into(),
        )),
        _ => Err(Error::Config(format!("CalDAV server URL {url:?} must start with https://"))),
    }
}

/// Host part of `host[:port]/path` (or `user@host...`), without brackets.
fn host_of(rest: &str) -> String {
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = hostport.strip_prefix('[').unwrap_or(hostport);
    let host = host.split(']').next().unwrap_or(host);
    host.split(':').next().unwrap_or(host).to_string()
}

/// Is this host loopback (for the local mock server)?
pub fn is_loopback_host(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else { return false };
    matches!(host_of(rest).as_str(), "127.0.0.1" | "localhost" | "::1")
}

fn dav_time(ts: &Timestamp) -> String {
    ts.format("%Y%m%dT%H%M%SZ").to_string()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The CalDAV client. Holds no secret: auth lives in the [`CaldavHttp`]
/// implementation (production: the vault helper).
pub struct CaldavClient {
    base_url: String,
    http: Box<dyn CaldavHttp>,
}

const XML_CT: &str = "application/xml; charset=utf-8";
const ICS_CT: &str = "text/calendar; charset=utf-8";

impl CaldavClient {
    /// `base_url` is validated here as well as in config: a directly
    /// constructed client must not bypass the https rule. Plain http
    /// is accepted only for loopback hosts (the local mock server).
    pub fn new(base_url: &str, http: Box<dyn CaldavHttp>) -> Result<Self> {
        validate_server_url(base_url, is_loopback_host(base_url))?;
        Ok(CaldavClient { base_url: base_url.trim_end_matches('/').to_string(), http })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn call(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse> {
        // At most one retry, only on transient 5xx.
        let mut resp = self.http.request(method, url, body, headers)?;
        if (500..600).contains(&resp.status) {
            resp = self.http.request(method, url, body, headers)?;
        }
        Ok(resp)
    }

    fn check(&self, method: &str, url: &str, resp: &HttpResponse) -> Result<()> {
        match resp.status {
            200..=299 => Ok(()),
            401 => Err(Error::Connector(format!(
                "CalDAV {method} {url} → HTTP 401 未授权：检查 vault token（`hatch vault list`）与服务器用户名"
            ))),
            403 => Err(Error::Connector(format!("CalDAV {method} {url} → HTTP 403 禁止访问"))),
            404 => Err(Error::Connector(format!("CalDAV {method} {url} → HTTP 404 不存在"))),
            s => Err(Error::Connector(format!(
                "CalDAV {method} {url} → HTTP {s}: {}",
                first_line(&resp.body)
            ))),
        }
    }

    /// PROPFIND the base URL (Depth 1), return the calendar collections.
    /// If the first PROPFIND finds no calendar but a
    /// `calendar-home-set`, that home is queried too.
    pub fn discover(&self) -> Result<Vec<CalendarInfo>> {
        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:displayname/><d:resourcetype/><c:calendar-home-set/><d:getctag/></d:prop>
</d:propfind>"#;
        let url = self.base_url.clone();
        let resp = self.call("PROPFIND", &url, Some(body), &[("Content-Type", XML_CT), ("Depth", "1")])?;
        self.check("PROPFIND", &url, &resp)?;
        let root = parse_xml(&resp.body)?;
        let mut cals = calendars_in(&root, &self.base_url);
        if cals.is_empty() {
            for home in root.descendants("calendar-home-set") {
                if let Some(href) = home.descendant("href").map(|h| h.text.clone()) {
                    let home_url = join_url(&self.base_url, &href);
                    let resp2 = self.call(
                        "PROPFIND",
                        &home_url,
                        Some(body),
                        &[("Content-Type", XML_CT), ("Depth", "1")],
                    )?;
                    self.check("PROPFIND", &home_url, &resp2)?;
                    let root2 = parse_xml(&resp2.body)?;
                    cals.extend(calendars_in(&root2, &self.base_url));
                }
            }
        }
        Ok(cals)
    }

    /// REPORT calendar-query: all events in `[start, end)`.
    pub fn fetch_range(
        &self,
        cal_url: &str,
        start: &Timestamp,
        end: &Timestamp,
    ) -> Result<Vec<ServerEvent>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8" ?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{}" end="{}"/></c:comp-filter></c:comp-filter></c:filter>
</c:calendar-query>"#,
            dav_time(start),
            dav_time(end)
        );
        let resp =
            self.call("REPORT", cal_url, Some(&body), &[("Content-Type", XML_CT), ("Depth", "1")])?;
        self.check("REPORT", cal_url, &resp)?;
        let (events, _) = parse_multistatus(&resp.body)?;
        Ok(events)
    }

    /// REPORT sync-collection (RFC 6578). `token = None` is the initial
    /// sync: the server returns everything plus a token.
    pub fn sync_collection(
        &self,
        cal_url: &str,
        token: Option<&str>,
    ) -> Result<(Vec<ServerEvent>, Vec<String>, String)> {
        let token_el = token
            .map(|t| format!("<d:sync-token>{}</d:sync-token>", xml_escape(t)))
            .unwrap_or_default();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8" ?>
<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  {token_el}
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
</d:sync-collection>"#
        );
        let resp =
            self.call("REPORT", cal_url, Some(&body), &[("Content-Type", XML_CT), ("Depth", "1")])?;
        self.check("REPORT", cal_url, &resp)?;
        let (events, new_token) = parse_multistatus(&resp.body)?;
        let deleted: Vec<String> =
            events.iter().filter(|e| e.status == 404).map(|e| e.uid.clone()).collect();
        let changed: Vec<ServerEvent> = events.into_iter().filter(|e| e.status != 404).collect();
        let token = new_token.ok_or_else(|| {
            Error::Invalid("sync-collection 响应里没有 sync-token（服务器不支持 RFC 6578？）".into())
        })?;
        Ok((changed, deleted, token))
    }

    /// PUT an event (create or update). `etag` enables If-Match on
    /// updates; creates use If-None-Match. Returns the server etag,
    /// read back with a PROPFIND (the vault path returns status + body
    /// only, no response headers) — empty when the read-back fails.
    pub fn put_event(
        &self,
        cal_url: &str,
        uid: &str,
        ics: &str,
        etag: Option<&str>,
    ) -> Result<String> {
        let url = format!("{}{}.ics", ensure_slash(cal_url), uid);
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", ICS_CT)];
        if let Some(e) = etag {
            headers.push(("If-Match", e));
        } else {
            headers.push(("If-None-Match", "*"));
        }
        let resp = self.call("PUT", &url, Some(ics), &headers)?;
        match resp.status {
            200 | 201 | 204 => {}
            412 => {
                return Err(Error::Connector(format!(
                    "CalDAV PUT {url} → HTTP 412 前置条件失败：服务器上的版本已变（先同步再重试）"
                )))
            }
            s => {
                self.check("PUT", &url, &resp)?;
                let _ = s;
            }
        }
        Ok(self.event_etag(&url).unwrap_or_default())
    }

    /// DELETE an event by UID. `etag` enables If-Match when known.
    pub fn delete_event(&self, cal_url: &str, uid: &str, etag: Option<&str>) -> Result<()> {
        let url = format!("{}{}.ics", ensure_slash(cal_url), uid);
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(e) = etag {
            headers.push(("If-Match", e));
        }
        let resp = self.call("DELETE", &url, None, &headers)?;
        match resp.status {
            200 | 204 | 404 => Ok(()), // 404 = already gone, same end state
            412 => Err(Error::Connector(format!(
                "CalDAV DELETE {url} → HTTP 412 前置条件失败：服务器上的版本已变（先同步再重试）"
            ))),
            _ => self.check("DELETE", &url, &resp),
        }
    }

    /// Read-only reachability probe for `hatch doctor`: PROPFIND Depth 0.
    /// Returns the HTTP status; never syncs, never writes.
    pub fn probe(&self) -> Result<u16> {
        let url = self.base_url.clone();
        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
        let resp =
            self.call("PROPFIND", &url, Some(body), &[("Content-Type", XML_CT), ("Depth", "0")])?;
        Ok(resp.status)
    }

    /// PROPFIND Depth 0 on one event href → its current etag ("" when
    /// the server does not report one).
    pub fn event_etag(&self, event_url: &str) -> Result<String> {
        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>"#;
        let resp = self.call(
            "PROPFIND",
            event_url,
            Some(body),
            &[("Content-Type", XML_CT), ("Depth", "0")],
        )?;
        if resp.status == 404 {
            return Ok(String::new());
        }
        self.check("PROPFIND", event_url, &resp)?;
        let root = parse_xml(&resp.body)?;
        Ok(root.descendant("getetag").map(|n| n.text.clone()).unwrap_or_default())
    }
}

fn ensure_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

/// Calendar collections inside one multistatus document.
fn calendars_in(root: &XmlNode, base: &str) -> Vec<CalendarInfo> {
    let mut out = Vec::new();
    for resp in root.descendants("response") {
        let Some(href) = resp.child_text("href") else { continue };
        let is_calendar = resp
            .descendant("resourcetype")
            .map(|rt| !rt.descendants("calendar").is_empty())
            .unwrap_or(false);
        if !is_calendar {
            continue;
        }
        out.push(CalendarInfo {
            url: join_url(base, &href),
            display_name: resp.descendant("displayname").map(|n| n.text.clone()).unwrap_or_default(),
            ctag: resp.descendant("getctag").map(|n| n.text.clone()).unwrap_or_default(),
        });
    }
    out
}
