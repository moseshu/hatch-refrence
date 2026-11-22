//! Plan-execution fault-injection tests (M1, task 4): pre-execution
//! validation, receipt verification (pass / fail / ambiguous), plan
//! deviation, plan read failure, and duplicate-approval reuse.
//!
//! The steps run on a stub tool or on `propose_email` through a
//! `FileOutbox` — nothing real is sent, and no model is called.

use chrono_tz::Tz;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_llm::ToolSpec;
use hatch_plan::{
    latest_plan_approval, run_plan, Plan, PlanApproveExecutor, PlanStatus, PlanStep, PlanStore,
    RiskLevel, RunDeps, RunOutcome, StepStatus, StepVerifier, StepVerify,
};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::{params, Db};
use hatch_tools::{
    builtin::default_registry, execute_approved, ExecutorRegistry, Registry, SendEmailExecutor,
    Tool, ToolContext, ToolOutput,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// A read tool that returns a fixed string — the verifier decides
/// whether the step passes, so the tool's output never matters.
struct StubTool {
    output: String,
}

impl Tool for StubTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stub_echo".into(),
            description: "test stub: returns a fixed string".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> hatch_core::Result<ToolOutput> {
        Ok(ToolOutput::system(self.output.clone()))
    }
}

fn now() -> Timestamp {
    parse_rfc3339("2026-11-12T00:00:00Z").unwrap()
}

fn stub_registry() -> Registry {
    Registry::new().with(Arc::new(StubTool {
        output: "stub output".into(),
    }))
}

fn deps<'a>(
    db: &'a Db,
    registry: &'a Registry,
    executors: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    ws: Option<std::path::PathBuf>,
    verify: Option<StepVerifier<'a>>,
) -> RunDeps<'a> {
    RunDeps {
        db,
        registry,
        executors,
        gate,
        tz: Tz::UTC,
        user_email: "moses@126.example".into(),
        workspace_root: ws,
        snapshot_root: None,
        verify,
        output_map: None,
        user_name: String::new(),
        lease_fence: None,
        grade: None,
    }
}

fn step(id: &str, tool: &str, arguments: Value) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: format!("step {id}"),
        tool: tool.into(),
        arguments,
        files: vec![],
        expected: "stub output".into(),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

/// Create a draft=false plan (PendingApproval) and approve it the way
/// `hatch plan approve` does, so `run_plan` may run it.
fn approved_plan(db: &Db, steps: Vec<PlanStep>) -> Plan {
    let plan = PlanStore::new(db)
        .create("test task", "", "", steps, false, &now())
        .unwrap();
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("计划应有审批记录");
    let q = ApprovalQueue::new(db);
    let status = q
        .decide(&entry.id, true, "test approve plan", &now())
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("test")));
    let outcome = execute_approved(db, &exec, &entry.id, &now()).unwrap();
    assert!(matches!(outcome, hatch_tools::ExecOutcome::Executed(_)));
    let plan = PlanStore::new(db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
    plan
}

/// How many `send_email` approvals exist for this origin (the
/// plan's own `plan_approve` entry shares the origin, so the kind
/// filter is what separates the step's approval from it).
fn count_approvals(db: &Db, origin: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM approvals WHERE origin = ?1 AND kind = 'send_email'",
            params![origin],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn validation_rejects_unknown_tool_before_any_step_runs() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "no_such_tool", json!({}))]);
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let err = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            None,
        ),
        &plan.id,
        &now(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("unknown tool") && err.to_string().contains("no_such_tool"),
        "未知工具应被点名，实际 {err}"
    );
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved, "校验失败不改计划状态");
    assert!(
        plan.steps.iter().all(|s| s.status == StepStatus::Pending),
        "校验失败前不得执行任何步骤"
    );
}

#[test]
fn verify_pass_marks_done_with_receipt() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let verify = |_step: &PlanStep, _out: &str| StepVerify::Pass(Some("r-1".into()));
    let report = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            Some(&verify),
        ),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].status, StepStatus::Done);
    assert_eq!(report.steps[0].receipt.as_deref(), Some("r-1"));
}

#[test]
fn verify_fail_stops_the_plan_without_ambiguity() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let verify = |_step: &PlanStep, _out: &str| StepVerify::Fail("交付物与验收不符".into());
    let report = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            Some(&verify),
        ),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps[0].status, StepStatus::Failed);
    assert!(!report.steps[0].ambiguous, "明确失败不是外部状态不明");
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_ne!(plan.status, PlanStatus::Completed, "失败的计划不得标记完成");
}

