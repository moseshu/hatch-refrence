//! Cross-step acceptance batch 2: NL items graded once per task
//! boundary over the shared evidence bundle (L3, Observe), with a
//! scripted grader — no model is called. The chain is the real one
//! (local calendar + FileOutbox mail), so the evidence the fake
//! grader sees is genuine readback data.

use chrono_tz::Tz;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_plan::acceptance::{
    AcceptanceItem, FieldRef, NlEvidence, NlItemVerdict, Operand, PredicateOp,
};
use hatch_plan::verification::VerificationStore;
use hatch_plan::{
    latest_plan_approval, run_plan, Plan, PlanApproveExecutor, PlanStatus, PlanStep, PlanStore,
    RiskLevel, RunDeps, RunOutcome, RunReport, StepStatus,
};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::verification::{Layer, Verdict};
use hatch_tools::{
    builtin::default_registry, execute_approved, CreateEventExecutor, ExecutorRegistry,
    SendEmailExecutor,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn now() -> Timestamp {
    parse_rfc3339("2026-11-12T00:00:00Z").unwrap()
}

type Grader = dyn Fn(&str, &[AcceptanceItem], &[NlEvidence], &str) -> Result<Vec<NlItemVerdict>>;

fn deps<'a>(
    db: &'a Db,
    registry: &'a hatch_tools::Registry,
    executors: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    ws: std::path::PathBuf,
    grade: Option<&'a Grader>,
) -> RunDeps<'a> {
    RunDeps {
        db,
        registry,
        executors,
        gate,
        tz: Tz::UTC,
        user_email: "moses@126.example".into(),
        workspace_root: Some(ws),
        snapshot_root: None,
        verify: None,
        output_map: None,
        user_name: String::new(),
        lease_fence: None,
        grade,
    }
}

fn steps() -> Vec<PlanStep> {
    vec![
        PlanStep {
            id: "s1".into(),
            title: "建日程".into(),
            tool: "propose_event".into(),
            arguments: json!({"title": "项目评审", "start": "2026-11-22T10:00:00+08:00"}),
            files: vec![],
            expected: "日程已创建".into(),
            risk: RiskLevel::Medium,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        },
        PlanStep {
            id: "s2".into(),
            title: "发邀请邮件".into(),
            tool: "propose_email".into(),
            arguments: json!({
                "to": ["li.ting@corp.example"],
                "subject": "邀请：项目评审会",
                "body": "项目评审会，会议时间 2026-11-22T10:00:00+08:00，请准时参加。",
            }),
            files: vec![],
            expected: "邀请邮件已发送".into(),
            risk: RiskLevel::Medium,
            status: StepStatus::Pending,
            output: String::new(),
            approval_id: None,
        },
    ]
}

fn approved_plan(db: &Db, acceptance: &[AcceptanceItem]) -> Plan {
    let store = PlanStore::new(db);
    let plan = store
        .create("test task", "", "", steps(), false, &now())
        .unwrap();
    if !acceptance.is_empty() {
        store.set_acceptance(&plan.id, acceptance, &now()).unwrap();
    }
    let entry = latest_plan_approval(db, &plan).unwrap().expect("审批记录");
    let q = ApprovalQueue::new(db);
    assert_eq!(
        q.decide(&entry.id, true, "test approve plan", &now())
            .unwrap(),
        ApprovalStatus::Approved
    );
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("test")));
    execute_approved(db, &exec, &entry.id, &now()).unwrap();
    let plan = store.get(&plan.id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
    plan
}

fn drive(db: &Db, d: &RunDeps, plan_id: &str, executors: &ExecutorRegistry) -> RunReport {
    for _ in 0..8 {
        let report = run_plan(d, plan_id, &now()).unwrap();
        if report.outcome != RunOutcome::PausedAwaitingApproval {
            return report;
        }
        let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
        let waiting = plan
            .steps
            .iter()
            .find(|s| s.status == StepStatus::AwaitingApproval)
            .expect("等待审批的步骤");
        let approval_id = waiting.approval_id.clone().expect("审批 id");
        let q = ApprovalQueue::new(db);
        q.decide(&approval_id, true, "test approve step", &now())
            .unwrap();
        execute_approved(db, executors, &approval_id, &now()).unwrap();
    }
    panic!("plan did not settle");
}

fn task_report(db: &Db, plan_id: &str) -> hatch_plan::verification::VerificationReport {
    VerificationStore::new(db)
        .reports(plan_id)
        .unwrap()
        .into_iter()
        .rfind(|r| r.step_id == "@task-completion")
        .expect("任务级报告")
}

struct Fixture {
    db: Db,
    ws: TempDir,
    registry: hatch_tools::Registry,
    executors: ExecutorRegistry,
    gate: Gatekeeper,
}

