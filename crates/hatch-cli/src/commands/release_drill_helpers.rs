//! Shared test doubles for the task 9 release drills.
//! Test-only; not part of the shipped binary.

#![cfg(test)]

use hatch_core::time::{now as now_ts, Timestamp};
use hatch_llm::ToolSpec;
use hatch_plan::{
    latest_plan_approval, PlanApproveExecutor, PlanStatus, PlanStep, PlanStore, RiskLevel, RunDeps,
    StepStatus, StepVerifier,
};
use hatch_policy::{ActionClass, ApprovalQueue, ApprovalStatus, Gatekeeper};
use hatch_tools::{ExecutorRegistry, Registry, Tool, ToolContext, ToolOutput};
use serde_json::json;
use std::sync::Arc;

pub(crate) fn now() -> Timestamp {
    now_ts()
}

pub(crate) fn step(id: &str, tool: &str) -> PlanStep {
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

pub(crate) fn approve_plan(db: &hatch_store::Db, plan_id: &str) {
    let plan = PlanStore::new(db).get(plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::PendingApproval);
    let entry = latest_plan_approval(db, &plan)
        .unwrap()
        .expect("plan approval entry");
    let status = ApprovalQueue::new(db)
        .decide(&entry.id, true, "drill approve plan", &now())
        .unwrap();
    assert_eq!(status, ApprovalStatus::Approved);
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("drill")));
    hatch_tools::execute_approved(db, &exec, &entry.id, &now()).unwrap();
}

pub(crate) fn run_deps<'a>(
    db: &'a hatch_store::Db,
    reg: &'a Registry,
    exec: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    verify: Option<StepVerifier<'a>>,
) -> RunDeps<'a> {
    RunDeps {
        db,
        registry: reg,
        executors: exec,
        gate,
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@example.invalid".into(),
        workspace_root: None,
        snapshot_root: None,
        verify,
        output_map: None,
        user_name: String::new(),
    }
}

/// A tool that always succeeds, so the verifier is the only
/// thing that can fail the step.
pub(crate) struct OkTool;

impl Tool for OkTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ok_read".into(),
            description: "test double: always succeeds".into(),
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
        Ok(ToolOutput::system("ok"))
    }
}

/// A tool that always fails at call time with a permanent
/// error: the tool layer itself is the fault.
pub(crate) struct FailTool;

impl Tool for FailTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fail_read".into(),
            description: "test double: always fails".into(),
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
        Err(hatch_core::Error::Invalid("读取失败：文件不存在".into()))
    }
}
