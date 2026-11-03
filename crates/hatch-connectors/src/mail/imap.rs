//! A deliberately small IMAP4rev1 client: LOGIN, ID, SELECT, UID SEARCH,
//! UID FETCH BODY.PEEK[], LOGOUT. `BODY.PEEK` never sets the \Seen flag, so
//! triage does not mark the user's mail as read.
//!
//! NetEase mailboxes (126 / 163 / yeah.net) reject SELECT with "Unsafe Login"
//! unless the client first identifies itself with the RFC 2971 `ID` command,
//! which is why `send_id` exists.

use super::parse::parse_rfc822;
use super::{MailSource, ParsedMail};
use chrono::Duration;
use hatch_core::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};

/// One tagged command's untagged lines and literals.
#[derive(Debug, Default)]
pub struct Response {
    pub lines: Vec<String>,
    pub literals: Vec<Vec<u8>>,
}

pub struct ImapSession<S: Read + Write> {
    stream: BufReader<S>,
    tag: u32,
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn literal_len(line: &str) -> Option<usize> {
    let t = line.trim_end();
    if !t.ends_with('}') {
        return None;
    }
    let open = t.rfind('{')?;
    t[open + 1..t.len() - 1].trim_end_matches('+').parse().ok()
}

impl<S: Read + Write> ImapSession<S> {
    /// Wrap a connected stream and consume the server greeting.
    pub fn new(stream: S) -> Result<Self> {
        let mut s = ImapSession { stream: BufReader::new(stream), tag: 0 };
        let greeting = s.read_line()?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            return Err(Error::Connector(format!("unexpected IMAP greeting: {}", greeting.trim())));
        }
        Ok(s)
    }

