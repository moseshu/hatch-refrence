//! `hatch-store` — persistence layer.
//!
//! One SQLite file holds every domain (mail, calendar, memory, approvals,
//! workflows, sessions). Domain crates own their queries; this crate owns the
//! connection, the schema and the append-only audit log.

mod schema;

use hatch_core::time::{now, to_rfc3339};
use hatch_core::{Error, Result};
pub use rusqlite;
pub use rusqlite::{params, Connection, OptionalExtension, Row};
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
use std::path::Path;

/// Current schema version written to `meta`.
pub const SCHEMA_VERSION: i64 = 18;

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
///
/// The connection sits behind a re-entrant mutex so one `Db` can be shared by
/// threads (parallel sub-agents) while nested calls on the same thread — a
/// repository method called inside [`Db::transaction`] — never self-deadlock.
pub struct Db {
    conn: ReentrantMutex<Connection>,
}

/// Locked connection handle returned by [`Db::conn`].
pub type ConnGuard<'a> = ReentrantMutexGuard<'a, Connection>;

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
        let db = Db { conn: ReentrantMutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    /// Apply the schema. Idempotent.
    pub fn migrate(&self) -> Result<()> {
        let c = self.conn();
        c.execute_batch(schema::SCHEMA).db()?;
        // Column added in schema v7: existing databases created the table
        // without it, and `CREATE TABLE IF NOT EXISTS` never alters them.
        let has_goal_id: bool = {
            let mut st = c.prepare("PRAGMA table_info(worker_outputs)").db()?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == "goal_id")
        };
        if !has_goal_id {
            c.execute("ALTER TABLE worker_outputs ADD COLUMN goal_id TEXT NOT NULL DEFAULT ''", [])
                .db()?;
        }
        // Column added in schema v8: the child process pid, so a later
        // process can tell a still-running child from a stale record.
        let has_child_pid: bool = {
            let mut st = c.prepare("PRAGMA table_info(subagent_runs)").db()?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == "child_pid")
        };
        if !has_child_pid {
            c.execute("ALTER TABLE subagent_runs ADD COLUMN child_pid INTEGER NOT NULL DEFAULT 0", [])
                .db()?;
        }
        // Columns added in schema v9 (memory depth P2): salience defaults
        // to 0.5 (neutral) for old claims; last_used_at stays NULL (never
        // used — ranking falls back to created_at).
        for (column, ddl) in [
            ("salience", "ALTER TABLE claims ADD COLUMN salience REAL NOT NULL DEFAULT 0.5"),
            ("last_used_at", "ALTER TABLE claims ADD COLUMN last_used_at TEXT"),
        ] {
            let has: bool = {
                let mut st = c.prepare("PRAGMA table_info(claims)").db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == column)
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        // Columns added in schema v10 (P1 D1): the plan's durable goal,
        // the id of the step execution has reached ('' = none yet /
        // finished), and its milestones as a JSON array (default '[]').
        // Old plans get the documented defaults; no separate table.
        for (column, ddl) in [
            ("goal", "ALTER TABLE plans ADD COLUMN goal TEXT NOT NULL DEFAULT ''"),
            (
                "current_step",
                "ALTER TABLE plans ADD COLUMN current_step TEXT NOT NULL DEFAULT ''",
            ),
            (
                "milestones_json",
                "ALTER TABLE plans ADD COLUMN milestones_json TEXT NOT NULL DEFAULT '[]'",
            ),
        ] {
            let has: bool = {
                let mut st = c.prepare("PRAGMA table_info(plans)").db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == column)
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        // Columns added in schema v12 (M0): the execution lease and the
        // structured blocking reason on `tasks`, and the plan revision
        // on `plans`. Old rows get the documented defaults (no lease,
        // not blocked, plan version 1); the lease is only meaningful
        // once a worker claims the task with `TaskStore::start`.
        for (table, column, ddl) in [
            ("tasks", "lease_owner", "ALTER TABLE tasks ADD COLUMN lease_owner TEXT NOT NULL DEFAULT ''"),
            ("tasks", "lease_generation", "ALTER TABLE tasks ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0"),
            ("tasks", "lease_ttl_secs", "ALTER TABLE tasks ADD COLUMN lease_ttl_secs INTEGER NOT NULL DEFAULT 0"),
            ("tasks", "lease_expires_at", "ALTER TABLE tasks ADD COLUMN lease_expires_at TEXT NOT NULL DEFAULT ''"),
            ("tasks", "blocked_kind", "ALTER TABLE tasks ADD COLUMN blocked_kind TEXT NOT NULL DEFAULT ''"),
            ("plans", "version", "ALTER TABLE plans ADD COLUMN version INTEGER NOT NULL DEFAULT 1"),
        ] {
            let has: bool = {
                let mut st = c.prepare(&format!("PRAGMA table_info({table})")).db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == column)
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        // Columns added in schema v13 (M1): the task's plan link and
        // revision (`plan_id` names the hatch-plan plan when the task
        // runs one; `plan_version` is the §10.2 version commands carry
        // and approvals are bound to), and the model-call budget
        // (`max_model_calls`, 0 = unlimited). Old rows get plan version
        // 1 and no budget — the over-budget path never triggers for them.
        for (table, column, ddl) in [
            ("tasks", "plan_id", "ALTER TABLE tasks ADD COLUMN plan_id TEXT NOT NULL DEFAULT ''"),
            ("tasks", "plan_version", "ALTER TABLE tasks ADD COLUMN plan_version INTEGER NOT NULL DEFAULT 1"),
            ("tasks", "max_model_calls", "ALTER TABLE tasks ADD COLUMN max_model_calls INTEGER NOT NULL DEFAULT 0"),
        ] {
            let has: bool = {
                let mut st = c.prepare(&format!("PRAGMA table_info({table})")).db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == column)
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        // Columns added in schema v14 (task 5): the per-step acceptance
        // (`expected`) on `task_steps`, mirroring the plan's expected so
        // the planning list can show what each step must produce without
        // joining the plan. Old rows get '' (no acceptance recorded).
        let has_expected: bool = {
            let mut st = c.prepare("PRAGMA table_info(task_steps)").db()?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .db()?
                .iter()
                .any(|n| n == "expected")
        };
        if !has_expected {
            c.execute(
                "ALTER TABLE task_steps ADD COLUMN expected TEXT NOT NULL DEFAULT ''",
                [],
            )
            .db()?;
        }
        // Column added in schema v15 (M2): the task's mail source
        // (`fixture` for an eval world, `gmail` for a real-account
        // run; '' for tasks created before v15, which are fixture
        // tasks). An approval bound to a task is executed with the
        // task's sender — without this, `approvals approve` would
        // always use the fixture outbox and a Gmail invite would
        // silently land in a file instead of being sent.
        let has_source: bool = {
            let mut st = c.prepare("PRAGMA table_info(tasks)").db()?;
            let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .db()?
                .iter()
                .any(|n| n == "source")
        };
        if !has_source {
            c.execute(
                "ALTER TABLE tasks ADD COLUMN source TEXT NOT NULL DEFAULT ''",
                [],
            )
            .db()?;
        }
        // Columns added in schema v16 (task 6): how a step failed
        // (`plan_steps` and its `task_steps` mirror). Old rows get ''
        // (not failed); only a failure recording writes a kind.
        for (table, ddl) in [
            (
                "plan_steps",
                "ALTER TABLE plan_steps ADD COLUMN failure_kind TEXT NOT NULL DEFAULT ''",
            ),
            (
                "task_steps",
                "ALTER TABLE task_steps ADD COLUMN failure_kind TEXT NOT NULL DEFAULT ''",
            ),
        ] {
            let has: bool = {
                let mut st = c.prepare(&format!("PRAGMA table_info({table})")).db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .db()?
                    .iter()
                    .any(|n| n == "failure_kind")
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        // Columns added in schema v18 (task 8): the task a notification
        // belongs to and what kind of task event it is. Old rows get ''
        // (not task-linked); the inbox filters on these columns.
        for (table, column, ddl) in [
            ("notifications", "task_id", "ALTER TABLE notifications ADD COLUMN task_id TEXT NOT NULL DEFAULT ''"),
            ("notifications", "kind", "ALTER TABLE notifications ADD COLUMN kind TEXT NOT NULL DEFAULT ''"),
        ] {
            let has: bool = {
                let mut st = c.prepare(&format!("PRAGMA table_info({table})")).db()?;
                let rows = st.query_map([], |r| r.get::<_, String>(1)).db()?;
                rows.collect::<rusqlite::Result<Vec<_>>>().db()?.iter().any(|n| n == column)
            };
            if !has {
                c.execute(ddl, []).db()?;
            }
        }
        c.execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )
            .db()?;
        c.execute("INSERT OR IGNORE INTO meta(key, value) VALUES('build', ?1)", params![hatch_core::BUILD_TAG])
            .db()?;
        Ok(())
    }

    /// Locked connection for domain repositories. Hold it only for the
    /// duration of the queries — never across a model call or a sleep.
    pub fn conn(&self) -> ConnGuard<'_> {
        self.conn.lock()
    }

    /// Run `f` inside a transaction; rolls back on error. Other threads are
    /// kept out for the whole transaction.
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let c = self.conn();
        c.execute_batch("BEGIN IMMEDIATE").db()?;
        match f(&c) {
            Ok(v) => {
                c.execute_batch("COMMIT").db()?;
                Ok(v)
            }
            Err(e) => {
                let _ = c.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Append to the audit log. Callers must never put secrets in `detail`.
    pub fn audit(&self, actor: &str, event: &str, detail: &str) -> Result<()> {
        let detail = hatch_core::secrets::redact(detail);
        self.conn()
            .execute(
                "INSERT INTO audit(ts, actor, event, detail) VALUES(?1, ?2, ?3, ?4)",
                params![to_rfc3339(&now()), actor, event, detail],
            )
            .db()?;
        Ok(())
    }

    /// Most recent audit entries, newest first.
    pub fn audit_tail(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let c = self.conn();
        let mut st = c
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
        self.conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .db()
    }

    /// Write a `meta` value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .db()?;
        Ok(())
    }

    /// Delete a `meta` value (missing keys are a no-op).
    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM meta WHERE key = ?1", params![key])
            .db()?;
        Ok(())
    }

    /// Row count of a table — used by `hatch doctor`.
    pub fn count(&self, table: &str) -> Result<i64> {
        if !schema::TABLES.contains(&table) {
            return Err(Error::Invalid(format!("unknown table {table}")));
        }
        self.conn()
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
        assert_eq!(
            db.meta("schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION.to_string()).as_deref()
        );
        for t in Db::tables() {
            assert_eq!(db.count(t).unwrap(), if *t == "meta" { 2 } else { 0 }, "{t}");
        }
    }

    /// v9: a pre-v9 `claims` table (no `salience`/`last_used_at`) gains
    /// both columns on migrate, old rows get the documented defaults.
    #[test]
    fn v9_columns_migrate_old_claims() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO claims(id, subject, text, tags, source, trust, confidence, created_at, inferred) VALUES('clm_old', 'user', 'old', '[]', 'user:chat', 'user', 0.9, '2026-01-01T00:00:00Z', 0)",
                [],
            )
            .unwrap();
        db.conn().execute("ALTER TABLE claims DROP COLUMN salience", []).unwrap();
        db.conn().execute("ALTER TABLE claims DROP COLUMN last_used_at", []).unwrap();
        db.migrate().unwrap();
        let (salience, last_used): (f64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT salience, last_used_at FROM claims WHERE id = 'clm_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(salience, 0.5);
        assert!(last_used.is_none());
        db.migrate().unwrap();
    }

    /// v10: a pre-v10 `plans` table (no `goal` / `current_step` /
    /// `milestones_json`) gains all three columns on migrate, old rows
    /// get the documented defaults.
    #[test]
    fn v10_columns_migrate_old_plans() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO plans(id, task, status, model, created_at, updated_at) VALUES('pln_old', '做 X', 'approved', '', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        db.conn().execute("ALTER TABLE plans DROP COLUMN goal", []).unwrap();
        db.conn()
            .execute("ALTER TABLE plans DROP COLUMN current_step", [])
            .unwrap();
        db.conn()
            .execute("ALTER TABLE plans DROP COLUMN milestones_json", [])
            .unwrap();
        db.migrate().unwrap();
        let (goal, current_step, milestones): (String, String, String) = db
            .conn()
            .query_row(
                "SELECT goal, current_step, milestones_json FROM plans WHERE id = 'pln_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(goal, "");
        assert_eq!(current_step, "");
        assert_eq!(milestones, "[]");
        db.migrate().unwrap();
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

    #[test]
    fn shared_across_threads_and_reentrant_inside_transactions() {
        fn assert_sync<T: Sync + Send>() {}
        assert_sync::<Db>();
        let db = Db::open_in_memory().unwrap();
        std::thread::scope(|s| {
            for t in 0..4 {
                let db = &db;
                s.spawn(move || {
                    for i in 0..25 {
                        db.audit("t", "e", &format!("{t}-{i}")).unwrap();
                    }
                });
            }
        });
        assert_eq!(db.count("audit").unwrap(), 100);
        // A nested call on the same thread inside a transaction must not deadlock.
        db.transaction(|c| {
            c.execute("INSERT INTO meta(key, value) VALUES('x', '1')", []).db()?;
            db.audit("t", "inside", "tx")
        })
        .unwrap();
        assert_eq!(db.meta("x").unwrap().as_deref(), Some("1"));
    }
}
