//! Persisted execution parameters (P0 product closed loop, schema
//! v20): the validated create payload a task was created with, so a
//! worker resuming after a daemon restart needs no CLI flags.
//!
//! The row is opaque JSON to this crate — the CLI layer owns the
//! schema (`RunParams` in hatch-cli) and interprets it. This store
//! only guarantees one row per task, keyed by task id, plus the
//! payload hash the idempotency check compares.
//!
//! Tasks created before v20 (CLI `task create`, old delegations)
//! have no row: the resident worker skips them — they stay runnable
//! only via an explicit `task run` that supplies the parameters.

use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

/// One task's persisted execution parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunParamsRecord {
    pub task_id: String,
    pub payload_hash: String,
    pub params_json: String,
}

pub struct TaskRunParams<'a> {
    db: &'a Db,
}

impl<'a> TaskRunParams<'a> {
    pub fn new(db: &'a Db) -> Self {
        TaskRunParams { db }
    }

    /// Insert or replace the row for a task (create, or the
    /// idempotency repair when a create crashed between the task
    /// row and this row).
    pub fn save(
        &self,
        task_id: &str,
        payload_hash: &str,
        params_json: &str,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db
            .conn()
            .execute(
                "INSERT INTO task_run_params(task_id, payload_hash, params_json, created_at, updated_at) \
                 VALUES(?1,?2,?3,?4,?4) \
                 ON CONFLICT(task_id) DO UPDATE SET payload_hash = excluded.payload_hash, params_json = excluded.params_json, updated_at = excluded.updated_at",
                params![task_id, payload_hash, params_json, ts],
            )
            .db()?;
        Ok(())
    }

    /// The row for a task, if it was created with parameters.
    pub fn get(&self, task_id: &str) -> Result<Option<RunParamsRecord>> {
        let row = self
            .db
            .conn()
            .query_row(
                "SELECT task_id, payload_hash, params_json FROM task_run_params WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok(RunParamsRecord {
                        task_id: r.get(0)?,
                        payload_hash: r.get(1)?,
                        params_json: r.get(2)?,
                    })
                },
            )
            .optional()
            .db()?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;
    use hatch_store::Db;

    #[test]
    fn save_get_and_replace_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskRunParams::new(&db);
        let t = now();
        assert!(store.get("tsk_missing").unwrap().is_none());
        store
            .save("tsk_1", "hash-a", r#"{"kind":"meeting-brief"}"#, &t)
            .unwrap();
        let row = store.get("tsk_1").unwrap().unwrap();
        assert_eq!(row.payload_hash, "hash-a");
        assert_eq!(row.params_json, r#"{"kind":"meeting-brief"}"#);
        // Replace (the idempotency repair path) overwrites in place.
        store
            .save("tsk_1", "hash-b", r#"{"kind":"weekly-report"}"#, &t)
            .unwrap();
        let row = store.get("tsk_1").unwrap().unwrap();
        assert_eq!(row.payload_hash, "hash-b");
        assert_eq!(row.params_json, r#"{"kind":"weekly-report"}"#);
    }
}