fn fixture() -> Fixture {
    let db = Db::open_in_memory().unwrap();
    let out = TempDir::new().unwrap();
    let account = "moses@126.example";
    let outbox = Arc::new(FileOutbox::new(out.path(), account).unwrap());
    let executors = ExecutorRegistry::new()
        .with(Arc::new(CreateEventExecutor))
        .with(Arc::new(SendEmailExecutor::new(outbox, account)));
    // Leak the outbox dir for the test's lifetime (process-local).
    std::mem::forget(out);
    Fixture {
        db,
        ws: TempDir::new().unwrap(),
        registry: default_registry(),
        executors,
        gate: Gatekeeper::new(PolicyConfig::default()),
    }
}

fn nl_item() -> AcceptanceItem {
    AcceptanceItem::Nl {
        id: "nl-1".into(),
        expected: "邀请邮件写明了会议时间且与日程一致".into(),
    }
}

/// Scripted grader: canned verdict per item, citing the mail step's
/// evidence; records call count and the evidence ids it was shown.
fn fake_grader(
    verdict: Verdict,
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<String>>>,
) -> impl Fn(&str, &[AcceptanceItem], &[NlEvidence], &str) -> Result<Vec<NlItemVerdict>> {
    move |_goal, items, evidence, _candidate| {
        calls.fetch_add(1, Ordering::SeqCst);
        seen.lock()
            .unwrap()
            .extend(evidence.iter().map(|e| e.id.clone()));
        Ok(items
            .iter()
            .map(|i| NlItemVerdict {
                id: i.id().to_string(),
                verdict: verdict.clone(),
                evidence: vec!["s2".into()],
                reason: "正文写明会议时间 2026-11-22T10:00:00+08:00".into(),
            })
            .collect())
    }
}

#[test]
fn nl_pass_lands_as_l3_shadow_pass() {
    let f = fixture();
    let plan = approved_plan(&f.db, &[nl_item()]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let grader = fake_grader(Verdict::Pass, calls.clone(), seen.clone());
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        Some(&grader),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "整组 NL 只评一次");
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&"s1".to_string()) && seen.contains(&"s2".to_string()),
        "grader 应看到全部可观测步骤的证据，实际 {seen:?}");
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass);
    assert_eq!(task.shadow(), Verdict::Pass);
    let l3 = task
        .checks
        .iter()
        .find(|c| c.rule == "cross-step-v1:nl-1")
        .expect("应有 L3 检查");
    assert_eq!(l3.verdict, Verdict::Pass);
    assert_eq!(l3.layer, Layer::L3);
    assert!(!l3.enforced, "第二批 L3 是 Observe");
    assert!(!l3.observed_digest.is_empty(), "L3 检查应带证据包摘要");
}

#[test]
fn nl_fail_is_shadow_only_in_observe() {
    let f = fixture();
    let plan = approved_plan(&f.db, &[nl_item()]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let grader = fake_grader(Verdict::Fail, calls.clone(), seen);
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        Some(&grader),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed, "Observe 不拦完成");
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass);
    assert_eq!(task.shadow(), Verdict::Fail);
}

#[test]
fn grader_error_degrades_to_ambiguous_shadow() {
    let f = fixture();
    let plan = approved_plan(&f.db, &[nl_item()]);
    let grader = |_g: &str, _i: &[AcceptanceItem], _e: &[NlEvidence], _c: &str| {
        Err(Error::Invalid("fake grader down".into()))
    };
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        Some(&grader),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass);
    assert_eq!(task.shadow(), Verdict::Ambiguous);
}

#[test]
fn no_nl_items_means_zero_grader_calls() {
    let f = fixture();
    let predicate = AcceptanceItem::Predicate {
        id: "cal-mail-time".into(),
        left: FieldRef {
            step: "s1".into(),
            field: "event.starts_at".into(),
        },
        op: PredicateOp::Equals,
        right: Some(Operand::Field {
            step: "s2".into(),
            field: "mail.meeting_time".into(),
        }),
        normalize: Some("rfc3339_tz".into()),
    };
    let plan = approved_plan(&f.db, &[predicate]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let grader = fake_grader(Verdict::Pass, calls.clone(), seen);
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        Some(&grader),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "无 NL 条目不得调用 grader");
    let task = task_report(&f.db, &plan.id);
    assert!(task.checks.iter().all(|c| c.layer != Layer::L3));
}

#[test]
fn nl_without_grader_records_no_l3() {
    let f = fixture();
    let plan = approved_plan(&f.db, &[nl_item()]);
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        None,
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    assert!(task.checks.iter().all(|c| c.layer != Layer::L3));
    assert_eq!(task.shadow(), Verdict::Pass);
}
