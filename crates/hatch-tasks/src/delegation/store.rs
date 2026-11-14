//! Durable delegation, part 2: spawning and reporting.

use super::types::{delegation_row, ChildResult, Delegation, DELEGATION_COLS};
use crate::events::insert_event;
use hatch_core::ids;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

pub struct DelegationStore<'a> {
    pub(crate) db: &'a Db,
}

impl<'a> DelegationStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        DelegationStore { db }
    }

    /// Spawn a child task for one parent step. Idempotent on
    /// (parent, step): a live delegation for the step returns the
    /// existing child instead of a second one. The child is a normal
    /// task row (parent_task set, its own budget) and the delegation
    /// row, committed together with the parent's `child_delegated`
    /// event — a crash cannot leave a child without its binding.
    pub fn spawn(
        &self,
        parent_task: &str,
        step_id: &str,
        kind: &str,
        title: &str,
        owner: &str,
        budget: u64,
        now: &Timestamp,
    ) -> Result<Delegation> {
        if let Some(live) = self.live_for_step(parent_task, step_id)? {
            return Ok(live);
        }
        let ts = to_rfc3339(now);
        let child_id = ids::random_id("tsk");
        let key = format!("delegation:{parent_task}:{step_id}:{child_id}");
        let title = title.to_string();
        let kind = kind.to_string();
        let owner = owner.to_string();
        let step_id = step_id.to_string();
        let parent_task = parent_task.to_string();
        let spawned = self.db.transaction(|c| {
            c.execute(
                "INSERT INTO tasks(id, kind, title, owner, parent_task, status, retries, max_retries, last_heartbeat, idempotency_key, error, created_at, updated_at, finished_at, max_model_calls) VALUES(?1,?2,?3,?4,?5,'queued',0,3,?6,?7,'',?6,?6,NULL,?8)",
                params![child_id, kind, title, owner, parent_task, ts, key, budget as i64],
            )
            .db()?;
            c.execute(
                "INSERT INTO task_delegations(parent_task, child_task, step_id, status, result_json, failure_kind, consumed_at, created_at, updated_at) VALUES(?1,?2,?3,'delegated','','','',?4,?4)",
                params![parent_task, child_id, step_id, ts],
            )
            .db()?;
            insert_event(
                c,
                &parent_task,
                "child_delegated",
                &step_id,
                &format!("子任务 {child_id} 已派生（{title}）"),
                &ts,
            )?;
            Ok(())
        });
        if let Err(e) = spawned {
            // A concurrent spawn for the same step won the race (the
            // live-step index refused ours): return the winner.
            if let Some(live) = self.live_for_step(&parent_task, &step_id)? {
                return Ok(live);
            }
            return Err(e);
        }
        self.get_by_child(&child_id)?
            .ok_or_else(|| Error::NotFound(format!("delegation for {child_id} not found")))
    }

    /// The live delegation for a parent step, if any (delegated /
    /// running / done / consumed — a failed, cancelled or superseded
    /// one leaves the step free for a re-spawn).
    pub fn live_for_step(&self, parent_task: &str, step_id: &str) -> Result<Option<Delegation>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                &format!("SELECT {DELEGATION_COLS} FROM task_delegations WHERE parent_task = ?1 AND step_id = ?2 AND status IN ('delegated','running','done','consumed') ORDER BY created_at DESC LIMIT 1"),
                params![parent_task, step_id],
                delegation_row,
            )
            .optional()
            .db()?;
        Ok(row)
    }

    pub fn get_by_child(&self, child_task: &str) -> Result<Option<Delegation>> {
        let c = self.db.conn();
        let row = c
            .query_row(
                &format!("SELECT {DELEGATION_COLS} FROM task_delegations WHERE child_task = ?1"),
                params![child_task],
                delegation_row,
            )
            .optional()
            .db()?;
        Ok(row)
    }

    pub fn list_for_parent(&self, parent_task: &str) -> Result<Vec<Delegation>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!(
                "SELECT {DELEGATION_COLS} FROM task_delegations WHERE parent_task = ?1 ORDER BY created_at"
            ))
            .db()?;
        let rows = st.query_map(params![parent_task], delegation_row).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// A child reports its structured result. The delegation moves to
    /// done (or failed, when `failure_kind` is set) and the parent
    /// gets a `child_completed` / `child_failed` event; a parent
    /// blocked waiting on children is woken to `waiting` so the next
    /// run can consume. One transaction: the report, the wake-up and
    /// the event commit together, so a kill after the handoff cannot
    /// leave the parent asleep with a result in hand.
    pub fn report_result(
        &self,
        child_task: &str,
        result: &ChildResult,
        failure_kind: &str,
        now: &Timestamp,
    ) -> Result<Delegation> {
        let delegation = self
            .get_by_child(child_task)?
            .ok_or_else(|| Error::NotFound(format!("no delegation for child {child_task}")))?;
        if delegation.status.is_terminal() {
            return Err(Error::Invalid(format!(
                "delegation for {child_task} is already {}",
                delegation.status.as_str()
            )));
        }
        let ts = to_rfc3339(now);
        let result_json = serde_json::to_string(result).unwrap_or_else(|_| "{}".into());
        let status = if failure_kind.is_empty() {
            "done"
        } else {
            "failed"
        };
        let kind = if failure_kind.is_empty() {
            "child_completed"
        } else {
            "child_failed"
        };
        let parent = delegation.parent_task.clone();
        let step = delegation.step_id.clone();
        let summary = result.summary.clone();
        let failure_kind = failure_kind.to_string();
        self.db.transaction(|c| {
            c.execute(
                "UPDATE task_delegations SET status = ?2, result_json = ?3, failure_kind = ?4, updated_at = ?5 WHERE child_task = ?1",
                params![child_task, status, result_json, failure_kind, ts],
            )
            .db()?;
            insert_event(c, &parent, kind, &step, &summary, &ts)?;
            // Wake the parent: it was blocked on its children, and
            // one just reported. `waiting` means the next run claims
            // it; the conditional UPDATE refuses if the parent moved
            // on (cancelled, or already woken by another child).
            c.execute(
                "UPDATE tasks SET status = 'waiting', blocked_kind = '', error = '', updated_at = ?2 WHERE id = ?1 AND status = 'blocked' AND blocked_kind = 'children'",
                params![parent, ts],
            )
            .db()?;
            Ok(())
        })?;
        self.get_by_child(child_task)?
            .ok_or_else(|| Error::NotFound(format!("delegation for {child_task} not found")))
    }
}
