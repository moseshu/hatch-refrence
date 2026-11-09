//! Pause / resume / fail transitions on [`PlanStore`].
//!
//! Kept separate from [`crate::transitions`] so each file stays small.
//! These are the P1 D1 additions to the state machine:
//!
//! * `blocked` — an executing plan paused while it waits on something
//!   external (an approval, the user, an external dependency). It resumes
//!   to `executing` when that arrives. It is NOT terminal, and it is NOT
//!   `needs_reapproval`: that state means the content changed and a fresh
//!   approval round is required. The resume paths differ, so the states
//!   stay apart.
//! * `failed` — the terminal state for a run that died irrecoverably.
//!   Distinct from a single failed step (`StepStatus::Failed`).
//! * `current_step` — the id of the step execution has reached. Step ids
//!   (not indices): an index goes stale the moment a step is inserted or
//!   removed before it, while an id either still exists or is detectably
//!   dangling — the runner falls back to the first unfinished step then.

use crate::plan::{Plan, PlanStatus};
use crate::store::PlanStore;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, DbResultExt};

impl<'a> PlanStore<'a> {
    /// Pause an executing plan while it waits on something external.
    /// Only `executing` can block — a plan that never started has
    /// nothing to pause. The reason is recorded as the decision note
    /// (same pattern as reject / cancel), so `show` names why it waits.
    /// `current_step` is kept: the run resumes at the same step.
    pub fn block(
        &self,
        id: &str,
        decided_by: &str,
        reason: &str,
        now: &Timestamp,
    ) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if plan.status != PlanStatus::Executing {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能暂停（只能暂停执行中的计划）",
                id,
                plan.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'blocked', decided_at = ?2, decided_by = ?3, decision_note = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, to_rfc3339(now), decided_by, reason, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_blocked",
            &format!("暂停计划（{decided_by}）：{reason}"),
        )?;
        Ok(self.get(id)?.unwrap())
    }

    /// Resume a blocked plan. It returns to `executing` — the thing it
    /// waited on has arrived, so no fresh approval round is needed.
    /// The decision note is left as the history of why it blocked.
    pub fn unblock(&self, id: &str, now: &Timestamp) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if plan.status != PlanStatus::Blocked {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，没有在暂停，不能恢复",
                id,
                plan.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'executing', updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.db
            .audit(&format!("plan:{id}"), "plan_unblocked", "暂停结束，恢复执行")?;
        Ok(self.get(id)?.unwrap())
    }

    /// Fail a plan irrecoverably. Only `executing` and `blocked` can
    /// fail — both are in-flight; a plan that never started cannot fail,
    /// and a terminal plan cannot fail twice. `current_step` is kept: it
    /// names the step where the run died.
    pub fn fail(
        &self,
        id: &str,
        decided_by: &str,
        reason: &str,
        now: &Timestamp,
    ) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if !matches!(plan.status, PlanStatus::Executing | PlanStatus::Blocked) {
            return Err(Error::Invalid(format!(
                "plan {} 处于 {}，不能标记失败（只能让执行中或暂停中的计划失败）",
                id,
                plan.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET status = 'failed', decided_at = ?2, decided_by = ?3, decision_note = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, to_rfc3339(now), decided_by, reason, to_rfc3339(now)],
            )
            .db()?;
        self.db.audit(
            &format!("plan:{id}"),
            "plan_failed",
            &format!("计划失败（{decided_by}）：{reason}"),
        )?;
        Ok(self.get(id)?.unwrap())
    }

    /// Point the plan at the step execution has reached (`None` clears
    /// it — a finished plan has no current step). The id must be a step
    /// of this plan; an unknown id is an error, never a silent dangle.
    pub fn set_current_step(
        &self,
        id: &str,
        step_id: Option<&str>,
        now: &Timestamp,
    ) -> Result<Plan> {
        let plan = self
            .get(id)?
            .ok_or_else(|| Error::NotFound(format!("plan {id} 不存在")))?;
        if let Some(sid) = step_id {
            if !plan.steps.iter().any(|s| s.id == sid) {
                return Err(Error::NotFound(format!("plan {id} 中没有步骤 {sid}")));
            }
        }
        self.db
            .conn()
            .execute(
                "UPDATE plans SET current_step = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, step_id.unwrap_or(""), to_rfc3339(now)],
            )
            .db()?;
        Ok(self.get(id)?.unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanStep, RiskLevel, StepStatus};
    use hatch_core::time::now;
    use hatch_store::Db;
    use serde_json::json;
    use std::sync::Arc;

    fn step(id: &str) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: "t".into(),
            tool: "memory_search".into(),
            arguments: json!({"query": "x"}),
            files: vec![],
            expected: "ok".into(),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }
    }

    /// Approve the way the product does: decide the plan's pending queue
    /// entry, then execute the decision (the executor applies it).
    fn approve_via_queue(db: &Db, plan_id: &str, now: &Timestamp) -> Plan {
        let entry = crate::pending_plan_approval(db, plan_id)
            .unwrap()
            .unwrap();
        hatch_policy::ApprovalQueue::new(db)
            .decide(&entry.id, true, "ok", now)
            .unwrap();
        let exec = hatch_tools::ExecutorRegistry::new().with(Arc::new(
            crate::PlanApproveExecutor::new("test"),
        ));
        hatch_tools::execute_approved(db, &exec, &entry.id, now).unwrap();
        PlanStore::new(db).get(plan_id).unwrap().unwrap()
    }

    fn executing_plan(db: &Db, now: &Timestamp) -> Plan {
        let store = PlanStore::new(db);
        let plan = store
            .create("做 X", "", "m", vec![step("s1"), step("s2")], false, now)
            .unwrap();
        let plan = approve_via_queue(db, &plan.id, now);
        assert_eq!(plan.status, PlanStatus::Approved);
        store.set_running(&plan.id, true, now).unwrap();
        store.get(&plan.id).unwrap().unwrap()
    }

    #[test]
    fn block_unblock_roundtrip_and_cancel_from_blocked() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();
        let plan = executing_plan(&db, &now);
        assert_eq!(plan.status, PlanStatus::Executing);

        let plan = store.block(&plan.id, "cli", "等用户确认", &now).unwrap();
        assert_eq!(plan.status, PlanStatus::Blocked);
        assert_eq!(plan.decision_note, "等用户确认");
        assert!(!plan.status.is_terminal());

        // Blocking twice is an error.
        assert!(store.block(&plan.id, "cli", "再等", &now).is_err());

        let plan = store.unblock(&plan.id, &now).unwrap();
        assert_eq!(plan.status, PlanStatus::Executing);

        // Block again, then cancel from blocked is allowed.
        let plan = store.block(&plan.id, "cli", "等外部依赖", &now).unwrap();
        let plan = store.cancel(&plan.id, "cli", "不等了", &now).unwrap();
        assert_eq!(plan.status, PlanStatus::Cancelled);
        assert!(plan.status.is_terminal());
    }

    #[test]
    fn block_illegal_states() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();

        // Draft cannot block: nothing is running.
        let draft = store
            .create("做 X", "", "m", vec![step("s1")], true, &now)
            .unwrap();
        assert!(store.block(&draft.id, "cli", "等", &now).is_err());

        // Approved-but-not-running cannot block either.
        let plan = store
            .create("做 Y", "", "m", vec![step("s1")], false, &now)
            .unwrap();
        let plan = approve_via_queue(&db, &plan.id, &now);
        assert_eq!(plan.status, PlanStatus::Approved);
        assert!(store.block(&plan.id, "cli", "等", &now).is_err());

        // Terminal plans cannot block.
        let plan = executing_plan(&db, &now);
        store.mark_completed(&plan.id, &now).unwrap();
        assert!(store.block(&plan.id, "cli", "等", &now).is_err());

        // Unblocking a plan that never blocked is an error.
        let plan = executing_plan(&db, &now);
        assert!(store.unblock(&plan.id, &now).is_err());
    }

    #[test]
    fn fail_from_executing_and_blocked() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();

        let plan = executing_plan(&db, &now);
        let plan = store.fail(&plan.id, "cli", "工具连环报错", &now).unwrap();
        assert_eq!(plan.status, PlanStatus::Failed);
        assert!(plan.status.is_terminal());
        assert_eq!(plan.decision_note, "工具连环报错");

        // Blocked can fail too (the thing it waited on died).
        let plan = executing_plan(&db, &now);
        let plan = store.block(&plan.id, "cli", "等外部服务", &now).unwrap();
        let plan = store.fail(&plan.id, "cli", "外部服务挂了", &now).unwrap();
        assert_eq!(plan.status, PlanStatus::Failed);

        // A failed plan cannot fail, block, or be cancelled again.
        assert!(store.fail(&plan.id, "cli", "再败", &now).is_err());
        assert!(store.block(&plan.id, "cli", "等", &now).is_err());
        assert!(store.cancel(&plan.id, "cli", "取消", &now).is_err());
    }

    #[test]
    fn fail_illegal_states() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();

        let draft = store
            .create("做 X", "", "m", vec![step("s1")], true, &now)
            .unwrap();
        assert!(store.fail(&draft.id, "cli", "败", &now).is_err());

        let plan = store
            .create("做 Y", "", "m", vec![step("s1")], false, &now)
            .unwrap();
        let plan = approve_via_queue(&db, &plan.id, &now);
        assert!(store.fail(&plan.id, "cli", "败", &now).is_err());
    }

    #[test]
    fn current_step_set_clear_and_unknown_rejected() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();
        let plan = executing_plan(&db, &now);
        assert_eq!(plan.current_step, None);

        let plan = store.set_current_step(&plan.id, Some("s2"), &now).unwrap();
        assert_eq!(plan.current_step.as_deref(), Some("s2"));

        // Unknown step id is an error, never a silent dangle.
        assert!(store
            .set_current_step(&plan.id, Some("s9"), &now)
            .is_err());
        let plan = store.get(&plan.id).unwrap().unwrap();
        assert_eq!(plan.current_step.as_deref(), Some("s2"));

        let plan = store.set_current_step(&plan.id, None, &now).unwrap();
        assert_eq!(plan.current_step, None);
    }

    #[test]
    fn completing_clears_current_step() {
        let db = Db::open_in_memory().unwrap();
        let store = PlanStore::new(&db);
        let now = now();
        let plan = executing_plan(&db, &now);
        store.set_current_step(&plan.id, Some("s1"), &now).unwrap();
        store.mark_completed(&plan.id, &now).unwrap();
        let plan = store.get(&plan.id).unwrap().unwrap();
        assert_eq!(plan.status, PlanStatus::Completed);
        assert_eq!(plan.current_step, None);
    }
}
