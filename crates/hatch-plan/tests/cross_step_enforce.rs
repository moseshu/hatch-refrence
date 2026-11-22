//! Cross-step acceptance batch 3: per-layer Enforce switch and
//! file-step evidence. With a layer enforced, its verdicts gate
//! completion through the same report; with it off (default) the
//! batch-1/2 Observe behaviour is unchanged. Scripted grader only.

use chrono_tz::Tz;
use hatch_connectors::mail::fixture::FileOutbox;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_plan::acceptance::{
    AcceptanceEnforce, AcceptanceItem, FieldRef, NlEvidence, NlItemVerdict, Operand, PredicateOp,
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
    builtin::{add_general_tools, default_registry, GeneralTools},
    execute_approved, CreateEventExecutor, ExecutorRegistry, SendEmailExecutor, Workspace,
    WriteFileExecutor,
};
use serde_json::json;
use std::sync::Arc;
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
    enforce: AcceptanceEnforce,
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
        acceptance_enforce: enforce,
    }
}

fn step(id: &str, tool: &str, arguments: serde_json::Value) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: id.into(),
        tool: tool.into(),
        arguments,
        files: vec![],
        expected: "done".into(),
        risk: RiskLevel::Medium,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

fn approved_plan(db: &Db, steps: Vec<PlanStep>, acceptance: &[AcceptanceItem]) -> Plan {
    let store = PlanStore::new(db);
    let plan = store.create("test task", "", "", steps, false, &now()).unwrap();
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
    let ws = TempDir::new().unwrap();
    let account = "moses@126.example";
    let outbox = Arc::new(FileOutbox::new(out.path(), account).unwrap());
    std::mem::forget(out);
    let executors = ExecutorRegistry::new()
        .with(Arc::new(CreateEventExecutor))
        .with(Arc::new(SendEmailExecutor::new(outbox, account)))
        .with(Arc::new(WriteFileExecutor));
    let registry = add_general_tools(
        default_registry(),
        &GeneralTools {
            workspace: Some(Workspace::new(ws.path()).unwrap()),
            web_fetch: false,
            search_backend: None,
            max_command_timeout_secs: 60,
        },
    );
    Fixture {
        db,
        ws,
        registry,
        executors,
        gate: Gatekeeper::new(PolicyConfig::default()),
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

fn event_mail_steps(mail_time: &str) -> Vec<PlanStep> {
    vec![
        step(
            "s1",
            "propose_event",
            json!({"title": "项目评审", "start": "2026-11-22T10:00:00+08:00"}),
        ),
        step(
            "s2",
            "propose_email",
            json!({
                "to": ["li.ting@corp.example"],
                "subject": "邀请：项目评审会",
                "body": format!("项目评审会，会议时间 {mail_time}，请准时参加。"),
            }),
        ),
    ]
}

#[test]
fn enforced_l2_blocks_contradiction_and_passes_consistent() {
    let enforce = AcceptanceEnforce { l2: true, l3: false };
    // Contradiction: gate now fails.
    let f = fixture();
    let plan = approved_plan(
        &f.db,
        event_mail_steps("2026-11-22T11:00:00+08:00"),
        &[time_predicate()],
    );
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        None,
        enforce,
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Fail);
    let l2 = task
        .checks
        .iter()
        .find(|c| c.rule == "cross-step-v1:cal-mail-time")
        .unwrap();
    assert!(l2.enforced);
    // Consistent: enforced L2 passes and the task completes.
    let f = fixture();
    let plan = approved_plan(
        &f.db,
        event_mail_steps("2026-11-22T10:00:00+08:00"),
        &[time_predicate()],
    );
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        None,
        enforce,
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
}

#[test]
fn enforced_l3_fail_and_ambiguous_block() {
    let nl = AcceptanceItem::Nl {
        id: "nl-1".into(),
        expected: "邀请邮件写明会议时间".into(),
    };
    let enforce = AcceptanceEnforce { l2: false, l3: true };
    for (verdict, want) in [(Verdict::Fail, Verdict::Fail), (Verdict::Ambiguous, Verdict::Ambiguous)] {
        let f = fixture();
        let plan = approved_plan(
            &f.db,
            event_mail_steps("2026-11-22T10:00:00+08:00"),
            std::slice::from_ref(&nl),
        );
        let grader = move |_g: &str, items: &[AcceptanceItem], _e: &[NlEvidence], _c: &str| {
            Ok(items
                .iter()
                .map(|i| NlItemVerdict {
                    id: i.id().to_string(),
                    verdict: verdict.clone(),
                    evidence: vec!["s2".into()],
                    reason: "fake".into(),
                })
                .collect())
        };
        let d = deps(
            &f.db,
            &f.registry,
            &f.executors,
            &f.gate,
            f.ws.path().to_path_buf(),
            Some(&grader),
            enforce,
        );
        let report = drive(&f.db, &d, &plan.id, &f.executors);
        assert_eq!(report.outcome, RunOutcome::StoppedOnFailure, "{want:?} 应拦住完成");
        let task = task_report(&f.db, &plan.id);
        assert_eq!(task.verdict, want);
        let l3 = task
            .checks
            .iter()
            .find(|c| c.layer == Layer::L3)
            .expect("L3 检查");
        assert!(l3.enforced);
    }
}

#[test]
fn file_step_evidence_feeds_predicates() {
    let f = fixture();
    let steps = vec![
        step(
            "s1",
            "fs_write",
            json!({"path": "brief.md", "content": "项目评审会纪要：结论已确认。"}),
        ),
        step(
            "s2",
            "propose_email",
            json!({
                "to": ["li.ting@corp.example"],
                "subject": "纪要已发",
                "body": "纪要见附件 brief.md。",
            }),
        ),
    ];
    let contains = AcceptanceItem::Predicate {
        id: "brief-topic".into(),
        left: FieldRef {
            step: "s1".into(),
            field: "file.content".into(),
        },
        op: PredicateOp::Contains,
        right: Some(Operand::Literal {
            value: json!("项目评审会纪要"),
        }),
        normalize: None,
    };
    let plan = approved_plan(&f.db, steps, &[contains]);
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        None,
        AcceptanceEnforce::default(),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::Completed);
    let task = task_report(&f.db, &plan.id);
    let l2 = task
        .checks
        .iter()
        .find(|c| c.rule == "cross-step-v1:brief-topic")
        .expect("文件谓词检查");
    assert_eq!(l2.verdict, Verdict::Pass, "file.content 应可观测：{}", l2.reason);
    assert_eq!(task.shadow(), Verdict::Pass);
}

/// The shipped default (env unset) must gate on L2: the same
/// contradiction that Observe let through now stops the task.
/// Guards the 2026-11-23 flip against silent regression.
#[test]
fn shipped_default_enforces_l2() {
    let f = fixture();
    let plan = approved_plan(
        &f.db,
        event_mail_steps("2026-11-22T11:00:00+08:00"),
        &[time_predicate()],
    );
    let d = deps(
        &f.db,
        &f.registry,
        &f.executors,
        &f.gate,
        f.ws.path().to_path_buf(),
        None,
        AcceptanceEnforce::shipped_default(),
    );
    let report = drive(&f.db, &d, &plan.id, &f.executors);
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    let task = task_report(&f.db, &plan.id);
    assert_eq!(task.verdict, Verdict::Fail);
}
