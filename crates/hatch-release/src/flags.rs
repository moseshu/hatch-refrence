//! Named feature flags (task 9).
//!
//! A flag is one row: name, on/off, updated time. A flag that has
//! never been set reads as its default — every flag defaults to
//! **on**, so an existing database behaves exactly as before until
//! someone explicitly turns a flag off. That is the whole point of
//! the pre-release rule "close the new-task entry, keep in-flight
//! tasks queryable": the flag only ever *closes* an entry point,
//! it never hides state.
//!
//! The one flag that exists today:
//!
//! * `new_task_entry` — creating a fresh task (`task create`, and
//!   a fresh `task run` without `--task`). When off, creation is
//!   refused with a message that says what still works: showing a
//!   task, its events, and its receipts. Resuming an existing task
//!   is not creation, and is never blocked by this flag.

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

/// The flag that gates creating a fresh task.
pub const NEW_TASK_ENTRY: &str = "new_task_entry";

pub struct FlagStore<'a> {
    db: &'a Db,
}

impl<'a> FlagStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        FlagStore { db }
    }

    /// Set a flag (insert or overwrite).
    pub fn set(&self, name: &str, enabled: bool, now: &Timestamp) -> Result<()> {
        let ts = to_rfc3339(now);
        let c = self.db.conn();
        c.execute(
            "INSERT INTO feature_flags(name, enabled, updated_at) VALUES(?1,?2,?3) \
             ON CONFLICT(name) DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![name, enabled as i64, ts],
        )
        .db()?;
        Ok(())
    }

    /// The stored value, if the flag was ever set.
    pub fn get(&self, name: &str) -> Result<Option<bool>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                "SELECT enabled FROM feature_flags WHERE name = ?1",
                params![name],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .db()?;
        Ok(row.map(|v| v != 0))
    }

    /// The effective value: the stored value, or the default
    /// (on) when the flag was never set.
    pub fn is_enabled(&self, name: &str) -> Result<bool> {
        Ok(self.get(name)?.unwrap_or(true))
    }

    /// All flags that were ever set, in name order.
    pub fn list(&self) -> Result<Vec<(String, bool)>> {
        let c = self.db.conn();
        let mut st = c
            .prepare("SELECT name, enabled FROM feature_flags ORDER BY name")
            .db()?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })
            .db()?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.db()?);
        }
        Ok(out)
    }
}

/// Refuse creating a fresh task while the new-task entry is
/// closed. Call this at the CLI entry points that create a task
/// (`task create`, a fresh `task run`); never at resume, show,
/// events or receipt reads.
pub fn ensure_new_task_entry(db: &Db) -> Result<()> {
    if FlagStore::new(db).is_enabled(NEW_TASK_ENTRY)? {
        return Ok(());
    }
    Err(Error::Invalid(
        "新任务入口已关闭（功能开关 new_task_entry=off）。在途任务仍可查询，已发动作的回执仍可查询；用 `hatch release flag new_task_entry on` 重新打开。"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_store::Db;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    #[test]
    fn unset_flag_defaults_to_on() {
        let db = Db::open_in_memory().unwrap();
        let store = FlagStore::new(&db);
        assert_eq!(store.get("new_task_entry").unwrap(), None);
        assert!(store.is_enabled("new_task_entry").unwrap());
        assert!(ensure_new_task_entry(&db).is_ok());
    }

    #[test]
    fn off_blocks_entry_and_on_reopens_it() {
        let db = Db::open_in_memory().unwrap();
        let store = FlagStore::new(&db);
        store.set(NEW_TASK_ENTRY, false, &now()).unwrap();
        assert!(!store.is_enabled(NEW_TASK_ENTRY).unwrap());
        let err = ensure_new_task_entry(&db).unwrap_err();
        assert!(err.to_string().contains("新任务入口已关闭"));
        store.set(NEW_TASK_ENTRY, true, &now()).unwrap();
        assert!(ensure_new_task_entry(&db).is_ok());
        assert_eq!(
            store.list().unwrap(),
            vec![(NEW_TASK_ENTRY.to_string(), true)]
        );
    }
}