#[test]
fn verify_ambiguous_marks_external_state_unknown() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let verify =
        |_step: &PlanStep, _out: &str| StepVerify::Ambiguous("无法确认副作用是否生效".into());
    let report = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            Some(&verify),
        ),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps[0].status, StepStatus::Failed);
    assert!(
        report.steps[0].ambiguous,
        "无法确认的步骤须标记 ambiguous，供人工核查而非自动重试"
    );
}

#[test]
fn deviation_stops_before_running() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    // Tamper the live plan: the approved snapshot still says stub_echo.
    db.conn()
        .execute(
            "UPDATE plan_steps SET tool = 'no_such_tool' WHERE plan_id = ?1 AND step_id = 's1'",
            params![plan.id],
        )
        .unwrap();
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let report = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            None,
        ),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnDeviation);
    assert!(report.steps.is_empty(), "偏离时不得执行任何步骤");
    assert!(report.deviation.is_some(), "偏离应说明原因");
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert!(!plan.deviation.is_empty(), "偏离应记录在计划上");
}

#[test]
fn missing_plan_is_a_read_failure() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    db.conn()
        .execute("DELETE FROM plans WHERE id = ?1", params![plan.id])
        .unwrap();
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let err = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().to_path_buf()),
            None,
        ),
        &plan.id,
        &now(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("不存在"),
        "读不到计划应是明确的 NotFound，实际 {err}"
    );
}

#[test]
fn second_run_reuses_the_queued_approval() {
    let db = Db::open_in_memory().unwrap();
    let out = TempDir::new().unwrap();
    let account = "moses@126.example";
    let plan = approved_plan(
        &db,
        vec![PlanStep {
            id: "send".into(),
            title: "发邮件".into(),
            tool: "propose_email".into(),
            arguments: json!({
                "to": ["li.ting@corp.example"],
                "subject": "邀请：项目评审会",
                "body": "请准时参加。",
            }),
            files: vec![],
            expected: "邀请邮件已发送".into(),
            risk: RiskLevel::Medium,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        }],
    );
    let origin = format!("plan:{}:v{}", plan.id, plan.version);
    let registry = default_registry();
    let outbox = Arc::new(FileOutbox::new(out.path(), account).unwrap());
    let executors = ExecutorRegistry::new().with(Arc::new(SendEmailExecutor::new(outbox, account)));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    let d = deps(
        &db,
        &registry,
        &executors,
        &gate,
        Some(ws.path().to_path_buf()),
        None,
    );
    // Run 1: queues the send approval and pauses — no mail yet.
    let first = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(first.outcome, RunOutcome::PausedAwaitingApproval);
    assert_eq!(count_approvals(&db, &origin), 1);
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    let approval_id = plan.steps[0]
        .approval_id
        .clone()
        .expect("发送步骤应已记录审批 id");
    // Run 2: the approval is still pending — no second approval.
    let second = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(second.outcome, RunOutcome::PausedAwaitingApproval);
    assert_eq!(count_approvals(&db, &origin), 1, "重复运行不得再排一个审批");
    let plan = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(
        plan.steps[0].approval_id.as_deref(),
        Some(approval_id.as_str()),
        "第二次运行应复用同一个审批"
    );
    // Decide + execute the queued approval, then run 3 finishes with
    // exactly one simulated send.
    let q = ApprovalQueue::new(&db);
    assert_eq!(
        q.decide(&approval_id, true, "test approve send", &now())
            .unwrap(),
        ApprovalStatus::Approved
    );
    let outcome = execute_approved(&db, &executors, &approval_id, &now()).unwrap();
    assert!(matches!(outcome, hatch_tools::ExecOutcome::Executed(_)));
    let third = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(third.outcome, RunOutcome::Completed);
    assert_eq!(third.steps[0].status, StepStatus::Done);
    assert!(
        third.steps[0]
            .receipt
            .as_ref()
            .is_some_and(|r| !r.is_empty()),
        "发送步骤应有回执"
    );
    let mails: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".eml"))
        .collect();
    assert_eq!(mails.len(), 1, "恰好一封模拟发送的邮件");
}

