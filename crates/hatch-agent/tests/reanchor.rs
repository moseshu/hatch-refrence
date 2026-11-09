//! D2 end to end: a turn bound to a plan re-anchors every N steps and
//! stops on drift; unbound turns and inactive plans behave as before.

use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_core::time::{now, Timestamp};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_plan::{
    pending_plan_approval, Plan, PlanApproveExecutor, PlanStatus, PlanStep, PlanStore, RiskLevel,
    StepStatus,
};
use hatch_policy::{ApprovalQueue, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::{builtin::default_registry, execute_approved, ExecOutcome, ExecutorRegistry};
use serde_json::json;
use std::sync::Arc;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn step(id: &str, title: &str, tool: &str, args: serde_json::Value) -> PlanStep {
    PlanStep {
        id: id.into(),
        title: title.into(),
        tool: tool.into(),
        arguments: args,
        files: vec![],
        expected: format!("{title}完成"),
        risk: RiskLevel::Low,
        status: StepStatus::Pending,
        output: String::new(),
        approval_id: None,
    }
}

fn steps() -> Vec<PlanStep> {
    vec![
        step("s1", "查记忆 x", "memory_search", json!({"query": "x"})),
        step("s2", "查记忆 y", "memory_search", json!({"query": "y"})),
    ]
}

/// Create a plan and approve it through the queue, the way
/// `hatch plan approve` does.
fn approved_plan(db: &Db, exec: &ExecutorRegistry, now: &Timestamp) -> Plan {
    let plan = PlanStore::new(db)
        .create("整理资料", "资料齐整", "m", steps(), false, now)
        .unwrap();
    let entry = pending_plan_approval(db, &plan.id).unwrap().unwrap();
    ApprovalQueue::new(db)
        .decide(&entry.id, true, "ok", now)
        .unwrap();
    let out = execute_approved(db, exec, &entry.id, now).unwrap();
    assert!(
        matches!(out, ExecOutcome::Executed(_)),
        "expected Executed, got {out:?}"
    );
    PlanStore::new(db).get(&plan.id).unwrap().unwrap()
}

fn env() -> (Db, ExecutorRegistry) {
    let db = Db::open_in_memory().unwrap();
    let exec = ExecutorRegistry::new().with(Arc::new(PlanApproveExecutor::new("test")));
    (db, exec)
}

fn agent<'a>(
    db: &'a Db,
    router: &'a Router,
    exec: &'a ExecutorRegistry,
    gate: &'a Gatekeeper,
    tools: &'a hatch_tools::Registry,
    plan_id: Option<String>,
) -> Agent<'a> {
    Agent {
        db,
        router,
        tools,
        executors: exec,
        gate,
        config: AgentConfig {
            plan_id,
            ..Default::default()
        },
    }
}

#[test]
fn anchor_appears_on_anchor_steps_only() {
    let (db, exec) = env();
    let now = now();
    let plan = approved_plan(&db, &exec, &now);
    assert_eq!(plan.status, PlanStatus::Approved);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "x"}))]),
            ChatResponse::calls(vec![call("2", "memory_search", json!({"query": "y"}))]),
            ChatResponse::text("做完了"),
        ],
    ));
    let router = Router::single(model.clone());
    let gate = Gatekeeper::new(PolicyConfig::default());
    let tools = default_registry();
    let a = agent(&db, &router, &exec, &gate, &tools, Some(plan.id.clone()));
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "按计划整理", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    // Step 0 (anchor) and step 2 (anchor again, N=2) carry the plan;
    // step 1 does not.
    let sys = |i: usize| model.requests()[i].messages[0].content.clone();
    assert!(sys(0).contains("进行中的计划") && sys(0).contains("资料齐整"), "{}", sys(0));
    assert!(sys(0).contains("查记忆 x") && sys(0).contains("查记忆 x完成"), "{}", sys(0));
    assert!(!sys(1).contains("进行中的计划"), "{}", sys(1));
    assert!(sys(2).contains("进行中的计划"), "{}", sys(2));
    let p = r.plan.expect("bound turn reports coverage");
    assert_eq!(p.plan_id, plan.id);
    assert_eq!(p.covered, vec!["s1", "s2"]);
    assert!(p.uncovered.is_empty() && p.off_plan.is_empty() && p.drift.is_none());
    assert_eq!(
        PlanStore::new(&db).get(&plan.id).unwrap().unwrap().status,
        PlanStatus::Approved,
        "an on-plan turn leaves the plan alone"
    );
}

