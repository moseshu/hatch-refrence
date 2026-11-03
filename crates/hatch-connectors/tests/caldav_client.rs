//! Unit tests for the CalDAV client against a scripted in-memory
//! HTTP fake (no network, no vault needed at this layer).


use chrono_tz::Tz;
use hatch_connectors::caldav::client::{CaldavClient, CaldavHttp, HttpResponse};
use hatch_core::time::parse_rfc3339;
use hatch_core::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Scripted in-memory HTTP: method+url → (status, body).
#[derive(Clone)]
pub struct FakeHttp {
    inner: Arc<Inner>,
}

/// One recorded call: (method, url, headers).
type Call = (String, String, Vec<(String, String)>);

struct Inner {
    routes: Mutex<HashMap<String, (u16, String)>>,
    /// (method, url, headers) per call, in order.
    calls: Mutex<Vec<Call>>,
}

impl Default for FakeHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHttp {
    pub fn new() -> Self {
        FakeHttp {
            inner: Arc::new(Inner {
                routes: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }),
        }
    }
    pub fn route(&self, method: &str, url: &str, status: u16, body: &str) {
        self.inner
            .routes
            .lock()
            .unwrap()
            .insert(format!("{method} {url}"), (status, body.to_string()));
    }
    pub fn call_count(&self, method: &str, url: &str) -> usize {
        self.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, u, _)| m == method && u == url)
            .count()
    }
    /// Headers of the most recent call to `method url`.
    pub fn last_headers(&self, method: &str, url: &str) -> Vec<(String, String)> {
        self.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(m, u, _)| m == method && u == url)
            .map(|(_, _, h)| h.clone())
            .unwrap_or_default()
    }
}

impl CaldavHttp for FakeHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        _body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse> {
        let hdrs: Vec<(String, String)> =
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.inner.calls.lock().unwrap().push((method.to_string(), url.to_string(), hdrs));
        let key = format!("{method} {url}");
        match self.inner.routes.lock().unwrap().get(&key) {
            Some((s, b)) => Ok(HttpResponse { status: *s, body: b.clone() }),
            None => Ok(HttpResponse { status: 404, body: "not found".into() }),
        }
    }
}

const DISCOVER_XML: &str = r#"<?xml version="1.0" ?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response><d:href>/</d:href><d:propstat><d:prop>
<d:resourcetype><d:collection/></d:resourcetype>
  </d:prop></d:propstat></d:response>
  <d:response><d:href>/cal/</d:href><d:propstat><d:prop>
<d:displayname>工作日历</d:displayname>
<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
<d:getctag>ctag-7</d:getctag>
  </d:prop></d:propstat></d:response>
</d:multistatus>"#;

const QUERY_XML: &str = r#"<?xml version="1.0" ?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response><d:href>/cal/u1.ics</d:href><d:propstat><d:prop>
<d:getetag>"e1"</d:getetag>
<c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:u1
DTSTART:20261104T020000Z
DTEND:20261104T030000Z
SUMMARY:周会
END:VEVENT
END:VCALENDAR</c:calendar-data>
  </d:prop></d:propstat></d:response>
</d:multistatus>"#;

fn client(fake: FakeHttp) -> CaldavClient {
    CaldavClient::new("http://127.0.0.1:8090/", Box::new(fake)).unwrap()
}

#[test]
fn discover_finds_calendar_collections() {
    let fake = FakeHttp::new();
    fake.route("PROPFIND", "http://127.0.0.1:8090", 207, DISCOVER_XML);
    let c = client(fake);
    let cals = c.discover().unwrap();
    assert_eq!(cals.len(), 1);
    assert_eq!(cals[0].url, "http://127.0.0.1:8090/cal/");
    assert_eq!(cals[0].display_name, "工作日历");
    assert_eq!(cals[0].ctag, "ctag-7");
}

#[test]
fn fetch_range_parses_events() {
    let fake = FakeHttp::new();
    fake.route("REPORT", "http://127.0.0.1:8090/cal/", 207, QUERY_XML);
    let c = client(fake);
    let tz: Tz = "Asia/Shanghai".parse().unwrap();
    let s = parse_rfc3339("2026-11-01T00:00:00Z").unwrap();
    let e = parse_rfc3339("2026-12-01T00:00:00Z").unwrap();
    let evs = c.fetch_range("http://127.0.0.1:8090/cal/", &s, &e).unwrap();
    assert_eq!(evs.len(), 1);
    let ev = evs[0].to_event(&tz).unwrap().unwrap();
    assert_eq!(ev.uid, "u1");
    assert_eq!(ev.title, "周会");
}

#[test]
fn http_url_rejected_unless_loopback() {
    assert!(CaldavClient::new("http://cal.example.com/", Box::new(FakeHttp::new())).is_err());
    assert!(CaldavClient::new("https://cal.example.com/", Box::new(FakeHttp::new())).is_ok());
    assert!(CaldavClient::new("ftp://x/", Box::new(FakeHttp::new())).is_err());
    assert!(CaldavClient::new("http://127.0.0.1:8090/", Box::new(FakeHttp::new())).is_ok());
}

#[test]
fn auth_error_is_honest_and_5xx_retried_once() {
    let fake = FakeHttp::new();
    fake.route("PROPFIND", "http://127.0.0.1:8090", 401, "unauthorized");
    let c = client(fake.clone());
    let err = c.discover().unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    assert_eq!(fake.call_count("PROPFIND", "http://127.0.0.1:8090"), 1, "no retry on 401");

    let fake = FakeHttp::new();
    fake.route("PROPFIND", "http://127.0.0.1:8090", 500, "boom");
    let c = client(fake.clone());
    let err = c.discover().unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    assert_eq!(fake.call_count("PROPFIND", "http://127.0.0.1:8090"), 2, "exactly one retry on 5xx");
}

#[test]
fn malformed_xml_is_an_error() {
    let fake = FakeHttp::new();
    fake.route("REPORT", "http://127.0.0.1:8090/cal/", 207, "<d:multistatus><d:response>");
    let c = client(fake);
    let s = parse_rfc3339("2026-11-01T00:00:00Z").unwrap();
    let e = parse_rfc3339("2026-12-01T00:00:00Z").unwrap();
    assert!(c.fetch_range("http://127.0.0.1:8090/cal/", &s, &e).is_err());
}

#[test]
fn put_uses_conditional_headers() {
    let fake = FakeHttp::new();
    fake.route("PUT", "http://127.0.0.1:8090/cal/n1.ics", 201, "");
    fake.route("PROPFIND", "http://127.0.0.1:8090/cal/n1.ics", 207,
        "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:propstat><d:prop><d:getetag>\"e9\"</d:getetag></d:prop></d:propstat></d:response></d:multistatus>");
    let c = client(fake.clone());
    let etag = c.put_event("http://127.0.0.1:8090/cal/", "n1", "BEGIN:VCALENDAR", None).unwrap();
    assert_eq!(etag, "\"e9\"");
    let h = fake.last_headers("PUT", "http://127.0.0.1:8090/cal/n1.ics");
    assert!(h.iter().any(|(k, v)| k == "If-None-Match" && v == "*"), "{h:?}");
}
