// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch-store` — persistence layer.
//!
//! One SQLite file holds every domain (mail, calendar, memory, approvals,
//! workflows, sessions). Domain crates own their queries; this crate owns the
//! connection, the schema and the append-only audit log.

mod schema;

use hatch_core::time::{now, to_rfc3339};
use hatch_core::{Error, Result};
pub use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

/// Current schema version written to `meta`.
pub const SCHEMA_VERSION: i64 = 1;

/// Map rusqlite errors into the workspace error type.
pub trait DbResultExt<T> {
    fn db(self) -> Result<T>;
}

impl<T> DbResultExt<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(|e| Error::Storage(e.to_string()))
    }
}

/// Handle to the hatch database.
pub struct Db {
    conn: Connection,
}

/// One row of the audit log.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: String,
    pub actor: String,
    pub event: String,
    pub detail: String,
}

impl Db {
    /// Open (and migrate) a database file, creating parent directories.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).db()?;
        conn.pragma_update(None, "journal_mode", "WAL").db()?;
        Self::init(conn)
    }

    /// In-memory database for tests and evaluation sandboxes.
    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory().db()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "foreign_keys", "ON").db()?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).db()?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Apply the schema. Idempotent.
    pub fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(schema::SCHEMA).db()?;
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )
            .db()?;
        Ok(())
    }

    /// Raw connection for domain repositories.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run `f` inside a transaction; rolls back on error.
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE").db()?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT").db()?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Append to the audit log. Callers must never put secrets in `detail`.
    pub fn audit(&self, actor: &str, event: &str, detail: &str) -> Result<()> {
        let detail = hatch_core::secrets::redact(detail);
        self.conn
            .execute(
                "INSERT INTO audit(ts, actor, event, detail) VALUES(?1, ?2, ?3, ?4)",
                params![to_rfc3339(&now()), actor, event, detail],
            )
            .db()?;
        Ok(())
    }

    /// Most recent audit entries, newest first.
    pub fn audit_tail(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let mut st = self
            .conn
            .prepare("SELECT id, ts, actor, event, detail FROM audit ORDER BY id DESC LIMIT ?1")
            .db()?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok(AuditEntry {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    actor: r.get(2)?,
                    event: r.get(3)?,
                    detail: r.get(4)?,
                })
            })
            .db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Read a `meta` value.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .db()
    }

    /// Write a `meta` value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .db()?;
        Ok(())
    }

    /// Row count of a table — used by `hatch doctor`.
    pub fn count(&self, table: &str) -> Result<i64> {
        if !schema::TABLES.contains(&table) {
            return Err(Error::Invalid(format!("unknown table {table}")));
        }
        self.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .db()
    }

    /// All table names known to the schema.
    pub fn tables() -> &'static [&'static str] {
        schema::TABLES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.migrate().unwrap();
        assert_eq!(db.meta("schema_version").unwrap().as_deref(), Some("1"));
        for t in Db::tables() {
            assert_eq!(db.count(t).unwrap(), if *t == "meta" { 1 } else { 0 }, "{t}");
        }
    }

    #[test]
    fn audit_redacts_secrets() {
        let db = Db::open_in_memory().unwrap();
        db.audit("test", "login", "验证码是 123456").unwrap();
        let tail = db.audit_tail(5).unwrap();
        assert_eq!(tail.len(), 1);
        assert!(!tail[0].detail.contains("123456"));
    }

    #[test]
    fn transaction_rolls_back() {
        let db = Db::open_in_memory().unwrap();
        let r: Result<()> = db.transaction(|c| {
            c.execute("INSERT INTO meta(key, value) VALUES('a', 'b')", []).db()?;
            Err(Error::Invalid("boom".into()))
        });
        assert!(r.is_err());
        assert_eq!(db.meta("a").unwrap(), None);
        assert!(db.count("drop table x").is_err());
    }

    #[test]
    fn file_database_persists() {
        let dir = std::env::temp_dir().join(format!("hatch-store-{}", std::process::id()));
        let path = dir.join("t.db");
        {
            let db = Db::open(&path).unwrap();
            db.set_meta("k", "v").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.meta("k").unwrap().as_deref(), Some("v"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
