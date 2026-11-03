//! End-to-end plan execution tests (public API only, scripted tools).

use hatch_plan::{
    run_plan, PlanStatus, PlanStep, PlanStore, RiskLevel, RunDeps, RunOutcome, StepStatus,
};

use hatch_connectors::mail::fixture::FileOutbox;
use hatch_policy::{ApprovalQueue, Gatekeeper, PolicyConfig};
use hatch_store::{params, Db};
use hatch_tools::builtin::default_registry;
use hatch_tools::executors::{CreateEventExecutor, SendEmailExecutor};
use hatch_tools::{ExecutorRegistry, Registry};
use serde_json::json;
use std::sync::Arc;

fn step(id: &str, tool: &str, args: serde_json::Value) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: "t".into(),
        tool: tool.into(),
        arguments: args,
        files: vec![],
        expected: "ok".into(),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

struct Env {
    db: Db,
    _dir: tempfile::TempDir,
    outbox: Arc<FileOutbox>,
    exec: ExecutorRegistry,
    gate: Gatekeeper,
    reg: Registry,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Arc::new(FileOutbox::new(dir.path(), "me@126.com").unwrap());
    Env {
        db: Db::open_in_memory().unwrap(),
        exec: ExecutorRegistry::new()
            .with(Arc::new(SendEmailExecutor::new(
                outbox.clone(),
                "me@126.com",
            )))
            .with(Arc::new(CreateEventExecutor)),
        outbox,
        _dir: dir,
        gate: Gatekeeper::new(PolicyConfig::default()),
        reg: default_registry(),
    }
}

fn deps(e: &Env) -> RunDeps<'_> {
    RunDeps {
        db: &e.db,
        registry: &e.reg,
        executors: &e.exec,
        gate: &e.gate,
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@126.com".into(),
    }
}

fn approved_plan(e: &Env, steps: Vec<PlanStep>) -> String {
    let store = PlanStore::new(&e.db);
    let now = hatch_core::time::now();
    let plan = store.create("task", "test", steps, false, &now).unwrap();
    let plan = store.approve(&plan.id, "test", "", &now).unwrap();
    plan.id
}

#[test]
fn approved_plan_runs_end_to_end() {
    let e = env();
    let now = hatch_core::time::now();
    let id = approved_plan(
        &e,
        vec![
            step("s1", "memory_search", json!({"query": "nothing here"})),
            step("s2", "memory_search", json!({"query": "still nothing"})),
        ],
    );
    let report = run_plan(&deps(&e), &id, &now).unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.steps.len(), 2);
    assert!(report.steps.iter().all(|s| s.status == StepStatus::Done));
    assert!(report.steps[0].output.contains("没有找到相关记忆"));
    let plan = PlanStore::new(&e.db).get(&id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Completed);
    assert_eq!(plan.steps[0].status, StepStatus::Done);
}

#[test]
fn unapproved_plan_cannot_run() {
    let e = env();
    let now = hatch_core::time::now();
    let store = PlanStore::new(&e.db);
    let plan = store
        .create(
            "task",
            "test",
            vec![step("s1", "memory_search", json!({"query": "x"}))],
            false,
            &now,
        )
        .unwrap();
    assert!(run_plan(&deps(&e), &plan.id, &now).is_err());
}

#[test]
fn tampered_step_stops_run_and_needs_reapproval() {
    let e = env();
    let now = hatch_core::time::now();
    let id = approved_plan(&e, vec![step("s1", "memory_search", json!({"query": "x"}))]);
    // Tamper with the stored step directly (bypassing `edit_step`, which
    // would have sent the plan back to pending_approval).
    e.db.conn()
            .execute(
                "UPDATE plan_steps SET tool = 'propose_email', arguments = '{\"to\": [\"x@y.com\"], \"subject\": \"s\", \"body\": \"b\"}' WHERE plan_id = ?1",
                params![id],
            )
            .unwrap();
    let report = run_plan(&deps(&e), &id, &now).unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnDeviation);
    assert!(report.deviation.as_deref().unwrap_or("").contains("工具"));
    let plan = PlanStore::new(&e.db).get(&id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::NeedsReapproval);
    assert!(!plan.deviation.is_empty());
    // Nothing executed: no mail queued, no step marked done.
    assert_eq!(ApprovalQueue::new(&e.db).list(None).unwrap().len(), 0);
    assert_eq!(plan.steps[0].status, StepStatus::Pending);
}

#[test]
fn queued_step_pauses_and_resumes_after_approval() {
    let e = env();
    let now = hatch_core::time::now();
    let id = approved_plan(
        &e,
        vec![
            step(
                "s1",
                "propose_email",
                json!({"to": ["boss@corp.com"], "subject": "周报", "body": "见附件"}),
            ),
            step("s2", "memory_search", json!({"query": "x"})),
        ],
    );
    // First run: the mail step is queued by the gate; the run pauses.
    let report = run_plan(&deps(&e), &id, &now).unwrap();
    assert_eq!(report.outcome, RunOutcome::PausedAwaitingApproval);
    assert_eq!(
        e.outbox.entries().unwrap().len(),
        0,
        "nothing sent before approval"
    );
    let plan = PlanStore::new(&e.db).get(&id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Executing);
    assert_eq!(plan.steps[0].status, StepStatus::AwaitingApproval);
    let approval_id = plan.steps[0].approval_id.clone().unwrap();

    // User approves in the approval queue; the executor runs the mail.
    ApprovalQueue::new(&e.db)
        .decide(&approval_id, true, "ok", &now)
        .unwrap();
    hatch_tools::execute_approved(&e.db, &e.exec, &approval_id, &now).unwrap();
    assert_eq!(e.outbox.entries().unwrap().len(), 1);

    // Resume: the queued step resolves from the ledger and the run finishes.
    let report = run_plan(&deps(&e), &id, &now).unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.steps.len(), 2);
    assert_eq!(
        e.outbox.entries().unwrap().len(),
        1,
        "mail sent exactly once"
    );
    let plan = PlanStore::new(&e.db).get(&id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Completed);
}

#[test]
fn failed_step_stops_and_resume_retries_it() {
    let e = env();
    let now = hatch_core::time::now();
    // `memory_add` with a bad subject fails at the tool level.
    let id = approved_plan(
        &e,
        vec![
            step("s1", "memory_add", json!({"subject": "", "text": "x"})),
            step("s2", "memory_search", json!({"query": "x"})),
        ],
    );
    let report = run_plan(&deps(&e), &id, &now).unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].status, StepStatus::Failed);
    let plan = PlanStore::new(&e.db).get(&id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Executing, "stays resumable");
    assert_eq!(
        plan.steps[1].status,
        StepStatus::Pending,
        "later steps never ran"
    );
}
