//! In-test CalDAV mock server: a local `TcpListener`, fake events only.
//!
//! No real network, no real credentials: the only accepted token is
//! the obviously fake `test-token-caldav`. Tests mutate the shared
//! state between syncs to simulate another client changing the server.
//! The server records every PUT/DELETE so tests can assert idempotent
//! re-syncs make zero writes.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const FAKE_TOKEN: &str = "test-token-caldav";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    None,
    Unauthorized,
    ServerError,
    MalformedXml,
}

pub struct MockEvent {
    pub ics: String,
    pub seq: u64,
}

pub struct MockState {
    pub events: HashMap<String, MockEvent>,
    pub tombstones: Vec<(String, u64)>,
    pub version: u64,
    /// "PUT <uid>" / "DELETE <uid>", in order.
    pub writes: Vec<String>,
    /// "METHOD <path>" for every request (retry assertions).
    pub calls: Vec<String>,
    pub fail: FailMode,
}

impl MockState {
    pub fn new() -> Self {
        MockState {
            events: HashMap::new(),
            tombstones: Vec::new(),
            version: 0,
            writes: Vec::new(),
            calls: Vec::new(),
            fail: FailMode::None,
        }
    }

    fn bump(&mut self) -> u64 {
        self.version += 1;
        self.version
    }

    pub fn put_server(&mut self, uid: &str, ics: String) {
        let seq = self.bump();
        self.events.insert(uid.to_string(), MockEvent { ics, seq });
        self.tombstones.retain(|(u, _)| u != uid);
    }

    pub fn delete_server(&mut self, uid: &str) {
        if self.events.remove(uid).is_some() {
            let seq = self.bump();
            self.tombstones.push((uid.to_string(), seq));
        }
    }
}

pub struct MockServer {
    pub base_url: String,
    pub state: Arc<Mutex<MockState>>,
}

impl MockServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(MockState::new()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle(stream, &shared);
            }
        });
        MockServer {
            base_url: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    /// Number of PUT/DELETE calls the server has seen.
    pub fn write_count(&self) -> usize {
        self.state.lock().unwrap().writes.len()
    }

    pub fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }
}

