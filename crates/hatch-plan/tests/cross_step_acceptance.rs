//! Cross-step acceptance (external review item 5), batch 1: frozen
//! task-level predicates evaluated deterministically (L2) over the
//! shared evidence bundle at the task boundary, in Observe.
//!
//! The chain is the real one — a local calendar event plus a sent
//! mail through `FileOutbox` — so L1 readback, `observe()` field
//! extraction and L2 evaluation all run on genuine readback data.
//! Nothing real is sent and no model is called.

use chrono_tz::Tz;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_connectors::{CalendarRepo, Event};
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_plan::acceptance::{AcceptanceItem, FieldRef, Operand, PredicateOp};
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
use std::sync::Arc;
use tempfile::TempDir;

fn now() -> Timestamp {
    parse_rfc3339("2026-11-12T00:00:00Z").unwrap()
}

fn deps<'a>(
    db: &'a Db,
    registry: &'a hatch_tools::Registry,
    executors: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    ws: std::path::PathBuf,
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
        grade: None,
        acceptance_enforce: Default::default(),
    }
}

fn event_step() -> PlanStep {
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
    }
}

fn mail_step(body: &str) -> PlanStep {
    PlanStep {
        id: "s2".into(),
        title: "发邀请邮件".into(),
        tool: "propose_email".into(),
        arguments: json!({
            "to": ["li.ting@corp.example"],
            "subject": "邀请：项目评审会",
            "body": body,
        }),
        files: vec![],
        expected: "邀请邮件已发送".into(),
        risk: RiskLevel::Medium,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

fn time_predicate() -> AcceptanceItem {
    AcceptanceItem::Predicate {
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
    }
}

/// Create the plan, freeze the acceptance contract, then approve —
/// the order the product path uses (submit → freeze → approval).
fn approved_plan_with_acceptance(db: &Db, body: &str) -> Plan {
    let store = PlanStore::new(db);
    let plan = store
        .create(
            "test task",
            "",
            "",
            vec![event_step(), mail_step(body)],
            false,
            &now(),
        )
        .unwrap();
    store
        .set_acceptance(&plan.id, &[time_predicate()], &now())
        .unwrap();
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("计划应有审批记录");
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
    assert_eq!(plan.acceptance.len(), 1, "验收契约应随计划冻结");
    plan
}

/// Drive the plan to a terminal outcome, approving + executing each
/// queued step approval as it appears (bounded).
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
            .expect("暂停时应有等待审批的步骤");
        let approval_id = waiting.approval_id.clone().expect("应有审批 id");
        let q = ApprovalQueue::new(db);
        assert_eq!(
            q.decide(&approval_id, true, "test approve step", &now())
                .unwrap(),
            ApprovalStatus::Approved
        );
        execute_approved(db, executors, &approval_id, &now()).unwrap();
    }
    panic!("plan did not settle within the drive bound");
}

fn task_report(db: &Db, plan_id: &str) -> hatch_plan::verification::VerificationReport {
    VerificationStore::new(db)
        .reports(plan_id)
        .unwrap()
        .into_iter()
        .rfind(|r| r.step_id == "@task-completion")
        .expect("应有任务级验收报告")
}

fn l2_check(report: &hatch_plan::verification::VerificationReport) -> &hatch_tools::verification::EvidenceCheck {
    report
        .checks
        .iter()
        .find(|c| c.rule == "cross-step-v1:cal-mail-time")
        .expect("应有 L2 检查")
}

struct Fixture {
    db: Db,
    out: TempDir,
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
    Fixture {
        db,
        out,
        ws: TempDir::new().unwrap(),
        registry: default_registry(),
        executors,
        gate: Gatekeeper::new(PolicyConfig::default()),
    }
}

fn mail_count(out: &TempDir) -> usize {
    std::fs::read_dir(out.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.ends_with(".eml"))
        .count()
}

#[test]
fn consistent_times_pass_gate_and_shadow() {
    let f = fixture();
    let plan = approved_plan_with_acceptance(
        &f.db,
        "项目评审会，会议时间 2026-11-22T10:00:00+08:00，请准时参加。",
    );
    let d = deps(&f.db, &f.registry, &f.executors, &f.gate, f.ws.path().to_path_buf());
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass);
    assert_eq!(task.shadow(), Verdict::Pass);
    let l2 = l2_check(&task);
    assert_eq!(l2.verdict, Verdict::Pass);
    assert_eq!(l2.layer, Layer::L2);
    assert!(!l2.enforced, "第一批 L2 是 Observe，不生效");
}