    fn read_line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = self.stream.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Err(Error::Connector("IMAP connection closed".into()));
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Send a command and collect the response up to its tagged completion.
    pub fn command(&mut self, cmd: &str) -> Result<Response> {
        self.tag += 1;
        let tag = format!("h{}", self.tag);
        self.stream.get_mut().write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        self.stream.get_mut().flush()?;
        let mut resp = Response::default();
        loop {
            let line = self.read_line()?;
            if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
                let rest = rest.trim();
                if rest.starts_with("OK") {
                    return Ok(resp);
                }
                let verb = cmd.split_whitespace().next().unwrap_or("");
                return Err(Error::Connector(format!("IMAP {verb} failed: {rest}")));
            }
            if let Some(n) = literal_len(&line) {
                let mut lit = vec![0u8; n];
                self.stream.read_exact(&mut lit)?;
                resp.literals.push(lit);
            }
            resp.lines.push(line);
        }
    }

    pub fn login(&mut self, user: &str, pass: &str) -> Result<()> {
        self.command(&format!("LOGIN {} {}", quote(user), quote(pass))).map(|_| ())
    }

    pub fn id(&mut self) -> Result<()> {
        self.command(&format!(
            "ID (\"name\" \"hatch-refrence\" \"version\" \"{}\" \"vendor\" \"Moses\")",
            env!("CARGO_PKG_VERSION")
        ))
        .map(|_| ())
    }

    pub fn select(&mut self, mailbox: &str) -> Result<()> {
        self.command(&format!("SELECT {}", quote(mailbox))).map(|_| ())
    }

    /// `UID SEARCH <criteria>` → UIDs.
    pub fn uid_search(&mut self, criteria: &str) -> Result<Vec<u32>> {
        let r = self.command(&format!("UID SEARCH {criteria}"))?;
        let mut out = Vec::new();
        for l in r.lines {
            if let Some(rest) = l.strip_prefix("* SEARCH") {
                out.extend(rest.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Raw RFC 822 bytes of one message, without setting \Seen.
    pub fn uid_fetch(&mut self, uid: u32) -> Result<Vec<u8>> {
        let mut r = self.command(&format!("UID FETCH {uid} BODY.PEEK[]"))?;
        if r.literals.is_empty() {
            return Err(Error::Connector(format!("no body returned for UID {uid}")));
        }
        Ok(r.literals.swap_remove(0))
    }

    pub fn logout(&mut self) -> Result<()> {
        // Servers may close the socket right after BYE; ignore that.
        let _ = self.command("LOGOUT");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub mailbox: String,
    /// Send RFC 2971 ID before SELECT (required by NetEase mailboxes).
    pub send_id: bool,
}

impl ImapConfig {
    pub fn needs_id(host: &str) -> bool {
        ["126.com", "163.com", "yeah.net", "188.com"].iter().any(|d| host.ends_with(d))
    }
}

/// IMAP-backed [`MailSource`]. The password lives only in memory.
pub struct ImapSource {
    cfg: ImapConfig,
    password: String,
}

impl ImapSource {
    pub fn new(cfg: ImapConfig, password: String) -> Self {
        ImapSource { cfg, password }
    }

    /// Run the fetch protocol over any stream (TLS in production, scripted in tests).
    pub fn fetch_over<S: Read + Write>(
        &self,
        stream: S,
        after_uid: Option<u32>,
        limit: usize,
        now: &hatch_core::time::Timestamp,
    ) -> Result<Vec<ParsedMail>> {
        let mut s = ImapSession::new(stream)?;
        s.login(&self.cfg.username, &self.password)?;
        if self.cfg.send_id {
            s.id()?;
        }
        s.select(&self.cfg.mailbox)?;
        let criteria = match after_uid {
            Some(u) => format!("UID {}:*", u + 1),
            None => format!("SINCE {}", (*now - Duration::days(7)).format("%d-%b-%Y")),
        };
        let mut uids: Vec<u32> = s.uid_search(&criteria)?.into_iter().filter(|u| after_uid.map(|a| *u > a).unwrap_or(true)).collect();
        if uids.len() > limit {
            uids = uids.split_off(uids.len() - limit);
        }
        let mut out = Vec::new();
        for uid in uids {
            let raw = s.uid_fetch(uid)?;
            out.push(parse_rfc822(&raw, Some(uid))?);
        }
        s.logout()?;
        Ok(out)
    }
}

impl MailSource for ImapSource {
    fn account(&self) -> String {
        self.cfg.username.to_ascii_lowercase()
    }

    fn fetch_new(&mut self, after_uid: Option<u32>, limit: usize) -> Result<Vec<ParsedMail>> {
        let stream = crate::tls::connect(&self.cfg.host, self.cfg.port, std::time::Duration::from_secs(30))?;
        self.fetch_over(stream, after_uid, limit, &hatch_core::time::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Replays a canned server transcript and records what the client wrote.
    struct Scripted {
        input: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mail(uid: u32, subject: &str) -> String {
        format!("From: Boss <boss@corp.com>\r\nTo: me@126.com\r\nSubject: {subject}\r\nMessage-ID: <m{uid}@corp.com>\r\n\r\nbody {uid}\r\n")
    }

    #[test]
    fn full_fetch_transcript_with_literals_and_id() {
        let m5 = mail(5, "weekly");
        let m9 = mail(9, "urgent");
        let server = format!(
            "* OK IMAP ready\r\n\
             h1 OK LOGIN completed\r\n\
             * ID NIL\r\nh2 OK ID completed\r\n\
             * 12 EXISTS\r\n* OK [UIDVALIDITY 1]\r\nh3 OK [READ-WRITE] SELECT completed\r\n\
             * SEARCH 4 5 9\r\nh4 OK SEARCH completed\r\n\
             * 2 FETCH (UID 5 BODY[] {{{}}}\r\n{})\r\nh5 OK FETCH completed\r\n\
             * 3 FETCH (UID 9 BODY[] {{{}}}\r\n{})\r\nh6 OK FETCH completed\r\n\
             * BYE\r\nh7 OK LOGOUT completed\r\n",
            m5.len(), m5, m9.len(), m9
        );
        let stream = Scripted { input: Cursor::new(server.into_bytes()), written: vec![] };
        let src = ImapSource::new(
            ImapConfig { host: "imap.126.com".into(), port: 993, username: "me@126.com".into(), mailbox: "INBOX".into(), send_id: true },
            "app-code\"x".into(),
        );
        let now = hatch_core::time::now();
        let mails = src.fetch_over(stream, Some(4), 10, &now).unwrap();
        assert_eq!(mails.len(), 2);
        assert_eq!(mails[0].uid, Some(5));
        assert_eq!(mails[1].subject, "urgent");
        assert_eq!(mails[1].message_id, "<m9@corp.com>");
        assert!(ImapConfig::needs_id("imap.126.com"));
    }

    #[test]
    fn writes_quoted_login_and_peek() {
        let server = "* OK hi\r\nh1 OK\r\nh2 OK\r\n* SEARCH\r\nh3 OK\r\nh4 OK\r\n";
        let mut s = ImapSession::new(Scripted { input: Cursor::new(server.as_bytes().to_vec()), written: vec![] }).unwrap();
        s.login("me@126.com", "p\"w").unwrap();
        s.select("INBOX").unwrap();
        assert!(s.uid_search("UID 1:*").unwrap().is_empty());
        s.logout().unwrap();
        let w = String::from_utf8(s.stream.get_ref().written.clone()).unwrap();
        assert!(w.contains("h1 LOGIN \"me@126.com\" \"p\\\"w\"\r\n"));
        assert!(w.contains("h3 UID SEARCH UID 1:*"));
    }

    #[test]
    fn surfaces_unsafe_login_rejection() {
        let server = "* OK hi\r\nh1 OK\r\nh2 NO SELECT Unsafe Login. Please contact kefu@188.com for help\r\n";
        let mut s = ImapSession::new(Scripted { input: Cursor::new(server.as_bytes().to_vec()), written: vec![] }).unwrap();
        s.login("a", "b").unwrap();
        let e = s.select("INBOX").unwrap_err().to_string();
        assert!(e.contains("Unsafe Login"));
        assert_eq!(literal_len("* 1 FETCH (BODY[] {42}\r\n"), Some(42));
        assert_eq!(literal_len("* OK done\r\n"), None);
    }
}