/// Minimal .ics for a fake event.
pub fn ics(uid: &str, title: &str, start: &str, end: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261101T000000Z\r\nLAST-MODIFIED:20261101T000000Z\r\nDTSTART:{start}\r\nDTEND:{end}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn respond(stream: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn etag(seq: u64) -> String {
    format!("\"etag-{seq}\"")
}

fn event_response(uid: &str, ev: &MockEvent) -> String {
    format!(
        "<d:response><d:href>/cal/{uid}.ics</d:href><d:propstat><d:prop>\
        <d:getetag>{}</d:getetag>\
        <c:calendar-data>{}</c:calendar-data>\
        </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        etag(ev.seq),
        xml_escape(&ev.ics)
    )
}

fn handle(mut stream: TcpStream, shared: &Arc<Mutex<MockState>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut content_len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_len = v.trim().parse().unwrap_or(0);
            }
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    let mut st = shared.lock().unwrap();
    st.calls.push(format!("{method} {path}"));
    let fail = st.fail;
    if fail == FailMode::Unauthorized {
        return respond(&mut stream, 401, "Unauthorized", "text/plain", "unauthorized");
    }
    if headers.get("authorization").map(|a| a.as_str()) != Some(&format!("Bearer {FAKE_TOKEN}")) {
        return respond(&mut stream, 401, "Unauthorized", "text/plain", "unauthorized");
    }
    if fail == FailMode::ServerError && (method == "REPORT" || method == "PROPFIND") {
        return respond(&mut stream, 500, "Internal Server Error", "text/plain", "boom");
    }
    match (method.as_str(), path.as_str()) {
        ("PROPFIND", "/") => {
            let body = format!(
                r#"<?xml version="1.0" ?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:response><d:href>/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/cal/</d:href><d:propstat><d:prop><d:displayname>Mock 日历</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:getctag>ctag-{}</d:getctag></d:prop></d:propstat></d:response>
</d:multistatus>"#,
                st.version
            );
            respond(&mut stream, 207, "Multi-Status", "application/xml", &body);
        }
        ("PROPFIND", p) if p.starts_with("/cal/") && p.ends_with(".ics") => {
            let uid = p.trim_start_matches("/cal/").trim_end_matches(".ics");
            match st.events.get(uid) {
                Some(ev) => {
                    let body = format!(
                        r#"<?xml version="1.0" ?><d:multistatus xmlns:d="DAV:"><d:response><d:href>{p}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop></d:propstat></d:response></d:multistatus>"#,
                        etag(ev.seq)
                    );
                    respond(&mut stream, 207, "Multi-Status", "application/xml", &body);
                }
                None => respond(&mut stream, 404, "Not Found", "text/plain", "no such event"),
            }
        }
        ("REPORT", "/cal/") => {
            if fail == FailMode::MalformedXml {
                return respond(
                    &mut stream,
                    207,
                    "Multi-Status",
                    "application/xml",
                    "<d:multistatus><d:response>",
                );
            }
            let mut out = String::from(
                r#"<?xml version="1.0" ?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
            );
            if body.contains("sync-collection") {
                let since = token_seq(&body);
                let mut uids: Vec<String> = st.events.keys().cloned().collect();
                uids.sort();
                for uid in uids {
                    let ev = &st.events[&uid];
                    if ev.seq > since {
                        out.push_str(&event_response(&uid, ev));
                    }
                }
                for (uid, seq) in st.tombstones.clone() {
                    if seq > since {
                        out.push_str(&format!(
                            "<d:response><d:href>/cal/{uid}.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
                        ));
                    }
                }
                out.push_str(&format!("<d:sync-token>tok-{}</d:sync-token>", st.version));
            } else {
                // calendar-query: return everything (tests use a wide range).
                let mut uids: Vec<String> = st.events.keys().cloned().collect();
                uids.sort();
                for uid in uids {
                    let ev = &st.events[&uid];
                    out.push_str(&event_response(&uid, ev));
                }
            }
            out.push_str("</d:multistatus>");
            respond(&mut stream, 207, "Multi-Status", "application/xml", &out);
        }
        ("PUT", p) if p.starts_with("/cal/") && p.ends_with(".ics") => {
            let uid = p.trim_start_matches("/cal/").trim_end_matches(".ics").to_string();
            if headers.get("if-none-match").map(|v| v.as_str()) == Some("*")
                && st.events.contains_key(&uid)
            {
                return respond(&mut stream, 412, "Precondition Failed", "text/plain", "exists");
            }
            if let Some(want) = headers.get("if-match") {
                match st.events.get(&uid) {
                    Some(ev) if etag(ev.seq) == *want => {}
                    _ => {
                        return respond(
                            &mut stream,
                            412,
                            "Precondition Failed",
                            "text/plain",
                            "etag mismatch",
                        )
                    }
                }
            }
            let existed = st.events.contains_key(&uid);
            let seq = st.bump();
            st.events.insert(uid.clone(), MockEvent { ics: body, seq });
            st.tombstones.retain(|(u, _)| *u != uid);
            st.writes.push(format!("PUT {uid}"));
            let code = if existed { 204 } else { 201 };
            respond(&mut stream, code, if existed { "No Content" } else { "Created" }, "text/plain", "");
        }
        ("DELETE", p) if p.starts_with("/cal/") && p.ends_with(".ics") => {
            let uid = p.trim_start_matches("/cal/").trim_end_matches(".ics").to_string();
            if let Some(want) = headers.get("if-match") {
                match st.events.get(&uid) {
                    Some(ev) if etag(ev.seq) == *want => {}
                    _ => {
                        return respond(
                            &mut stream,
                            412,
                            "Precondition Failed",
                            "text/plain",
                            "etag mismatch",
                        )
                    }
                }
            }
            if st.events.remove(&uid).is_some() {
                let seq = st.bump();
                st.tombstones.push((uid.clone(), seq));
                st.writes.push(format!("DELETE {uid}"));
                respond(&mut stream, 204, "No Content", "text/plain", "");
            } else {
                respond(&mut stream, 404, "Not Found", "text/plain", "no such event");
            }
        }
        _ => respond(&mut stream, 404, "Not Found", "text/plain", "unknown route"),
    }
}

fn token_seq(body: &str) -> u64 {
    let Some(start) = body.find("<d:sync-token>") else { return 0 };
    let rest = &body[start + "<d:sync-token>".len()..];
    let Some(end) = rest.find("</d:sync-token>") else { return 0 };
    rest[..end].trim().strip_prefix("tok-").and_then(|n| n.parse().ok()).unwrap_or(0)
}
