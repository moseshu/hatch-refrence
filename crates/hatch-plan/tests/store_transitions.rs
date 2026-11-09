//! PlanStore state-machine tests (public API only).

use hatch_core::time::{now, Timestamp};
use hatch_plan::{Plan, PlanStatus, PlanStep, PlanStore, RiskLevel, StepEdit, StepStatus};
use hatch_store::Db;
use serde_json::json;

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

fn env() -> Db {
    Db::open_in_memory().unwrap()
}

/// Approve the way the product does: decide the plan's pending queue
/// entry, then execute the decision (the executor applies it).
fn approve_via_queue(db: &Db, plan_id: &str, by: &str, note: &str, now: &Timestamp) -> Plan {
    let entry = hatch_plan::pending_plan_approval(db, plan_id)
        .unwrap()
        .unwrap();
    hatch_policy::ApprovalQueue::new(db)
        .decide(&entry.id, true, note, now)
        .unwrap();
    let exec = hatch_tools::ExecutorRegistry::new().with(std::sync::Arc::new(
        hatch_plan::PlanApproveExecutor::new(by),
    ));
    hatch_tools::execute_approved(db, &exec, &entry.id, now).unwrap();
    PlanStore::new(db).get(plan_id).unwrap().unwrap()
}

#[test]
fn create_approve_reject_and_edit_transitions() {
    let db = env();
    let store = PlanStore::new(&db);
    let now = now();
    let plan = store
        .create("做 X", "", "m", vec![step("s1"), step("s2")], false, &now)
        .unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    assert_eq!(plan.task, "做 X");
    assert_eq!(plan.steps.len(), 2);

    // Approve from pending; snapshot is recorded.
    let plan = approve_via_queue(&db, &plan.id, "cli", "ok", &now);
    assert_eq!(plan.status, PlanStatus::Approved);
    assert_eq!(plan.decided_by, "cli");
    assert!(plan.decided_at.is_some());
    assert!(!plan.approved_steps_json.is_empty());
    assert_eq!(plan.approved_hash, Plan::steps_hash(&plan.steps));
    assert_eq!(store.approved_steps(&plan).unwrap().len(), 2);

    // Approving twice is an error.
    assert!(store.approve(&plan.id, "cli", "again", &now).is_err());

    // Editing an approved plan returns it to pending_approval and
    // clears the snapshot.
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
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    assert_eq!(plan.steps[0].title, "改名");
    assert!(plan.approved_steps_json.is_empty());

    // Re-approve, then reject.
    let plan = approve_via_queue(&db, &plan.id, "cli", "ok2", &now);
    let plan = store.reject(&plan.id, "cli", "不要了", &now).unwrap();
    assert_eq!(plan.status, PlanStatus::Rejected);
    assert_eq!(plan.decision_note, "不要了");
    // Terminal plans cannot be edited or approved.
    assert!(store
        .edit_step(&plan.id, "s1", StepEdit::default(), &now)
        .is_err());
    assert!(store.approve(&plan.id, "cli", "x", &now).is_err());
}

#[test]
fn same_task_supersedes_unapproved_plan() {
    let db = env();
    let store = PlanStore::new(&db);
    let now = now();
    let first = store
        .create("做 X", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    let _second = store
        .create("做 X", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    assert_eq!(
        store.get(&first.id).unwrap().unwrap().status,
        PlanStatus::Superseded
    );
    // An approved plan is NOT superseded by a newer plan for the task.
    let third = store
        .create("做 Y", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    let third = approve_via_queue(&db, &third.id, "cli", "", &now);
    let _fourth = store
        .create("做 Y", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    assert_eq!(
        store.get(&third.id).unwrap().unwrap().status,
        PlanStatus::Approved
    );
}

#[test]
fn draft_stays_draft_on_edit_and_cannot_run() {
    let db = env();
    let store = PlanStore::new(&db);
    let now = now();
    let plan = store
        .create("做 Z", "", "m", vec![step("s1")], true, &now)
        .unwrap();
    assert_eq!(plan.status, PlanStatus::Draft);
    let plan = store
        .edit_step(
            &plan.id,
            "s1",
            StepEdit {
                expected: Some("new".into()),
                ..Default::default()
            },
            &now,
        )
        .unwrap();
    assert_eq!(plan.status, PlanStatus::Draft);
    let plan = store.cancel(&plan.id, "cli", "", &now).unwrap();
    assert_eq!(plan.status, PlanStatus::Cancelled);
}

#[test]
fn flag_deviation_and_step_results() {
    let db = env();
    let store = PlanStore::new(&db);
    let now = now();
    let plan = store
        .create("做 W", "", "m", vec![step("s1")], false, &now)
        .unwrap();
    let plan = approve_via_queue(&db, &plan.id, "cli", "", &now);
    store
        .flag_deviation(&plan.id, "步骤 s1 工具变了", &now)
        .unwrap();
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::NeedsReapproval);
    assert_eq!(plan.deviation, "步骤 s1 工具变了");
    // Re-approval from needs_reapproval works and clears the deviation.
    let plan = approve_via_queue(&db, &plan.id, "cli", "确认无误", &now);
    assert_eq!(plan.status, PlanStatus::Approved);
    assert!(plan.deviation.is_empty());
    store
        .record_step(&plan.id, 0, StepStatus::Done, "结果", None, &now)
        .unwrap();
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.steps[0].status, StepStatus::Done);
    assert_eq!(plan.steps[0].output, "结果");
}
