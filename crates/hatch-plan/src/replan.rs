//! Local replanning (task 6): when a resource version changes, redo
//! only the affected branch.
//!
//! A replan takes an approved (or executing) plan, a version the
//! caller expected, and the steps whose inputs went stale, and
//! commits a new plan version in which:
//!
//! * affected steps are reset to `pending` (a done step keeps its old
//!   output in the ledger for audit, but its row is cleared so the
//!   next run re-executes it — with the new plan version the
//!   idempotency key is fresh, so the redo is a new action, not a
//!   replay of the old receipt);
//! * unaffected steps are left entirely alone — a done step keeps its
//!   status, output and receipt, and the executor skips it, so no
//!   completed side effect ever runs twice because of a replan;
//! * the approved snapshot is cleared and the plan returns to
//!   `pending_approval`, so the new version needs a fresh approval
//!   round (the §10.2 rule: a changed plan is never executed on the
//!   old approval).
//!
//! The diff names every step (changed and unchanged), so the caller
//! can show exactly what moved. The goal is never touched by a
//! replan.
//!
//! Caller contract (the plan half lives here, the task half in the
//! control plane): after `replan_local` bumps the *plan* version, the
//! caller must bump the *task* version to match and invalidate the
//! older approvals (`hatch-cli` `task replan` does this). The budget
//! is never touched here — the task's `max_model_calls` and the used
//! count carry over unchanged, so the replanned branches inherit the
//! remainder.

use crate::plan::{PlanStep, StepEdit, StepStatus};
use crate::store::PlanStore;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_store::Db;

/// One step whose inputs went stale. `edit` replaces the step
/// definition (tool / arguments / files / expected); `None` keeps the
/// definition and only resets the step to run again.
#[derive(Clone, Debug)]
pub struct ReplanStep {
    pub step_id: String,
    pub edit: Option<StepEdit>,
}

/// How one step changed in the replan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Untouched: status, output and receipt preserved.
    Unchanged,
    /// Was done (or failed / waiting) and is pending again.
    Reset,
    /// Definition replaced and pending again.
    Edited,
}

impl ChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeKind::Unchanged => "unchanged",
            ChangeKind::Reset => "reset",
            ChangeKind::Edited => "edited",
        }
    }
}

/// What happened to one step in the replan.
#[derive(Clone, Debug)]
pub struct StepChange {
    pub step_id: String,
    pub change: ChangeKind,
    /// One line: what the step was, and what it is now.
    pub detail: String,
}

/// The replan diff: every step, changed and unchanged.
#[derive(Clone, Debug)]
pub struct PlanDiff {
    pub plan_id: String,
    pub from_version: u64,
    pub to_version: u64,
    pub reason: String,
    pub changes: Vec<StepChange>,
}

impl PlanDiff {
    /// Steps that will run again (reset or edited).
    pub fn changed_ids(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|c| c.change != ChangeKind::Unchanged)
            .map(|c| c.step_id.as_str())
            .collect()
    }

    /// Steps left exactly as they were.
    pub fn unchanged_ids(&self) -> Vec<&str> {
        self.changes
            .iter()
            .filter(|c| c.change == ChangeKind::Unchanged)
            .map(|c| c.step_id.as_str())
            .collect()
    }

    pub fn reset_count(&self) -> usize {
        self.changed_ids().len()
    }

    /// One line per step, for the control-plane event and the CLI.
    pub fn describe(&self) -> String {
        let mut out = format!(
            "计划 {} 重规划 v{} → v{}（{}）：",
            self.plan_id, self.from_version, self.to_version, self.reason
        );
        for c in &self.changes {
            let mark = match c.change {
                ChangeKind::Unchanged => "保持",
                ChangeKind::Reset => "重做",
                ChangeKind::Edited => "改定义并重做",
            };
            out.push_str(&format!("\n  {} {}：{}", mark, c.step_id, c.detail));
        }
        out
    }
}

/// Local replan: reset only the affected steps of a plan, commit a
/// new version, and return the diff. The plan must not be terminal,
/// and its version must be the one the caller expected — a stale
/// caller is refused before anything moves.
pub fn replan_local(
    db: &Db,
    plan_id: &str,
    expected_version: u64,
    affected: &[ReplanStep],
    reason: &str,
    now: &Timestamp,
) -> Result<PlanDiff> {
    let store = PlanStore::new(db);
    let plan = store
        .get(plan_id)?
        .ok_or_else(|| Error::NotFound(format!("plan {plan_id} 不存在")))?;
    if plan.status.is_terminal() {
        return Err(Error::Invalid(format!(
            "plan {} 已是终态 {}，不能重规划",
            plan_id,
            plan.status.as_str()
        )));
    }
    if plan.version != expected_version {
        return Err(Error::Invalid(format!(
            "plan {} 的版本已是 v{}，不是期望的 v{}（版本已推进，指令过期）",
            plan_id, plan.version, expected_version
        )));
    }
    for a in affected {
        if !plan.steps.iter().any(|s| s.id == a.step_id) {
            return Err(Error::NotFound(format!(
                "plan {plan_id} 中没有步骤 {}",
                a.step_id
            )));
        }
    }
    // The version bump is conditional on the expected version, so two
    // replans racing on the same version cannot both win.
    let bumped = store.bump_version(plan_id, expected_version, now)?;
    let to_version = bumped.version;
    let mut changes: Vec<StepChange> = Vec::new();
    let mut first_reset: Option<String> = None;
    for (idx, step) in plan.steps.iter().enumerate() {
        let hit = affected.iter().find(|a| a.step_id == step.id);
        match hit {
            None => {
                changes.push(StepChange {
                    step_id: step.id.clone(),
                    change: ChangeKind::Unchanged,
                    detail: step_kept_detail(step),
                });
            }
            Some(a) => {
                let was = step.status;
                let edited = a.edit.is_some();
                store.reset_step(plan_id, idx, a.edit.as_ref(), now)?;
                if first_reset.is_none() {
                    first_reset = Some(step.id.clone());
                }
                changes.push(StepChange {
                    step_id: step.id.clone(),
                    change: if edited {
                        ChangeKind::Edited
                    } else {
                        ChangeKind::Reset
                    },
                    detail: format!(
                        "原状态 {}，{}重做（旧输出保留在事件中）",
                        was.as_str(),
                        if edited { "定义已更新，" } else { "" }
                    ),
                });
            }
        }
    }
    // The new version has no approved snapshot: it needs a fresh
    // approval round before it runs (the §10.2 rule, same as an edit).
    // Submit the entry so the new version is actually approvable.
    store.clear_approval_snapshot(plan_id, now)?;
    let fresh = store
        .get(plan_id)?
        .ok_or_else(|| Error::NotFound(format!("plan {plan_id} 不存在")))?;
    crate::approval::submit_plan_approval(db, &fresh, now)?;
    // Point the current step at the first step that will run again.
    if let Some(first) = first_reset {
        store.set_current_step(plan_id, Some(&first), now)?;
    }
    Ok(PlanDiff {
        plan_id: plan_id.to_string(),
        from_version: expected_version,
        to_version,
        reason: reason.to_string(),
        changes,
    })
}

