//! Drills for task 6 (c): a failure is classified at the executor
//! and routed at the worker — transient retries, permanent fails,
//! ambiguous blocks. Everything runs on the scripted tools / test
//! doubles: no live model, no real mail.

#![cfg(test)]

use super::task_invite::route_stopped_on_failure;
use hatch_core::time::Timestamp;
use hatch_llm::ToolSpec;
use hatch_plan::{
    latest_plan_approval, run_plan, FailureKind, PlanApproveExecutor, PlanStatus, PlanStep,
    PlanStore, RiskLevel, RunDeps, RunOutcome, RunReport, StepOutcome, StepStatus, StepVerifier,
    StepVerify,
};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{StepSpec, TaskStore, UnitStore};
use hatch_tools::builtin::default_registry;
use hatch_tools::{execute_approved, ExecutorRegistry, Registry, Tool, ToolContext, ToolOutput};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// A test tool that fails once with a transient error, then succeeds.
/// Registered in the plan executor's registry for test (c).
struct FlakyTool {
    calls: AtomicUsize,
}

impl Tool for FlakyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "flaky_read".into(),
            description: "test double: fails once, then succeeds".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> hatch_core::Result<ToolOutput> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(hatch_core::Error::Invalid("请求超时，请稍后重试".into()))
        } else {
            Ok(ToolOutput::system("flaky ok"))
        }
    }
}

/// A test tool that always fails permanently (a missing file).
struct AlwaysFailTool;

impl Tool for AlwaysFailTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "always_fail".into(),
            description: "test double: always fails permanently".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> hatch_core::Result<ToolOutput> {
        Err(hatch_core::Error::Invalid(
            "文件不存在：/x/missing.txt".into(),
        ))
    }
}

fn run_deps<'a>(
    db: &'a Db,
    reg: &'a Registry,
    exec: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    verify: Option<StepVerifier<'a>>,
    workspace_root: Option<std::path::PathBuf>,
) -> RunDeps<'a> {
    RunDeps {
        db,
        registry: reg,
        executors: exec,
        gate,
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@126.com".into(),
        workspace_root,
        snapshot_root: None,
        verify,
        output_map: None,
        user_name: String::new(),
    }
}

/// (c) The executor classifies the failure, and the worker routes on
/// it: transient → wait and retry (the step then succeeds),
/// permanent → the task fails, ambiguous → the task blocks.
#[test]
fn failure_classification_and_routing() {
    // Transient: a flaky tool fails once with a timeout, then works.
    let db = Db::open_in_memory().unwrap();
    let mut reg = default_registry();
    reg.register(Arc::new(FlakyTool {
        calls: AtomicUsize::new(0),
    }));
    reg.register(Arc::new(AlwaysFailTool));
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let plan = PlanStore::new(&db)
        .create(
            "test",
            "",
            "",
            vec![step("s1", "flaky_read")],
            false,
            &now(),
        )
        .unwrap();
    approve_plan(&db, &plan.id);
    let report = run_plan(
        &run_deps(&db, &reg, &exec, &gate, None, None),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps[0].failure_kind, Some(FailureKind::Transient));
    assert!(!report.steps[0].ambiguous);
    // Retry: the same plan runs again and the step succeeds.
    let report = run_plan(
        &run_deps(&db, &reg, &exec, &gate, None, None),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.steps[0].status, StepStatus::Done);
    assert_eq!(report.steps[0].failure_kind, None, "成功后失败类型已清");
    // Permanent: a missing file is not retryable.
    let plan = PlanStore::new(&db)
        .create(
            "test",
            "",
            "",
            vec![step("s1", "always_fail")],
            false,
            &now(),
        )
        .unwrap();
    approve_plan(&db, &plan.id);
    let report = run_plan(
        &run_deps(&db, &reg, &exec, &gate, None, None),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps[0].failure_kind, Some(FailureKind::Permanent));
    // Ambiguous: the verifier cannot tell whether the side effect
    // happened — never retried, always needs a human.
    let plan = PlanStore::new(&db)
        .create(
            "test",
            "",
            "",
            vec![PlanStep {
                arguments: json!({"query": "测试"}),
                ..step("s1", "memory_search")
            }],
            false,
            &now(),
        )
        .unwrap();
    approve_plan(&db, &plan.id);
    let verify = |_: &PlanStep, _: &str| StepVerify::Ambiguous("无法确认邮件是否已发出".into());
    let report = run_plan(
        &run_deps(&db, &reg, &exec, &gate, Some(&verify), None),
        &plan.id,
        &now(),
    )
    .unwrap();
    assert_eq!(report.outcome, RunOutcome::StoppedOnFailure);
    assert_eq!(report.steps[0].failure_kind, Some(FailureKind::NeedsHuman));
    assert!(report.steps[0].ambiguous);
}

/// (c, worker half) The routing itself, driven by a fabricated
/// report: transient waits, needs-human blocks, permanent fails.
#[test]
fn worker_routes_on_failure_kind() {
    let db = Db::open_in_memory().unwrap();
    let tasks = TaskStore::new(&db);
    let units = UnitStore::new(&db);
    for (kind, want) in [
        (FailureKind::Transient, "waiting"),
        (FailureKind::NeedsHuman, "blocked"),
        (FailureKind::Permanent, "failed"),
    ] {
        let task = tasks
            .create("test", "路由演练", "worker", "", 3, None, &now())
            .unwrap();
        let claimed = tasks.start(&task.id, "drill", 60, &now()).unwrap();
        let lease = claimed.lease().unwrap();
        units
            .ensure_steps(&task.id, &lease, &[step_spec("s1", "fs_read")], &now())
            .unwrap();
        let report = RunReport {
            plan_id: "plan_x".into(),
            outcome: RunOutcome::StoppedOnFailure,
            steps: vec![StepOutcome::failed("s1".into(), "失败".into(), kind)],
            deviation: None,
            snapshots: vec![],
        };
        let result = route_stopped_on_failure(&db, &task.id, &lease, &report, &chrono_tz::UTC, &now());
        let task = tasks.get(&task.id).unwrap().unwrap();
        assert_eq!(task.status.as_str(), want, "kind {kind:?} 应走向 {want}");
        match kind {
            FailureKind::Transient | FailureKind::NeedsHuman => {
                let outcome = result.unwrap();
                assert!(outcome.blocked.is_some());
                assert_eq!(outcome.blocked_step.as_deref(), Some("s1"));
            }
            FailureKind::Permanent => {
                assert!(result.is_err(), "永久失败应使任务失败并返回错误");
            }
        }
        let step = units.get_step(&task.id, "s1").unwrap().unwrap();
        assert_eq!(step.failure_kind, kind.as_str(), "失败类型应写回任务步骤");
    }
}
