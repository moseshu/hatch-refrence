//! The append-only per-task event log (M0): what a client reads to
//! follow a task, and what a reconnected client resumes from.
//!
//! A client keeps the last `seq` it saw and asks for events after that
//! cursor. Sequences are per task, assigned inside the writing
//! transaction (`MAX(seq) + 1` under `BEGIN IMMEDIATE`), so two
//! writers cannot be assigned the same number. Step completion writes
//! its event in the same transaction as the step transition (see
//! [`crate::unit::UnitStore::complete_step`]), so the two can never
//! split across a crash.

use crate::task::Lease;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt};
use serde::{Deserialize, Serialize};

/// One row of `task_events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub seq: i64,
    pub kind: String,
    pub step_id: String,
    pub detail: String,
    pub created_at: String,
}

pub(crate) const EVENT_COLS: &str = "task_id, seq, kind, step_id, detail, created_at";

pub(crate) fn event_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<TaskEvent> {
    Ok(TaskEvent {
        task_id: r.get(0)?,
        seq: r.get(1)?,
        kind: r.get(2)?,
        step_id: r.get(3)?,
        detail: r.get(4)?,
        created_at: r.get(5)?,
    })
}

/// Insert one event row inside an already-open transaction (used by
/// [`crate::unit::UnitStore::complete_step`], which commits the step
/// transition and its event atomically). The caller owns the guard.
pub(crate) fn insert_event(
    c: &hatch_store::Connection,
    task_id: &str,
    kind: &str,
    step_id: &str,
    detail: &str,
    ts: &str,
) -> Result<TaskEvent> {
    let seq: i64 = c
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM task_events WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .db()?;
    c.execute(
        "INSERT INTO task_events(task_id, seq, kind, step_id, detail, created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![task_id, seq, kind, step_id, detail, ts],
    )
    .db()?;
    Ok(TaskEvent {
        task_id: task_id.to_string(),
        seq,
        kind: kind.to_string(),
        step_id: step_id.to_string(),
        detail: detail.to_string(),
        created_at: ts.to_string(),
    })
}

pub struct EventLog<'a> {
    db: &'a Db,
}

impl<'a> EventLog<'a> {
    pub fn new(db: &'a Db) -> Self {
        EventLog { db }
    }

    /// Append one event (lease-guarded, sequential). Returns the
    /// stored event with its assigned sequence number.
    pub fn append_event(
        &self,
        task_id: &str,
        lease: &Lease,
        kind: &str,
        step_id: &str,
        detail: &str,
        now: &Timestamp,
    ) -> Result<TaskEvent> {
        let ts = to_rfc3339(now);
        self.db.transaction(|c| {
            let changed = c
                .execute(
                    "UPDATE tasks SET updated_at = ?2 WHERE id = ?1 AND status = 'running' AND lease_owner = ?3 AND lease_generation = ?4",
                    params![task_id, ts, lease.owner, lease.generation as i64],
                )
                .db()?;
            if changed == 0 {
                return Err(Error::Invalid(format!(
                    "task {task_id} 的租约已失效或已被他人接管，不能写入事件"
                )));
            }
            insert_event(c, task_id, kind, step_id, detail, &ts)
        })
    }

    /// Append a control-plane event (M1): a user command, a cancel, a
    /// version bump. No lease — the control plane is single-owner by
    /// construction (same argument as `TaskStore::cancel`), and the
    /// task need not be `running` (a blocked task's version can move).
    /// The task must exist; nothing else is required.
    pub fn append_control_event(
        &self,
        task_id: &str,
        kind: &str,
        step_id: &str,
        detail: &str,
        now: &Timestamp,
    ) -> Result<TaskEvent> {
        let ts = to_rfc3339(now);
        self.db.transaction(|c| {
            let exists: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE id = ?1",
                    params![task_id],
                    |r| r.get(0),
                )
                .db()?;
            if exists == 0 {
                return Err(Error::NotFound(format!("task {task_id} 不存在")));
            }
            c.execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
                params![task_id, ts],
            )
            .db()?;
            insert_event(c, task_id, kind, step_id, detail, &ts)
        })
    }

    /// Events after a cursor (`after_seq`, exclusive), oldest first.
    /// A client that disconnects resumes from the last seq it saw.
    pub fn events_since(
        &self,
        task_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<TaskEvent>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {EVENT_COLS} FROM task_events WHERE task_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
            ))
            .db()?;
        let rows = st
            .query_map(params![task_id, after_seq, limit as i64], event_row)
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }
}
