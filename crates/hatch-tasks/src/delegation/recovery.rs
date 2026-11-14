//! Durable delegation, part 4: failure rules and cancel.

use super::consume::{load_context, save_context};
use super::types::{Delegation, DelegationStatus, FailureReport};
use super::DelegationStore;
use crate::events::insert_event;
use crate::task::Lease;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, DbResultExt};

impl<'a> DelegationStore<'a> {
    pub fn handle_failed(
        &self,
        parent_task: &str,
        lease: Option<&Lease>,
        now: &Timestamp,
    ) -> Result<FailureReport> {
        let mut report = FailureReport::default();
        for d in self.list_for_parent(parent_task)? {
            if d.status != DelegationStatus::Failed {
                continue;
            }
            match d.failure_kind.as_str() {
                "transient" => {
                    let child = crate::store::TaskStore::new(self.db)
                        .get(&d.child_task)?
                        .ok_or_else(|| {
                            Error::NotFound(format!("child {} not found", d.child_task))
                        })?;
                    if child.retries < child.max_retries {
                        let new = self.respawn_branch(parent_task, &d.step_id, now)?;
                        report.respawned.push(new.child_task);
                    } else {
                        report.failed_branches.push(d.step_id.clone());
                        self.record_gap(parent_task, &d, "暂时失败但重试已用完", lease, now)?;
                    }
                }
                "permanent" => {
                    report.failed_branches.push(d.step_id.clone());
                    self.record_gap(parent_task, &d, "永久失败", lease, now)?;
                }
                _ => {
                    report.blocked_reason = Some(format!(
                        "分支 {} 需人工介入：{}",
                        d.step_id,
                        d.result.as_ref().map(|r| r.summary.as_str()).unwrap_or("")
                    ));
                }
            }
        }
        Ok(report)
    }

    /// Record a failed branch as an open item in the parent context,
    /// so the parent can finish with partial results and the gap is
    /// visible instead of silent. Optionally fails the bound step
    /// when the caller holds the parent lease.
    fn record_gap(
        &self,
        parent_task: &str,
        delegation: &Delegation,
        reason: &str,
        lease: Option<&Lease>,
        now: &Timestamp,
    ) -> Result<()> {
        let ts = to_rfc3339(now);
        let parent = parent_task.to_string();
        let step = delegation.step_id.clone();
        let note = format!("分支 {step} 失败（{reason}），其余分支的结果已合并");
        let lease = lease.cloned();
        self.db.transaction(|c| {
            let (mut ctx, has_ctx) = load_context(c, &parent)?;
            if !ctx.open_items.contains(&note) {
                ctx.open_items.push(note.clone());
            }
            save_context(c, &ctx, has_ctx, &ts)?;
            insert_event(c, &parent, "child_failed_branch", &step, &note, &ts)?;
            if let Some(l) = &lease {
                let (owner, generation): (String, i64) = c
                    .query_row(
                        "SELECT lease_owner, lease_generation FROM tasks WHERE id = ?1",
                        params![parent],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .db()?;
                if owner == l.owner && generation == l.generation as i64 {
                    let changed = c.execute(
                        "UPDATE task_steps SET status = 'failed', output = ?3, updated_at = ?4 WHERE task_id = ?1 AND step_id = ?2 AND status != 'done'",
                        params![parent, step, note, ts],
                    )
                    .db()?;
                    if changed > 0 {
                        insert_event(c, &parent, "step_failed", &step, &note, &ts)?;
                    }
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Re-spawn one failed branch: the old delegation is superseded
    /// (kept for audit) and a fresh child takes the same step, kind,
    /// title and budget. Every other branch is untouched.
    pub fn respawn_branch(
        &self,
        parent_task: &str,
        step_id: &str,
        now: &Timestamp,
    ) -> Result<Delegation> {
        let old = self
            .live_for_step(parent_task, step_id)?
            .filter(|d| d.status == DelegationStatus::Failed)
            .or_else(|| {
                self.list_for_parent(parent_task).ok().and_then(|ds| {
                    ds.into_iter()
                        .find(|d| d.step_id == step_id && d.status == DelegationStatus::Failed)
                })
            })
            .ok_or_else(|| {
                Error::NotFound(format!("no failed delegation for {parent_task}/{step_id}"))
            })?;
        let child = crate::store::TaskStore::new(self.db)
            .get(&old.child_task)?
            .ok_or_else(|| Error::NotFound(format!("child {} not found", old.child_task)))?;
        let ts = to_rfc3339(now);
        self.db
            .conn()
            .execute(
                "UPDATE task_delegations SET status = 'superseded', updated_at = ?2 WHERE child_task = ?1",
                params![old.child_task, ts],
            )
            .db()?;
        self.spawn(
            parent_task,
            step_id,
            &child.kind,
            &child.title,
            &child.owner,
            child.max_model_calls,
            now,
        )
    }

    /// Cancel every non-terminal child of a parent, recursively, and
    /// record it on the parent. A child's receipts and step records
    /// are untouched — an external action already submitted keeps its
    /// receipt for reconciliation.
    pub fn propagate_cancel(
        &self,
        parent_task: &str,
        note: &str,
        now: &Timestamp,
    ) -> Result<Vec<String>> {
        let mut cancelled = Vec::new();
        let store = crate::store::TaskStore::new(self.db);
        for d in self.list_for_parent(parent_task)? {
            if d.status.is_terminal() {
                continue;
            }
            let Some(child) = store.get(&d.child_task)? else {
                continue;
            };
            if child.status.is_terminal() {
                continue;
            }
            // Recurse first: grandchildren stop before their parent.
            let mut sub = self.propagate_cancel(&d.child_task, note, now)?;
            cancelled.append(&mut sub);
            store.cancel(&d.child_task, note, now)?;
            let ts = to_rfc3339(now);
            self.db
                .conn()
                .execute(
                    "UPDATE task_delegations SET status = 'cancelled', updated_at = ?2 WHERE child_task = ?1",
                    params![d.child_task, ts],
                )
                .db()?;
            cancelled.push(d.child_task.clone());
        }
        if !cancelled.is_empty() {
            crate::events::EventLog::new(self.db).append_control_event(
                parent_task,
                "children_cancelled",
                "",
                &format!(
                    "已取消 {} 个子任务：{}",
                    cancelled.len(),
                    cancelled.join("、")
                ),
                now,
            )?;
        }
        Ok(cancelled)
    }
}
