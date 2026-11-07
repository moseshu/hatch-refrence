//! Plan approvals go through the approval queue (public API only).
//!
//! Creating (or editing / flagging) a plan submits a `plan_approve`
//! entry; the plan flips to `approved` only when that entry is decided
//! `Approved` and the decision executed. Direct `store.approve` calls
//! without a decided entry fail — there is no second path.

use hatch_core::time::{now, Timestamp};
use hatch_plan::{
    latest_plan_approval, pending_plan_approval, run_plan, Plan, PlanApproveExecutor, PlanStatus,
    PlanStep, PlanStore, RiskLevel, RunDeps, StepEdit, StepStatus, PLAN_APPROVAL_KIND,
};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::{params, Db};
use hatch_tools::{execute_approved, ExecOutcome, ExecutorRegistry};
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

fn env() -> (Db, ExecutorRegistry) {
    let db = Db::open_in_memory().unwrap();
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("test")));
    (db, exec)
}

fn create(db: &Db, task: &str, draft: bool, now: &Timestamp) -> Plan {
    PlanStore::new(db)
        .create(task, "m", vec![step("s1")], draft, now)
        .unwrap()
}

/// Decide the plan's pending entry and execute the decision, the way
/// `hatch plan approve` does. Returns the plan afterwards.
fn approve_via_queue(db: &Db, exec: &ExecutorRegistry, plan_id: &str, now: &Timestamp) -> Plan {
    let entry = pending_plan_approval(db, plan_id).unwrap().unwrap();
    ApprovalQueue::new(db)
        .decide(&entry.id, true, "ok", now)
        .unwrap();
    let out = execute_approved(db, exec, &entry.id, now).unwrap();
    assert!(
        matches!(out, ExecOutcome::Executed(_)),
        "expected Executed, got {out:?}"
    );
    PlanStore::new(db).get(plan_id).unwrap().unwrap()
}

