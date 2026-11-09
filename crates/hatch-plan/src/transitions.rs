//! State transitions on [`PlanStore`].
//!
//! Kept separate from [`crate::store`] so each file stays small. These are
//! the methods that change a plan's status: approve / reject / cancel /
//! edit (which re-requires approval) / deviation flagging / run bookkeeping.

use crate::plan::{Plan, PlanStatus, StepEdit, StepStatus};
use crate::store::PlanStore;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_policy::ApprovalStatus;
use hatch_store::{params, DbResultExt};

impl<'a> PlanStore<'a> {
    fn set_status(&self, id: &str, status: PlanStatus, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status.as_str(), to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }

    /// Approve a plan: only from `pending_approval` or `needs_reapproval`.
    /// Snapshots the exact steps approved and records who decided and when.
    /// The approval queue is the source of truth — a `plan_approve` entry
    /// for exactly this plan version must already be decided `Approved`
    /// (see [`crate::approval`]); without it this refuses. The approval is
    /// a distinct, auditable decision — it is also written to the audit log.
    pub fn approve(&self, id: &str, decided_by: &str, note: &str, now: &Timestamp) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if !matches!(
            plan.status,
            PlanStatus::PendingApproval | PlanStatus::NeedsReapproval
        ) {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能批准（只能批准待审批或需重新审批的计划）",
                id,
                plan.status.as_str()
            )));
        }
        // The approval queue is the single source of truth: the newest
        // entry for exactly this plan version must be decided `Approved`.
        // A direct call without a decided entry fails — a plan cannot be
        // approved by flipping the plan table alone.
        let entry = crate::approval::latest_plan_approval(self.db, &plan)?.ok_or_else(|| {
            Error::Invalid(format!(
                "plan {id} 没有审批队列记录，不能批准；请先提交审批"
            ))
        })?;
        match entry.status {
            // Executed means the decision was Approved and the executor
            // already applied it (the plan-status check above normally
            // rejects this call first — this arm only keeps the match
            // honest about what "decided Approved" covers).
            ApprovalStatus::Approved | ApprovalStatus::Executed => {}
            ApprovalStatus::Pending => {
                return Err(Error::Invalid(format!(
                    "plan {id} 的审批 {} 尚未通过，不能批准",
                    entry.id
                )))
            }
            ApprovalStatus::Rejected | ApprovalStatus::Expired | ApprovalStatus::Failed => {
                return Err(Error::Invalid(format!(
                    "plan {id} 的审批 {} 已{}，批准失效；请重新提交审批",
                    entry.id,
                    match entry.status {
                        ApprovalStatus::Rejected => "拒绝",
                        ApprovalStatus::Expired => "过期",
                        _ => "执行失败",
                    }
                )))
            }
        }
        let snapshot = Plan::steps_json(&plan.steps);
        let hash = Plan::steps_hash(&plan.steps);
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'approved', decided_at = ?2, decided_by = ?3, decision_note = ?4, approved_steps_json = ?5, approved_hash = ?6, deviation = '', updated_at = ?7 WHERE id = ?1",
                params![id, to_rfc3339(now), decided_by, note, snapshot, hash, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_approved",
            &format!("批准计划（{decided_by}）：{}", plan.task),
        )?;
        Ok(self.get(id)?.unwrap())
    }

    /// Reject a plan (from draft / pending / needs-reapproval / approved).
    pub fn reject(&self, id: &str, decided_by: &str, note: &str, now: &Timestamp) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if !matches!(
            plan.status,
            PlanStatus::Draft
                | PlanStatus::PendingApproval
                | PlanStatus::NeedsReapproval
                | PlanStatus::Approved
        ) {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能拒绝",
                id,
                plan.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'rejected', decided_at = ?2, decided_by = ?3, decision_note = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, to_rfc3339(now), decided_by, note, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_rejected",
            &format!("拒绝计划（{decided_by}）：{}", plan.task),
        )?;
        Ok(self.get(id)?.unwrap())
    }

    /// Cancel a plan (from draft / pending / approved / needs-reapproval /
    /// executing). A cancelled plan never runs.
    pub fn cancel(&self, id: &str, decided_by: &str, note: &str, now: &Timestamp) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if plan.status.is_terminal() {
            return Err(Error::Invalid(format!(
                "plan {} 已是终态 {}，不能取消",
                id,
                plan.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'cancelled', decided_at = ?2, decided_by = ?3, decision_note = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, to_rfc3339(now), decided_by, note, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_cancelled",
            &format!("取消计划（{decided_by}）：{}", plan.task),
        )?;
        Ok(self.get(id)?.unwrap())
    }

    /// Edit one step. An approved / needs-reapproval plan drops back to
    /// `pending_approval` and its approved snapshot is cleared — editing
    /// always re-requires approval. A draft stays a draft.
    pub fn edit_step(
        &self,
        id: &str,
        step_id: &str,
        edit: StepEdit,
        now: &Timestamp,
    ) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if matches!(plan.status, PlanStatus::Executing | PlanStatus::Completed) {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能编辑（先取消）",
                id,
                plan.status.as_str()
            )));
        }
        if plan.status.is_terminal() {
            return Err(Error::Invalid(format!(
                "plan {} 已是终态 {}，不能编辑",
                id,
                plan.status.as_str()
            )));
        }
        let idx = plan
            .steps
            .iter()
            .position(|s| s.id == step_id)
            .ok_or_else(|| Error::NotFound(format!("plan {id} 中没有步骤 {step_id}")))?;
        let mut step = plan.steps[idx].clone();
        if let Some(t) = edit.tool {
            step.tool = t;
        }
        if let Some(a) = edit.arguments {
            step.arguments = a;
        }
        if let Some(f) = edit.files {
            step.files = f;
        }
        if let Some(t) = edit.title {
            step.title = t;
        }
        if let Some(e) = edit.expected {
            step.expected = e;
        }
        if let Some(r) = edit.risk {
            step.risk = r;
        }
        self.db
            .conn()
            .execute(
                "UPDATE plan_steps SET title = ?3, tool = ?4, arguments = ?5, files = ?6, expected = ?7, risk = ?8, updated_at = ?9 WHERE plan_id = ?1 AND idx = ?2",
                params![
                    id,
                    idx as i64,
                    step.title,
                    step.tool,
                    step.arguments.to_string(),
                    serde_json::to_string(&step.files).unwrap_or_default(),
                    step.expected,
                    step.risk.as_str(),
                    to_rfc3339(now),
                ],
            )
            .db()?;
        let new_status = match plan.status {
            PlanStatus::Draft => PlanStatus::Draft,
            _ => PlanStatus::PendingApproval,
        };
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = ?2, approved_steps_json = '', approved_hash = '', deviation = '', updated_at = ?3 WHERE id = ?1",
                params![id, new_status.as_str(), to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_edited",
            &format!("编辑步骤 {step_id}，计划回到 {}", new_status.as_str()),
        )?;
        // Edited plans re-enter the queue: approving the old version must
        // not approve the new one. Submitting a fresh entry rejects the
        // previous pending entry for this plan. A draft stays a draft and
        // submits nothing.
        if new_status == PlanStatus::PendingApproval {
            let plan = self.get(id)?.unwrap();
            crate::approval::submit_plan_approval(self.db, &plan, now)?;
        }
        Ok(self.get(id)?.unwrap())
    }

    /// Steps of a plan as stored at approval time (canonical JSON).
    pub fn approved_steps(&self, plan: &Plan) -> Result<Vec<crate::plan::PlanStep>> {
        if plan.approved_steps_json.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<String> = serde_json::from_str(&plan.approved_steps_json)
            .map_err(|e| Error::Storage(format!("plan {} 的批准快照损坏：{e}", plan.id)))?;
        raw.iter()
            .map(|s| {
                serde_json::from_str(s)
                    .map_err(|e| Error::Storage(format!("plan {} 的批准快照损坏：{e}", plan.id)))
            })
            .collect()
    }

    /// Mark a plan as needing re-approval, with the deviation recorded.
    pub fn flag_deviation(&self, id: &str, deviation: &str, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'needs_reapproval', deviation = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, deviation, to_rfc3339(now)],
            )
            .db()?;
        self.db
            .audit(&format!("plan:{id}"), "plan_deviation", deviation)?;
        // Deviation needs a fresh human decision: submit the re-approval
        // entry now (rejecting any pending entry for this plan).
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::Storage(format!("plan {id} vanished after flag")))?;
        crate::approval::submit_plan_approval(self.db, &plan, now)?;
        Ok(())
    }

    /// Move an approved plan into `executing` (run start) or back to
    /// `approved` (run paused because a step needs a gate approval).
    pub fn set_running(&self, id: &str, running: bool, now: &Timestamp) -> Result<()> {
        self.set_status(
            id,
            if running {
                PlanStatus::Executing
            } else {
                PlanStatus::Approved
            },
            now,
        )
    }

    /// Mark a plan completed. A finished plan has no current step.
    pub fn mark_completed(&self, id: &str, now: &Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'completed', current_step = '', updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_completed",
            "计划全部步骤执行完毕",
        )?;
        Ok(())
    }

    /// Record one step's outcome. `output` is stored verbatim (callers must
    /// not put secrets in tool output — dispatch already redacts audit
    /// details; step output is the tool's own result text).
    pub fn record_step(
        &self,
        plan_id: &str,
        idx: usize,
        status: StepStatus,
        output: &str,
        approval_id: Option<&str>,
        now: &Timestamp,
    ) -> Result<()> {
        self.db
            .conn()
            .execute(
                "UPDATE plan_steps SET status = ?3, output = ?4, approval_id = ?5, updated_at = ?6 WHERE plan_id = ?1 AND idx = ?2",
                params![plan_id, idx as i64, status.as_str(), output, approval_id.unwrap_or(""), to_rfc3339(now)],
            )
            .db()?;
        Ok(())
    }
}
