//! Drills for task 6 (long-task context + local replanning):
//!
//! (b) a resource version change redoes only the affected branch.
//! (The compression-survival half lives in `task_compression_drill`;
//! the failure-classification half in `task_failure_drill`.)
//!
//! Everything runs on the scripted tools / test doubles: no live
//! model, no real mail.

#![cfg(test)]

use super::task_replan::replan_task;
use hatch_core::time::Timestamp;
use hatch_plan::{
    latest_plan_approval, PlanApproveExecutor, PlanStatus, PlanStep, PlanStore, RiskLevel,
    StepStatus,
};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, NewApproval};
use hatch_store::Db;
use hatch_tasks::{
    EvidenceRef, ExecStatus, StepSpec, TaskApprovals, TaskContext, TaskContextStore, TaskStore,
    UnitStore,
};
use hatch_tools::{execute_approved, ExecutorRegistry};
use serde_json::json;
use std::sync::Arc;

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
        expected: "完成".into(),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

fn step_spec(id: &str, tool: &str) -> StepSpec {
    StepSpec {
        step_id: id.into(),
        title: format!("步骤 {id}"),
        tool: tool.into(),
        arguments: json!({}),
        expected: "完成".into(),
    }
}

/// Approve a plan through the queue (the `hatch plan approve` path,
/// in-process): decide the submitted entry, then execute it.
fn approve_plan(db: &Db, plan_id: &str) {
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("计划应有待审批记录");
    let status = ApprovalQueue::new(db)
        .decide(&entry.id, true, "drill approve plan", &now())
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("drill")));
    execute_approved(db, &exec, &entry.id, &now()).unwrap();
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
}

/// A task-bound pending approval at a given plan version (the shape
/// the worker links when a gated step dispatches).
fn link_pending_approval(db: &Db, task_id: &str, plan_version: u64, step_id: &str) -> String {
    let na = NewApproval {
        idempotency_key: format!("drill:{task_id}:{step_id}:v{plan_version}"),
        kind: "plan_step".into(),
        class: ActionClass::DraftMessage,
        title: format!("批准步骤 {step_id}"),
        preview: format!("步骤 {step_id} 需要批准"),
        payload: json!({"step_id": step_id}),
        reasons: vec!["步骤需要批准".into()],
        origin: format!("task:{task_id}"),
        ttl: chrono::Duration::hours(72),
    };
    let id = ApprovalQueue::new(db)
        .submit(&na, &now())
        .unwrap()
        .id()
        .to_string();
    TaskApprovals::new(db)
        .link(task_id, &id, plan_version, step_id, &now())
        .unwrap();
    id
}