#[test]
fn create_submits_entry_and_direct_approve_fails() {
    let (db, _exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    assert_eq!(entry.kind, PLAN_APPROVAL_KIND);
    assert_eq!(entry.origin, format!("plan:{}", plan.id));
    assert_eq!(entry.status, ApprovalStatus::Pending);
    assert_eq!(
        entry.payload.get("plan_id").and_then(|v| v.as_str()),
        Some(plan.id.as_str())
    );
    assert_eq!(
        entry.payload.get("steps_hash").and_then(|v| v.as_str()),
        Some(Plan::steps_hash(&plan.steps).as_str())
    );
    // The plan itself cannot be approved without a decision.
    assert!(PlanStore::new(&db)
        .approve(&plan.id, "test", "", &now)
        .is_err());
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
}

#[test]
fn draft_submits_nothing() {
    let (db, _exec) = env();
    let now = now();
    let plan = create(&db, "做 Z", true, &now);
    assert_eq!(plan.status, PlanStatus::Draft);
    assert!(pending_plan_approval(&db, &plan.id).unwrap().is_none());
    assert_eq!(ApprovalQueue::new(&db).list(None).unwrap().len(), 0);
}

#[test]
fn decide_then_execute_approves_and_snapshot_is_set() {
    let (db, exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    let plan = approve_via_queue(&db, &exec, &plan.id, &now);
    assert_eq!(plan.status, PlanStatus::Approved);
    assert_eq!(plan.decided_by, "test");
    assert!(!plan.approved_steps_json.is_empty());
    assert_eq!(plan.approved_hash, Plan::steps_hash(&plan.steps));
    // A second execution of the same decision is an error, not a
    // double-apply: the entry is Executed, not Approved.
    let entry = latest_plan_approval(&db, &plan).unwrap().unwrap();
    assert_eq!(entry.status, ApprovalStatus::Executed);
    assert!(execute_approved(&db, &exec, &entry.id, &now).is_err());
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
}

#[test]
fn denied_entry_blocks_approve_and_plan_stays_pending() {
    let (db, _exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    let entry = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    ApprovalQueue::new(&db)
        .decide(&entry.id, false, "不要", &now)
        .unwrap();
    let err = PlanStore::new(&db)
        .approve(&plan.id, "test", "", &now)
        .unwrap_err();
    assert!(err.to_string().contains("拒绝"), "{err}");
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    // And the plan still cannot run.
    let gate = Gatekeeper::new(PolicyConfig::default());
    let reg = hatch_tools::builtin::default_registry();
    let exec = ExecutorRegistry::new();
    let deps = RunDeps {
        db: &db,
        registry: &reg,
        executors: &exec,
        gate: &gate,
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@126.com".into(),
    };
    let err = run_plan(&deps, &plan.id, &now).unwrap_err();
    assert!(err.to_string().contains("不能执行"), "{err}");
}

#[test]
fn edit_resubmits_and_old_decision_cannot_approve_new_content() {
    let (db, exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    let first = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    let plan = approve_via_queue(&db, &exec, &plan.id, &now);
    let plan = PlanStore::new(&db)
        .edit_step(
            &plan.id,
            "s1",
            StepEdit {
                title: Some("改名".into()),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    // A fresh entry was submitted; the old (approved) one is not reused.
    let second = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    assert_ne!(second.id, first.id);
    // The old decision does not approve the new content.
    let err = PlanStore::new(&db)
        .approve(&plan.id, "test", "", &now)
        .unwrap_err();
    assert!(err.to_string().contains("尚未通过"), "{err}");
    let plan = approve_via_queue(&db, &exec, &plan.id, &now);
    assert_eq!(plan.status, PlanStatus::Approved);
    assert_eq!(plan.steps[0].title, "改名");
}

#[test]
fn same_second_edit_back_gets_a_fresh_entry() {
    // Regression: with a key derived from (plan, steps, second-precision
    // timestamp), editing a step away and back within one second reused
    // the already-approved entry and left the plan with no pending entry
    // to decide. Every round now gets its own entry.
    let (db, exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    let first = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    let plan = approve_via_queue(&db, &exec, &plan.id, &now);
    let store = PlanStore::new(&db);
    let plan = store
        .edit_step(
            &plan.id,
            "s1",
            StepEdit {
                title: Some("改名".into()),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    let plan = store
        .edit_step(
            &plan.id,
            "s1",
            StepEdit {
                title: Some("t".into()),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    assert_ne!(entry.id, first.id);
    // And the round can be decided and executed normally.
    let plan = approve_via_queue(&db, &exec, &plan.id, &now);
    assert_eq!(plan.status, PlanStatus::Approved);
}

#[test]
fn supersede_rejects_the_dangling_approval() {
    let (db, _exec) = env();
    let now = now();
    let first = create(&db, "做 X", false, &now);
    let entry = pending_plan_approval(&db, &first.id).unwrap().unwrap();
    let _second = create(&db, "做 X", false, &now);
    assert_eq!(
        PlanStore::new(&db).get(&first.id).unwrap().unwrap().status,
        PlanStatus::Superseded
    );
    // The superseded plan's entry was rejected: it can no longer be
    // decided, and the new plan has its own pending entry.
    let entry = ApprovalQueue::new(&db).get(&entry.id).unwrap();
    assert_eq!(entry.status, ApprovalStatus::Rejected);
    assert!(ApprovalQueue::new(&db)
        .decide(&entry.id, true, "x", &now)
        .is_err());
}

#[test]
fn executor_refuses_steps_changed_after_submission() {
    let (db, exec) = env();
    let now = now();
    let plan = create(&db, "做 X", false, &now);
    let entry = pending_plan_approval(&db, &plan.id).unwrap().unwrap();
    ApprovalQueue::new(&db)
        .decide(&entry.id, true, "ok", &now)
        .unwrap();
    // Tamper with the stored step after the decision (bypassing
    // `edit_step`, which would have submitted a fresh entry).
    db.conn()
        .execute(
            "UPDATE plan_steps SET tool = 'propose_email' WHERE plan_id = ?1",
            params![plan.id],
        )
        .unwrap();
    let out = execute_approved(&db, &exec, &entry.id, &now).unwrap();
    assert!(
        matches!(out, ExecOutcome::Failed(_)),
        "expected Failed, got {out:?}"
    );
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    assert_eq!(
        ApprovalQueue::new(&db).get(&entry.id).unwrap().status,
        ApprovalStatus::Failed
    );
}