// Independent evidence must gate approval recovery and survive process loss.
struct CountingWrite {
    writes: Arc<std::sync::atomic::AtomicUsize>,
    apply: bool,
}
impl hatch_tools::ActionExecutor for CountingWrite {
    fn kind(&self) -> &'static str {
        "write_file"
    }
    fn execute(
        &self,
        payload: &Value,
        ctx: &hatch_tools::ExecContext,
    ) -> hatch_core::Result<String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.apply {
            hatch_tools::ActionExecutor::execute(&hatch_tools::WriteFileExecutor, payload, ctx)
        } else {
            Ok("worker claims write succeeded".into())
        }
    }
    fn verify(
        &self,
        payload: &Value,
        receipt: &str,
        ctx: &hatch_tools::ExecContext,
    ) -> hatch_core::Result<hatch_tools::verification::EvidenceCheck> {
        hatch_tools::ActionExecutor::verify(&hatch_tools::WriteFileExecutor, payload, receipt, ctx)
    }
}

fn file_registry(ws: &TempDir) -> Registry {
    hatch_tools::builtin::add_general_tools(
        default_registry(),
        &hatch_tools::builtin::GeneralTools {
            workspace: Some(hatch_tools::Workspace::new(ws.path()).unwrap()),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    )
}

fn approve_file_action(db: &Db, executors: &ExecutorRegistry, report: &hatch_plan::RunReport) {
    let approval = report.steps[0].approval_id.as_deref().unwrap();
    ApprovalQueue::new(db)
        .decide(approval, true, "test", &now())
        .unwrap();
    assert!(matches!(
        execute_approved(db, executors, approval, &now()).unwrap(),
        hatch_tools::ExecOutcome::Executed(_)
    ));
}

#[test]
fn approval_recovery_cannot_pass_a_fictional_write_or_repeat_it() {
    let db = Db::open_in_memory().unwrap();
    let ws = TempDir::new().unwrap();
    let plan = approved_plan(
        &db,
        vec![step(
            "write",
            "fs_write",
            json!({"path":"result.txt","content":"approved"}),
        )],
    );
    let registry = file_registry(&ws);
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executors = ExecutorRegistry::new().with(Arc::new(CountingWrite {
        writes: writes.clone(),
        apply: false,
    }));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let rubber_stamp = |_: &PlanStep, _: &str| StepVerify::Pass(None);
    let d = deps(
        &db,
        &registry,
        &executors,
        &gate,
        Some(ws.path().into()),
        Some(&rubber_stamp),
    );
    let pending = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(pending.outcome, RunOutcome::PausedAwaitingApproval);
    approve_file_action(&db, &executors, &pending);
    let failed = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(failed.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(
        failed.steps[0].failure_kind,
        Some(hatch_plan::FailureKind::NeedsHuman)
    );
    assert!(failed.steps[0].ambiguous);
    let reports = hatch_plan::verification::VerificationStore::new(&db)
        .reports(&plan.id)
        .unwrap();
    assert_eq!(
        reports.len(),
        3,
        "three read-only observations, not three writes"
    );
    let again = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(again.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        hatch_plan::verification::VerificationStore::new(&db)
            .reports(&plan.id)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn saved_execution_receipt_recovers_without_reexecuting_the_write() {
    let db = Db::open_in_memory().unwrap();
    let ws = TempDir::new().unwrap();
    let plan = approved_plan(
        &db,
        vec![step(
            "write",
            "fs_write",
            json!({"path":"result.txt","content":"approved"}),
        )],
    );
    let registry = file_registry(&ws);
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executors = ExecutorRegistry::new().with(Arc::new(CountingWrite {
        writes: writes.clone(),
        apply: true,
    }));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let fail = |_: &PlanStep, _: &str| StepVerify::Fail("business check failed".into());
    let d = deps(
        &db,
        &registry,
        &executors,
        &gate,
        Some(ws.path().into()),
        Some(&fail),
    );
    let pending = run_plan(&d, &plan.id, &now()).unwrap();
    approve_file_action(&db, &executors, &pending);
    assert_eq!(
        run_plan(&d, &plan.id, &now()).unwrap().outcome,
        RunOutcome::StoppedOnFailure
    );
    // Restore the precise durable state just after execution, before any
    // observation/report was committed (fault injection, not production retry).
    db.conn()
        .execute(
            "DELETE FROM verification_reports WHERE owner=?1",
            params![plan.id],
        )
        .unwrap();
    db.conn()
        .execute(
            "UPDATE verification_runs SET phase='executed',attempts=0 WHERE owner=?1",
            params![plan.id],
        )
        .unwrap();
    let recovered = run_plan(
        &deps(
            &db,
            &registry,
            &executors,
            &gate,
            Some(ws.path().into()),
            None,
        ),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(recovered.outcome, RunOutcome::Completed);
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(ws.path().join("result.txt")).unwrap(),
        "approved"
    );
    let reports = hatch_plan::verification::VerificationStore::new(&db)
        .reports(&plan.id)
        .unwrap();
    assert_eq!(reports.len(), 2, "step and task-level evidence reports");
}

#[test]
fn changed_acceptance_is_an_approval_deviation() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    db.conn()
        .execute(
            "UPDATE plan_steps SET expected='weaker criterion' WHERE plan_id=?1",
            params![plan.id],
        )
        .unwrap();
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let r = run_plan(
        &deps(&db, &registry, &executors, &gate, None, None),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(r.outcome, RunOutcome::StoppedOnDeviation);
    assert!(r.steps.is_empty());
}

#[test]
fn read_only_dispatching_record_redispatches_on_recovery() {
    let db = Db::open_in_memory().unwrap();
    let plan = approved_plan(&db, vec![step("s1", "stub_echo", json!({}))]);
    let registry = stub_registry();
    let executors = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let ws = TempDir::new().unwrap();
    // First run stops on a verify failure so the plan stays executing.
    let fail = |_: &PlanStep, _: &str| StepVerify::Fail("business check failed".into());
    let d = deps(&db, &registry, &executors, &gate, Some(ws.path().into()), Some(&fail));
    assert_eq!(run_plan(&d, &plan.id, &now()).unwrap().outcome, RunOutcome::StoppedOnFailure);
    // Fault injection: the durable state of a crash between dispatch
    // and receipt for a read-only step.
    db.conn().execute("DELETE FROM verification_reports WHERE owner=?1", params![plan.id]).unwrap();
    db.conn().execute("UPDATE verification_runs SET phase='dispatching',attempts=0 WHERE owner=?1", params![plan.id]).unwrap();
    db.conn().execute("UPDATE plan_steps SET status='pending' WHERE plan_id=?1", params![plan.id]).unwrap();
    // A read cannot have applied an effect: recovery re-dispatches.
    let rubber_stamp = |_: &PlanStep, _: &str| StepVerify::Pass(None);
    let d2 = deps(&db, &registry, &executors, &gate, Some(ws.path().into()), Some(&rubber_stamp));
    let recovered = run_plan(&d2, &plan.id, &now()).unwrap();
    assert_eq!(recovered.outcome, RunOutcome::Completed);
    assert_eq!(recovered.steps[0].status, StepStatus::Done);
}

#[test]
fn write_dispatching_record_still_needs_human_on_recovery() {
    let db = Db::open_in_memory().unwrap();
    let ws = TempDir::new().unwrap();
    let plan = approved_plan(
        &db,
        vec![step("write", "fs_write", json!({"path":"result.txt","content":"approved"}))],
    );
    let registry = file_registry(&ws);
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executors = ExecutorRegistry::new().with(Arc::new(CountingWrite {
        writes: writes.clone(),
        apply: true,
    }));
    let gate = Gatekeeper::new(PolicyConfig::default());
    let rubber_stamp = |_: &PlanStep, _: &str| StepVerify::Pass(None);
    let d = deps(&db, &registry, &executors, &gate, Some(ws.path().into()), Some(&rubber_stamp));
    let pending = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(pending.outcome, RunOutcome::PausedAwaitingApproval);
    approve_file_action(&db, &executors, &pending);
    // Fault injection: crash after dispatch, before the receipt landed.
    db.conn().execute("UPDATE verification_runs SET phase='dispatching' WHERE owner=?1", params![plan.id]).unwrap();
    db.conn().execute("UPDATE plan_steps SET status='pending' WHERE plan_id=?1", params![plan.id]).unwrap();
    let failed = run_plan(&d, &plan.id, &now()).unwrap();
    assert_eq!(failed.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(failed.steps[0].failure_kind, Some(hatch_plan::FailureKind::NeedsHuman));
    assert!(failed.steps[0].ambiguous);
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1, "only the approved execution ran; recovery never re-dispatched the write");
}