#[test]
fn contradictory_times_fail_shadow_but_not_gate() {
    let f = fixture();
    let plan = approved_plan_with_acceptance(
        &f.db,
        "项目评审会，会议时间 2026-11-22T11:00:00+08:00，请准时参加。",
    );
    let d = deps(&f.db, &f.registry, &f.executors, &f.gate, f.ws.path().to_path_buf());
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    // Observe: the cross-step contradiction is recorded, not enforced.
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass, "门槛只算 Enforce 检查");
    assert_eq!(task.shadow(), Verdict::Fail, "影子裁决应抓到跨步骤错位");
    assert_eq!(l2_check(&task).verdict, Verdict::Fail);
    // Every L1 check passed on its own — only the combination is wrong.
    assert!(task
        .checks
        .iter()
        .filter(|c| c.layer == Layer::L1)
        .all(|c| c.verdict != Verdict::Fail));
}

#[test]
fn unconfirmable_mail_time_is_ambiguous_shadow() {
    let f = fixture();
    let plan = approved_plan_with_acceptance(&f.db, "项目评审会，时间另行通知，请准时参加。");
    let d = deps(&f.db, &f.registry, &f.executors, &f.gate, f.ws.path().to_path_buf());
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Pass);
    assert_eq!(task.shadow(), Verdict::Ambiguous);
    assert_eq!(l2_check(&task).verdict, Verdict::Ambiguous);
}

#[test]
fn observe_l2_does_not_relax_l1_and_recovery_never_resends() {
    let f = fixture();
    let plan = approved_plan_with_acceptance(
        &f.db,
        "项目评审会，会议时间 2026-11-22T10:00:00+08:00，请准时参加。",
    );
    let d = deps(&f.db, &f.registry, &f.executors, &f.gate, f.ws.path().to_path_buf());
    // Drive until the mail step has executed but the plan has not
    // finished: run 1 queues s1, approve+execute; run 2 settles s1
    // and queues s2, approve+execute. Then mutate the calendar
    // event so the task-boundary L1 re-read disagrees with the
    // approved payload.
    let mut mutated = false;
    let mut final_report = None;
    for _ in 0..8 {
        let report = run_plan(&d, &plan.id, &now()).unwrap();
        if report.outcome != RunOutcome::PausedAwaitingApproval {
            final_report = Some(report);
            break;
        }
        let current = PlanStore::new(&f.db).get(&plan.id).unwrap().unwrap();
        let waiting = current
            .steps
            .iter()
            .find(|s| s.status == StepStatus::AwaitingApproval)
            .unwrap()
            .clone();
        let approval_id = waiting.approval_id.clone().unwrap();
        let q = ApprovalQueue::new(&f.db);
        q.decide(&approval_id, true, "test approve step", &now())
            .unwrap();
        execute_approved(&f.db, &f.executors, &approval_id, &now()).unwrap();
        if waiting.id == "s2" && !mutated {
            // Both effects are applied; corrupt the event title so
            // the boundary re-read fails L1 while L2 (times) still
            // agrees.
            let done = PlanStore::new(&f.db).get(&plan.id).unwrap().unwrap();
            let uid = done.steps[0]
                .output
                .strip_prefix("event ")
                .and_then(|s| s.split_whitespace().next())
                .expect("s1 输出应带事件 uid")
                .to_string();
            let repo = CalendarRepo::new(&f.db);
            let mut ev: Event = repo.get(&uid).unwrap().unwrap();
            ev.title = "被改掉的标题".into();
            repo.upsert(&ev).unwrap();
            mutated = true;
        }
    }
    let report = final_report.expect("plan should settle");
    assert!(mutated);
    assert_eq!(
        report.outcome,
        RunOutcome::StoppedOnFailure,
        "L1 在任务边界回读失败必须拦住完成（L2 Observe 不得放宽）"
    );
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Fail);
    assert_eq!(mail_count(&f.out), 1, "恰好一封邮件");
    // Recovery re-runs verification reads only — never the send.
    let _ = run_plan(&d, &plan.id, &now());
    assert_eq!(mail_count(&f.out), 1, "恢复不得重发邮件");
}
