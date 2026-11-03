//! SMTP submission over implicit TLS (port 465) with AUTH LOGIN.

use super::mime::{compose, dot_stuff};
use super::{MailSender, OutgoingMail};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hatch_core::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};

pub struct SmtpClient<S: Read + Write> {
    stream: BufReader<S>,
}

impl<S: Read + Write> SmtpClient<S> {
    pub fn new(stream: S) -> Result<Self> {
        let mut c = SmtpClient { stream: BufReader::new(stream) };
        c.expect(220)?;
        Ok(c)
    }

    /// Read a (possibly multi-line) reply: `250-...` continues, `250 ...` ends.
    pub fn reply(&mut self) -> Result<(u16, String)> {
        let mut text = String::new();
        loop {
            let mut line = String::new();
            if self.stream.read_line(&mut line)? == 0 {
                return Err(Error::Connector("SMTP connection closed".into()));
            }
            let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| Error::Connector(format!("bad SMTP reply {line:?}")))?;
            text.push_str(line.get(4..).unwrap_or("").trim_end());
            text.push(' ');
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text.trim().to_string()));
            }
        }
    }

    fn expect(&mut self, want: u16) -> Result<String> {
        let (code, text) = self.reply()?;
        if code != want {
            return Err(Error::Connector(format!("SMTP expected {want}, got {code} {text}")));
        }
        Ok(text)
    }

    fn send_line(&mut self, line: &str) -> Result<()> {
        self.stream.get_mut().write_all(format!("{line}\r\n").as_bytes())?;
        self.stream.get_mut().flush()?;
        Ok(())
    }

    pub fn cmd(&mut self, line: &str, want: u16) -> Result<String> {
        self.send_line(line)?;
        self.expect(want)
    }

    /// Full submission. `wire` must already be a composed message with CRLF line endings.
    pub fn submit(&mut self, user: &str, pass: &str, from: &str, to: &[String], wire: &str) -> Result<()> {
        self.cmd("EHLO hatch.local", 250)?;
        self.cmd("AUTH LOGIN", 334)?;
        self.cmd(&B64.encode(user), 334)?;
        self.cmd(&B64.encode(pass), 235).map_err(|_| Error::Connector("SMTP authentication failed (check the app password)".into()))?;
        self.cmd(&format!("MAIL FROM:<{from}>"), 250)?;
        for r in to {
            self.cmd(&format!("RCPT TO:<{r}>"), 250)?;
        }
        self.cmd("DATA", 354)?;
        self.stream.get_mut().write_all(dot_stuff(wire.trim_end_matches("\r\n")).as_bytes())?;
        self.cmd("\r\n.", 250)?;
        let _ = self.cmd("QUIT", 221);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from: String,
}

pub struct SmtpSender {
    cfg: SmtpConfig,
    password: String,
}

impl SmtpSender {
    pub fn new(cfg: SmtpConfig, password: String) -> Self {
        SmtpSender { cfg, password }
    }
}

impl MailSender for SmtpSender {
    fn send(&self, mail: &OutgoingMail) -> Result<String> {
        if mail.to.is_empty() {
            return Err(Error::Invalid("mail has no recipients".into()));
        }
        let mut m = mail.clone();
        m.from = self.cfg.from.clone();
        let (id, wire) = compose(&m, &hatch_core::time::now());
        let stream = crate::tls::connect(&self.cfg.host, self.cfg.port, std::time::Duration::from_secs(30))?;
        let mut c = SmtpClient::new(stream)?;
        c.submit(&self.cfg.username, &self.password, &m.from, &m.to, &wire)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    #[test]
    fn submission_dialogue() {
        let server = "220 smtp ready\r\n250-hello\r\n250-AUTH LOGIN\r\n250 8BITMIME\r\n334 VXNlcm5hbWU6\r\n334 UGFzc3dvcmQ6\r\n235 ok\r\n250 ok\r\n250 ok\r\n354 go\r\n250 queued\r\n221 bye\r\n";
        let mut c = SmtpClient::new(Scripted { input: Cursor::new(server.as_bytes().to_vec()), written: vec![] }).unwrap();
        c.submit("me@126.com", "code", "me@126.com", &["a@b.com".into()], "Subject: x\r\n\r\n.hidden\r\n").unwrap();
        let w = String::from_utf8(c.stream.get_ref().written.clone()).unwrap();
        assert!(w.contains("RCPT TO:<a@b.com>\r\n"));
        assert!(w.contains("\r\n..hidden"));
        assert!(w.contains("\r\n.\r\n"));
        assert!(w.contains(&B64.encode("code")));
    }

    #[test]
    fn auth_failure_is_reported_without_password() {
        let server = "220 hi\r\n250 hello\r\n334 a\r\n334 b\r\n535 bad\r\n";
        let mut c = SmtpClient::new(Scripted { input: Cursor::new(server.as_bytes().to_vec()), written: vec![] }).unwrap();
        let e = c.submit("u", "secret-pass", "u", &["x@y".into()], "x").unwrap_err().to_string();
        assert!(e.contains("authentication failed"));
        assert!(!e.contains("secret-pass"));
    }
}