fn step_kept_detail(step: &PlanStep) -> String {
    match step.status {
        StepStatus::Done => "已完成，回执保留，本次不动".to_string(),
        StepStatus::Pending => "未执行，本次不动".to_string(),
        StepStatus::Failed => "已失败，本次不动（失败原因见步骤记录）".to_string(),
        StepStatus::AwaitingApproval => "等待审批中，本次不动".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanStatus, PlanStep, RiskLevel};
    use hatch_store::Db;
    use serde_json::json;

    fn now() -> Timestamp {
        hatch_core::time::now()
    }

    fn step(id: &str, tool: &str) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: format!("步骤 {id}"),
            tool: tool.into(),
            arguments: json!({}),
            files: vec![],
            expected: format!("完成 {id}"),
            risk: RiskLevel::Low,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }
    }

    fn make_plan(db: &Db) -> String {
        let store = PlanStore::new(db);
        let plan = store
            .create(
                "做点事",
                "做点事",
                "",
                vec![
                    step("s1", "fs_read"),
                    step("s2", "fs_read"),
                    step("s3", "fs_read"),
                ],
                false,
                &now(),
            )
            .unwrap();
        store.set_goal(&plan.id, "做点事", &now()).unwrap();
        plan.id
    }

    #[test]
    fn replan_resets_only_affected_steps() {
        let db = Db::open_in_memory().unwrap();
        let id = make_plan(&db);
        let store = PlanStore::new(&db);
        // s1 and s2 are done (with outputs); s3 is still pending.
        store
            .record_step(&id, 0, StepStatus::Done, "s1 的结果", None, &now())
            .unwrap();
        store
            .record_step(&id, 1, StepStatus::Done, "s2 的结果", None, &now())
            .unwrap();
        let diff = replan_local(
            &db,
            &id,
            1,
            &[ReplanStep {
                step_id: "s2".into(),
                edit: None,
            }],
            "s2 读的资源版本变了",
            &now(),
        )
        .unwrap();
        assert_eq!(diff.from_version, 1);
        assert_eq!(diff.to_version, 2);
        assert_eq!(diff.changed_ids(), vec!["s2"]);
        assert_eq!(diff.unchanged_ids(), vec!["s1", "s3"]);
        assert_eq!(diff.reset_count(), 1);
        let plan = store.get(&id).unwrap().unwrap();
        assert_eq!(plan.version, 2);
        assert_eq!(plan.goal, "做点事", "重规划不得改目标");
        assert_eq!(plan.steps[0].status, StepStatus::Done);
        assert_eq!(
            plan.steps[0].output, "s1 的结果",
            "未受影响的已完成步骤原样不动"
        );
        assert_eq!(plan.steps[1].status, StepStatus::Pending);
        assert_eq!(plan.steps[1].output, "", "重做的步骤输出已清");
        assert_eq!(plan.steps[2].status, StepStatus::Pending);
        assert_eq!(plan.status, PlanStatus::PendingApproval, "新版本需重新批准");
        assert!(plan.approved_steps_json.is_empty(), "旧批准快照已清");
    }

    #[test]
    fn replan_with_new_definition_edits_the_step() {
        let db = Db::open_in_memory().unwrap();
        let id = make_plan(&db);
        let diff = replan_local(
            &db,
            &id,
            1,
            &[ReplanStep {
                step_id: "s1".into(),
                edit: Some(StepEdit {
                    tool: Some("fs_list".into()),
                    ..Default::default()
                }),
            }],
            "换个工具重读",
            &now(),
        )
        .unwrap();
        assert_eq!(diff.changes[0].change, ChangeKind::Edited);
        let plan = PlanStore::new(&db).get(&id).unwrap().unwrap();
        assert_eq!(plan.steps[0].tool, "fs_list");
        assert_eq!(plan.steps[0].status, StepStatus::Pending);
    }

    #[test]
    fn replan_refuses_stale_version_and_unknown_steps() {
        let db = Db::open_in_memory().unwrap();
        let id = make_plan(&db);
        assert!(replan_local(&db, &id, 9, &[], "x", &now()).is_err());
        assert!(replan_local(
            &db,
            &id,
            1,
            &[ReplanStep {
                step_id: "sX".into(),
                edit: None
            }],
            "x",
            &now(),
        )
        .is_err());
    }
}
