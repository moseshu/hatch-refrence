// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Local mail store and per-account UID cursor.

use super::ParsedMail;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StoredMail {
    pub id: String,
    pub account: String,
    pub message_id: String,
    pub uid: Option<u32>,
    pub from_addr: String,
    pub from_name: String,
    pub to: Vec<String>,
    pub subject: String,
    pub sent_at: Option<Timestamp>,
    pub body: String,
}

pub fn mail_id(account: &str, message_id: &str) -> String {
    ids::stable_id("mail", &[account, message_id])
}

const COLS: &str = "id, account, message_id, uid, from_addr, from_name, to_addrs, subject, sent_at, body";

fn row_to_mail(r: &rusqlite::Row) -> rusqlite::Result<StoredMail> {
    let to: String = r.get(6)?;
    let sent: Option<String> = r.get(8)?;
    Ok(StoredMail {
        id: r.get(0)?,
        account: r.get(1)?,
        message_id: r.get(2)?,
        uid: r.get::<_, Option<i64>>(3)?.map(|u| u as u32),
        from_addr: r.get(4)?,
        from_name: r.get(5)?,
        to: serde_json::from_str(&to).unwrap_or_default(),
        subject: r.get(7)?,
        sent_at: sent.and_then(|s| parse_rfc3339(&s).ok()),
        body: r.get(9)?,
    })
}

pub struct MailRepo<'a> {
    db: &'a Db,
}

impl<'a> MailRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        MailRepo { db }
    }

    /// Store a mail; returns `(id, newly_inserted)`. Re-fetching is harmless.
    pub fn store(&self, account: &str, m: &ParsedMail, now: &Timestamp) -> Result<(String, bool)> {
        let id = mail_id(account, &m.message_id);
        let n = self
            .db
            .conn()
            .execute(
                &format!("INSERT OR IGNORE INTO mail_messages({COLS}, fetched_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"),
                params![
                    id,
                    account,
                    m.message_id,
                    m.uid.map(|u| u as i64),
                    m.from_addr,
                    m.from_name,
                    serde_json::to_string(&m.to)?,
                    m.subject,
                    m.sent_at.map(|t| to_rfc3339(&t)),
                    m.body,
                    to_rfc3339(now)
                ],
            )
            .db()?;
        Ok((id, n > 0))
    }

    pub fn get(&self, id: &str) -> Result<StoredMail> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM mail_messages WHERE id = ?1"), params![id], row_to_mail)
            .optional()
            .db()?
            .ok_or_else(|| Error::NotFound(format!("mail {id}")))
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<StoredMail>> {
        let mut st = self
            .db
            .conn()
            .prepare(&format!("SELECT {COLS} FROM mail_messages ORDER BY sent_at DESC LIMIT ?1"))
            .db()?;
        let rows = st.query_map(params![limit as i64], row_to_mail).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Substring search over sender, subject and body.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<StoredMail>> {
        let like = format!("%{}%", query.replace('%', "").replace('_', ""));
        let mut st = self
            .db
            .conn()
            .prepare(&format!(
                "SELECT {COLS} FROM mail_messages
                 WHERE subject LIKE ?1 OR body LIKE ?1 OR from_addr LIKE ?1 OR from_name LIKE ?1
                 ORDER BY sent_at DESC LIMIT ?2"
            ))
            .db()?;
        let rows = st.query_map(params![like, limit as i64], row_to_mail).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn cursor(&self, account: &str) -> Result<Option<u32>> {
        self.db
            .conn()
            .query_row("SELECT last_uid FROM mail_cursor WHERE account = ?1", params![account], |r| r.get::<_, i64>(0))
            .optional()
            .db()
            .map(|o| o.map(|u| u as u32))
    }

    pub fn set_cursor(&self, account: &str, uid: u32) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO mail_cursor(account, last_uid) VALUES(?1, ?2)
                 ON CONFLICT(account) DO UPDATE SET last_uid = MAX(last_uid, excluded.last_uid)",
                params![account, uid as i64],
            )
            .db()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_is_idempotent_and_cursor_monotonic() {
        let db = Db::open_in_memory().unwrap();
        let r = MailRepo::new(&db);
        let now = hatch_core::time::now();
        let m = ParsedMail {
            uid: Some(3),
            message_id: "<a@b>".into(),
            from_addr: "x@y.com".into(),
            from_name: "X".into(),
            to: vec!["me@126.com".into()],
            subject: "季度预算".into(),
            sent_at: Some(now),
            body: "请审批".into(),
        };
        let (id, new) = r.store("me", &m, &now).unwrap();
        assert!(new);
        assert!(!r.store("me", &m, &now).unwrap().1);
        assert_eq!(r.get(&id).unwrap().subject, "季度预算");
        assert_eq!(r.search("预算", 5).unwrap().len(), 1);
        assert_eq!(r.recent(5).unwrap().len(), 1);
        assert_eq!(r.cursor("me").unwrap(), None);
        r.set_cursor("me", 9).unwrap();
        r.set_cursor("me", 4).unwrap();
        assert_eq!(r.cursor("me").unwrap(), Some(9));
    }
}