/// (b) Acceptance: a resource version change redoes only the
/// affected branch — s2 resets, s1/s3 and their receipts do not move,
/// the version bumps, the old approval dies, the budget is inherited.
#[test]
fn replan_redoes_only_the_affected_branch() {
    let db = Db::open_in_memory().unwrap();
    let tasks = TaskStore::new(&db);
    let task = tasks
        .create("test", "长任务：三步整理", "worker", "", 3, None, &now())
        .unwrap();
    tasks.set_budget(&task.id, 10, &now()).unwrap();
    let plan = PlanStore::new(&db)
        .create(
            "长任务：三步整理",
            "完成三步整理",
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
    tasks.set_plan_id(&task.id, &plan.id, &now()).unwrap();
    approve_plan(&db, &plan.id);
    let ctx = TaskContext {
        task_id: task.id.clone(),
        goal: "完成三步整理".into(),
        facts: vec!["s1、s2 已完成".into()],
        constraints: vec!["不得向未确认的邮箱地址发邮件".into()],
        evidence: vec![EvidenceRef::new("receipt", "rcpt-s1", "s1 的回执")],
        open_items: vec!["s3 待执行".into()],
    };
    TaskContextStore::new(&db).save(&ctx, &now()).unwrap();
    let claimed = tasks.start(&task.id, "drill", 60, &now()).unwrap();
    let lease = claimed.lease().unwrap();
    let units = UnitStore::new(&db);
    units
        .ensure_steps(
            &task.id,
            &lease,
            &[
                step_spec("s1", "fs_read"),
                step_spec("s2", "fs_read"),
                step_spec("s3", "fs_read"),
            ],
            &now(),
        )
        .unwrap();
    let store = PlanStore::new(&db);
    for (idx, (id, receipt)) in [("s1", "rcpt-s1"), ("s2", "rcpt-s2")].iter().enumerate() {
        units
            .complete_step(
                &task.id,
                &lease,
                id,
                ExecStatus::Done,
                &format!("{id} 的结果"),
                &now(),
            )
            .unwrap();
        units
            .set_step_receipt(&task.id, &lease, id, receipt, &now())
            .unwrap();
        store
            .record_step(
                &plan.id,
                idx,
                StepStatus::Done,
                &format!("{id} 的结果"),
                None,
                &now(),
            )
            .unwrap();
    }
    let unit = units
        .begin_unit(&task.id, &lease, "s1", &[], &now())
        .unwrap();
    units
        .finish_unit(
            &task.id,
            &lease,
            unit.unit_seq,
            ExecStatus::Done,
            3,
            1,
            &now(),
        )
        .unwrap();
    let old_approval = link_pending_approval(&db, &task.id, 1, "s3");
    assert_eq!(
        TaskContextStore::new(&db)
            .view(&task.id, 10)
            .unwrap()
            .remaining_budget,
        Some(7)
    );
    // s2's data source changed version: redo s2 only.
    let msg = replan_task(
        &db,
        &task.id,
        1,
        &["s2".to_string()],
        "s2 的数据源版本变了",
        "cli",
        &now(),
    )
    .unwrap();
    assert!(msg.contains("v1 → v2"), "{msg}");
    assert!(msg.contains("s2"), "{msg}");
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.version, 2);
    assert_eq!(plan.goal, "完成三步整理", "重规划不得改目标");
    assert_eq!(plan.steps[0].status, StepStatus::Done);
    assert_eq!(
        plan.steps[0].output, "s1 的结果",
        "未受影响的已完成步骤原样不动"
    );
    assert_eq!(
        plan.steps[1].status,
        StepStatus::Pending,
        "受影响的步骤回到待执行"
    );
    assert_eq!(plan.steps[1].output, "", "重做的步骤输出已清");
    assert_eq!(plan.steps[2].status, StepStatus::Pending);
    assert_eq!(plan.status, PlanStatus::PendingApproval, "新版本需重新批准");
    let steps = units.list_steps(&task.id).unwrap();
    assert_eq!(steps[0].status, ExecStatus::Done);
    assert_eq!(steps[0].receipt_id, "rcpt-s1");
    assert_eq!(steps[1].status, ExecStatus::Pending, "任务镜像与计划一致");
    assert_eq!(
        steps[1].receipt_id, "rcpt-s2",
        "旧回执保留在账本里，待重做时覆盖"
    );
    assert_eq!(steps[2].status, ExecStatus::Pending);
    let task = tasks.get(&task.id).unwrap().unwrap();
    assert_eq!(task.plan_version, 2);
    let old = ApprovalQueue::new(&db).get(&old_approval).unwrap();
    assert_eq!(
        old.status,
        ApprovalStatus::Invalidated,
        "旧版本绑定的审批已失效"
    );
    let fresh = latest_plan_approval(&db, &plan).unwrap().unwrap();
    assert_eq!(
        fresh.status,
        ApprovalStatus::Pending,
        "新版本有自己的待审批"
    );
    assert_eq!(
        TaskContextStore::new(&db)
            .view(&task.id, 10)
            .unwrap()
            .remaining_budget,
        Some(7),
        "剩余预算继承，不得重置"
    );
    let loaded = TaskContextStore::new(&db).load(&task.id).unwrap().unwrap();
    assert_eq!(loaded, ctx, "上下文在重规划后原样不动");
    // A stale replan is refused.
    assert!(replan_task(&db, &task.id, 1, &["s3".to_string()], "过期", "cli", &now()).is_err());
}
