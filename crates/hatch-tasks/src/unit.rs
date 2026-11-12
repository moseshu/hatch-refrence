//! Durable execution units (M0): the recoverable step record.
//!
//! One `task_steps` row per plan step (see [`crate::steps`]); one
//! `task_units` row per model decision batch inside a step; one
//! `task_unit_results` row per dispatched call. A call is complete iff
//! its result row exists, so a killed worker resumes exactly the calls
//! without results — request and observation stay paired, and a re-run
//! never re-executes a call that already produced a result. The event
//! log itself lives in [`crate::events::EventLog`]; the resume side
//! lives in [`crate::recovery`].
//!
//! Every write is lease-guarded: the guard UPDATE on `tasks` (status =
//! 'running' AND owner AND generation) runs in the same transaction
//! as the write, so a stale owner matches zero rows and writes nothing.

use crate::task::Lease;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Status of a step or a unit. `Pending` → `Running` → `Done`/`Failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl ExecStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecStatus::Pending => "pending",
            ExecStatus::Running => "running",
            ExecStatus::Done => "done",
            ExecStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> ExecStatus {
        match s {
            "running" => ExecStatus::Running,
            "done" => ExecStatus::Done,
            "failed" => ExecStatus::Failed,
            _ => ExecStatus::Pending,
        }
    }
}

/// One call in a unit's decision batch: the model asked for this tool
/// with these arguments. The id is stable (model-issued `call_id`, or
/// the worker's own id for workflow calls), so a retry of the same
/// batch addresses the same calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnitCallSpec {
    pub id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// One row of `task_units`: one model decision batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecUnit {
    pub task_id: String,
    pub unit_seq: i64,
    pub step_id: String,
    pub calls: Vec<UnitCallSpec>,
    pub status: ExecStatus,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub created_at: String,
    pub updated_at: String,
}

/// One row of `task_unit_results`: the observation for one call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnitCall {
    pub task_id: String,
    pub unit_seq: i64,
    pub call_id: String,
    pub tool: String,
    pub content: String,
    pub trust: String,
    pub created_at: String,
}

const UNIT_COLS: &str = "task_id, unit_seq, step_id, calls_json, status, model_calls, tool_calls, created_at, updated_at";
pub(crate) const RESULT_COLS: &str = "task_id, unit_seq, call_id, tool, content, trust, created_at";

fn unit_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<ExecUnit> {
    Ok(ExecUnit {
        task_id: r.get(0)?,
        unit_seq: r.get(1)?,
        step_id: r.get(2)?,
        calls: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        status: ExecStatus::parse(&r.get::<_, String>(4)?),
        model_calls: r.get::<_, i64>(5)? as u64,
        tool_calls: r.get::<_, i64>(6)? as u64,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

pub(crate) fn result_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<UnitCall> {
    Ok(UnitCall {
        task_id: r.get(0)?,
        unit_seq: r.get(1)?,
        call_id: r.get(2)?,
        tool: r.get(3)?,
        content: r.get(4)?,
        trust: r.get(5)?,
        created_at: r.get(6)?,
    })
}

pub struct UnitStore<'a> {
    db: &'a Db,
}

impl<'a> UnitStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        UnitStore { db }
    }

    pub(crate) fn db(&self) -> &Db {
        self.db
    }

    /// One unit by `(task_id, unit_seq)` (resume side, see `recovery`).
    pub(crate) fn get_unit(&self, task_id: &str, unit_seq: i64) -> Result<Option<ExecUnit>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {UNIT_COLS} FROM task_units WHERE task_id = ?1 AND unit_seq = ?2"
            ))
            .db()?;
        st.query_row(params![task_id, unit_seq], unit_row)
            .optional()
            .db()
    }

    /// The lease guard, inside a transaction: proves the caller still
    /// owns this running task. Zero rows = stale lease or wrong state.
    /// `pub(crate)` so the step impl (`steps.rs`) shares the one guard.
    pub(crate) fn guard(
        c: &hatch_store::Connection,
        task_id: &str,
        lease: &Lease,
        now: &str,
    ) -> Result<()> {
        let changed = c
            .execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1 AND status = 'running' AND lease_owner = ?3 AND lease_generation = ?4",
                params![task_id, now, lease.owner, lease.generation as i64],
            )
            .db()?;
        if changed == 0 {
            return Err(Error::Invalid(format!(
                "task {task_id} 的租约已失效或已被他人接管，不能写入执行记录"
            )));
        }
        Ok(())
    }

    /// Start a unit for `step_id`: the model decision batch. The
    /// sequence is per task (`MAX(unit_seq) + 1`); a caller that resumes
    /// reuses `latest_unit` instead of starting a new one.
    pub fn begin_unit(
        &self,
        task_id: &str,
        lease: &Lease,
        step_id: &str,
        calls: &[UnitCallSpec],
        now: &Timestamp,
    ) -> Result<ExecUnit> {
        let ts = to_rfc3339(now);
        let calls_json = serde_json::to_string(calls).unwrap_or_else(|_| "[]".into());
        let seq = self.db.transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            let seq: i64 = c
                .query_row(
                    "SELECT COALESCE(MAX(unit_seq), 0) + 1 FROM task_units WHERE task_id = ?1",
                    params![task_id],
                    |r| r.get(0),
                )
                .db()?;
            c.execute(
                "INSERT INTO task_units(task_id, unit_seq, step_id, calls_json, status, model_calls, tool_calls, created_at, updated_at) VALUES(?1,?2,?3,?4,'pending',0,0,?5,?5)",
                params![task_id, seq, step_id, calls_json, ts],
            )
            .db()?;
            Ok(seq)
        })?;
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {UNIT_COLS} FROM task_units WHERE task_id = ?1 AND unit_seq = ?2"
            ))
            .db()?;
        st.query_row(params![task_id, seq], unit_row).db()
    }

    pub fn latest_unit(&self, task_id: &str, step_id: &str) -> Result<Option<ExecUnit>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {UNIT_COLS} FROM task_units WHERE task_id = ?1 AND step_id = ?2 ORDER BY unit_seq DESC LIMIT 1"
            ))
            .db()?;
        st.query_row(params![task_id, step_id], unit_row)
            .optional()
            .db()
    }

    /// Record one call's observation. `INSERT OR IGNORE` on the primary
    /// key makes a re-recorded result a no-op (returns false) — a
    /// worker that died after the dispatch but before the ack can safely
    /// record the same result again.
    /// Parameters are the result row's columns (task, unit, call,
    /// content, trust, time); no smaller grouping fits the callers.
    #[allow(clippy::too_many_arguments)]
    pub fn record_result(
        &self,
        task_id: &str,
        lease: &Lease,
        unit_seq: i64,
        call_id: &str,
        tool: &str,
        content: &str,
        trust: &str,
        now: &Timestamp,
    ) -> Result<bool> {
        let ts = to_rfc3339(now);
        let inserted = self.db.transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            let n = c
                .execute(
                    "INSERT OR IGNORE INTO task_unit_results(task_id, unit_seq, call_id, tool, content, trust, created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![task_id, unit_seq, call_id, tool, content, trust, ts],
                )
                .db()?;
            Ok(n)
        })?;
        Ok(inserted == 1)
    }

    /// Parameters are the unit's identity, its final status, and the
    /// turn's usage counts; see `record_result` for the same judgement.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_unit(
        &self,
        task_id: &str,
        lease: &Lease,
        unit_seq: i64,
        status: ExecStatus,
        model_calls: u64,
        tool_calls: u64,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        self.db.transaction(|c| {
            Self::guard(c, task_id, lease, &ts)?;
            c.execute(
                "UPDATE task_units SET status = ?3, model_calls = ?4, tool_calls = ?5, updated_at = ?6 WHERE task_id = ?1 AND unit_seq = ?2",
                params![task_id, unit_seq, status.as_str(), model_calls as i64, tool_calls as i64, ts],
            )
            .db()?;
            Ok(())
        })
    }
}