#[test]
fn off_plan_write_stops_and_flags_reapproval() {
    let (db, exec) = env();
    let now = now();
    let plan = approved_plan(&db, &exec, &now);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "x"}))]),
            ChatResponse::calls(vec![call("2", "memory_add", json!({"text": "私自记的"}))]),
            ChatResponse::text("不该到这里"),
        ],
    ));
    let router = Router::single(model);
    let gate = Gatekeeper::new(PolicyConfig::default());
    let tools = default_registry();
    let a = agent(&db, &router, &exec, &gate, &tools, Some(plan.id.clone()));
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "按计划整理", &now).unwrap();
    assert!(
        matches!(&r.stop, StopReason::PlanDeviation(m) if m.contains("改动调用")),
        "got {:?}",
        r.stop
    );
    let p = r.plan.as_ref().expect("drift turn reports coverage");
    assert_eq!(p.covered, vec!["s1"]);
    assert_eq!(p.uncovered, vec!["s2"]);
    assert_eq!(p.off_plan, vec!["memory_add"]);
    assert!(p.drift.as_ref().unwrap().contains("改动调用"));
    assert!(r.receipt().contains("偏离计划") && r.receipt().contains("计划覆盖：1 / 2 步"));
    let after = PlanStore::new(&db).get(&plan.id).unwrap().unwrap();
    assert_eq!(after.status, PlanStatus::NeedsReapproval);
    assert!(pending_plan_approval(&db, &plan.id).unwrap().is_some(), "a fresh approval was submitted");
    let (_, history) = SessionStore::new(&db).active(&s).unwrap();
    assert!(history.last().unwrap().message.content.contains("已偏离计划"));
}

#[test]
fn two_consecutive_off_plan_reads_stop() {
    let (db, exec) = env();
    let now = now();
    let plan = approved_plan(&db, &exec, &now);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_search", json!({"query": "nope"}))]),
            ChatResponse::calls(vec![call("2", "memory_search", json!({"query": "nah"}))]),
            ChatResponse::text("不该到这里"),
        ],
    ));
    let router = Router::single(model);
    let gate = Gatekeeper::new(PolicyConfig::default());
    let tools = default_registry();
    let a = agent(&db, &router, &exec, &gate, &tools, Some(plan.id.clone()));
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "按计划整理", &now).unwrap();
    assert!(
        matches!(&r.stop, StopReason::PlanDeviation(m) if m.contains("连续 2 次")),
        "got {:?}",
        r.stop
    );
    assert_eq!(
        PlanStore::new(&db).get(&plan.id).unwrap().unwrap().status,
        PlanStatus::NeedsReapproval
    );
}

#[test]
fn unbound_and_inactive_plans_run_as_before() {
    let (db, exec) = env();
    let now = now();
    let plan = approved_plan(&db, &exec, &now);
    let gate = Gatekeeper::new(PolicyConfig::default());
    let tools = default_registry();
    // No plan bound: an off-plan write is nobody's drift to flag.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_add", json!({"text": "私自记的"}))]),
            ChatResponse::text("做完了"),
        ],
    ));
    let router = Router::single(model);
    let a = agent(&db, &router, &exec, &gate, &tools, None);
    let s = SessionStore::new(&db).create("t", &now).unwrap();
    let r = a.run_turn(&s, "随便记点", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(r.plan.is_none());
    // A draft plan bound: inactive, so the turn runs unbound too.
    let draft = PlanStore::new(&db)
        .create("草稿任务", "", "m", steps(), true, &now)
        .unwrap();
    assert_eq!(draft.status, PlanStatus::Draft);
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "memory_add", json!({"text": "私自记的"}))]),
            ChatResponse::text("做完了"),
        ],
    ));
    let router = Router::single(model);
    let a = agent(&db, &router, &exec, &gate, &tools, Some(draft.id.clone()));
    let s = SessionStore::new(&db).create("t2", &now).unwrap();
    let r = a.run_turn(&s, "随便记点", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(r.plan.is_none());
    assert_eq!(
        PlanStore::new(&db).get(&draft.id).unwrap().unwrap().status,
        PlanStatus::Draft,
        "an inactive plan is never flagged by a turn"
    );
    assert_eq!(
        PlanStore::new(&db).get(&plan.id).unwrap().unwrap().status,
        PlanStatus::Approved,
        "the approved plan was never bound here"
    );
    // A missing plan id binds to nothing.
    let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text("做完了")]));
    let router = Router::single(model);
    let a = agent(&db, &router, &exec, &gate, &tools, Some("plan_nope".into()));
    let s = SessionStore::new(&db).create("t3", &now).unwrap();
    let r = a.run_turn(&s, "hi", &now).unwrap();
    assert_eq!(r.stop, StopReason::Answered);
    assert!(r.plan.is_none());
}
