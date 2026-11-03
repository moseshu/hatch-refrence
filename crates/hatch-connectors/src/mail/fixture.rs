//! Offline connectors: a JSON mailbox and a file outbox.
//!
//! They power `--dry-run`, the evaluation sandbox and tests. The outbox writes
//! real `.eml` files so you can open exactly what *would* have been sent.

use super::mime::compose;
use super::{MailSender, MailSource, OutgoingMail, ParsedMail};
use hatch_core::time::parse_rfc3339;
use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One mail in a fixture file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FixtureMail {
    pub uid: u32,
    pub message_id: String,
    pub from: String,
    #[serde(default)]
    pub from_name: String,
    #[serde(default)]
    pub to: Vec<String>,
    pub subject: String,
    pub date: String,
    pub body: String,
}

impl FixtureMail {
    pub fn to_parsed(&self) -> Result<ParsedMail> {
        Ok(ParsedMail {
            uid: Some(self.uid),
            message_id: self.message_id.clone(),
            from_addr: self.from.to_ascii_lowercase(),
            from_name: self.from_name.clone(),
            to: self.to.clone(),
            subject: self.subject.clone(),
            sent_at: Some(parse_rfc3339(&self.date)?),
            body: self.body.clone(),
        })
    }
}

pub struct FixtureMailbox {
    account: String,
    mails: Vec<FixtureMail>,
}

impl FixtureMailbox {
    pub fn new(account: &str, mails: Vec<FixtureMail>) -> Self {
        let mut mails = mails;
        mails.sort_by_key(|m| m.uid);
        FixtureMailbox { account: account.to_string(), mails }
    }

    pub fn load(account: &str, path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Connector(format!("cannot read fixture {}: {e}", path.display())))?;
        Ok(Self::new(account, serde_json::from_str(&text)?))
    }
}

impl MailSource for FixtureMailbox {
    fn account(&self) -> String {
        self.account.clone()
    }

    fn fetch_new(&mut self, after_uid: Option<u32>, limit: usize) -> Result<Vec<ParsedMail>> {
        let mut out: Vec<ParsedMail> = self
            .mails
            .iter()
            .filter(|m| after_uid.map(|a| m.uid > a).unwrap_or(true))
            .map(FixtureMail::to_parsed)
            .collect::<Result<_>>()?;
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }
}

/// Summary line of a mail written to the outbox.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutboxEntry {
    pub message_id: String,
    pub to: Vec<String>,
    pub subject: String,
    pub file: String,
}

/// Writes outgoing mail to a directory instead of sending it.
pub struct FileOutbox {
    dir: PathBuf,
    from: String,
    lock: Mutex<()>,
}

impl FileOutbox {
    pub fn new(dir: &Path, from: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(FileOutbox { dir: dir.to_path_buf(), from: from.to_string(), lock: Mutex::new(()) })
    }

    pub fn entries(&self) -> Result<Vec<OutboxEntry>> {
        let index = self.dir.join("outbox.jsonl");
        if !index.exists() {
            return Ok(vec![]);
        }
        std::fs::read_to_string(index)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(Error::from))
            .collect()
    }
}

impl MailSender for FileOutbox {
    fn send(&self, mail: &OutgoingMail) -> Result<String> {
        let _g = self.lock.lock().unwrap();
        let mut m = mail.clone();
        m.from = self.from.clone();
        let (id, wire) = compose(&m, &hatch_core::time::now());
        let n = self.entries()?.len() + 1;
        let file = format!("{n:04}-{}.eml", id.trim_matches(|c| c == '<' || c == '>').replace(['@', '/'], "_"));
        std::fs::write(self.dir.join(&file), wire)?;
        let entry = OutboxEntry { message_id: id.clone(), to: m.to.clone(), subject: m.subject.clone(), file };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("outbox.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&entry)?)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm(uid: u32) -> FixtureMail {
        FixtureMail {
            uid,
            message_id: format!("<{uid}@x>"),
            from: "A@X.com".into(),
            from_name: "A".into(),
            to: vec!["me@126.com".into()],
            subject: format!("s{uid}"),
            date: "2026-11-02T01:00:00Z".into(),
            body: "b".into(),
        }
    }

    #[test]
    fn mailbox_respects_cursor_and_limit() {
        let mut mb = FixtureMailbox::new("me", vec![fm(3), fm(1), fm(2)]);
        assert_eq!(mb.fetch_new(None, 10).unwrap().len(), 3);
        let got = mb.fetch_new(Some(1), 1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, Some(3));
        assert_eq!(got[0].from_addr, "a@x.com");
    }

    #[test]
    fn outbox_writes_eml_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let ob = FileOutbox::new(dir.path(), "me@126.com").unwrap();
        let id = ob
            .send(&OutgoingMail {
                from: "ignored".into(),
                to: vec!["b@y.com".into()],
                subject: "Re: hi".into(),
                body: "ok".into(),
                in_reply_to: None,
                references: vec![],
            })
            .unwrap();
        let e = ob.entries().unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].message_id, id);
        let eml = std::fs::read_to_string(dir.path().join(&e[0].file)).unwrap();
        assert!(eml.starts_with("From: me@126.com"));
    }
}
