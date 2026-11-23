//! Schedule tools end to end with a scripted model: hidden behind the
//! tool bridge, creation/deletion queue approvals and only land in
//! the job store after approval + execution, listing is a plain read.

#[path = "common/mod.rs"]
mod common;

use common::{call, Harness};
use hatch_agent::{Agent, AgentConfig, StopReason};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router};
use hatch_policy::{ApprovalQueue, ApprovalStatus, Gatekeeper, PolicyConfig};
use hatch_scheduler::JobStore;
use hatch_store::Db;
use hatch_tools::builtin::default_registry;
use hatch_tools::{execute_approved, CreateJobExecutor, DeleteJobExecutor, ExecutorRegistry};
use serde_json::json;
use std::sync::Arc;

fn harness() -> Harness {
    Harness {
        db: Db::open_in_memory().unwrap(),
        tools: default_registry(),
        exec: ExecutorRegistry::new()
            .with(Arc::new(CreateJobExecutor::new(
                vec!["agent_prompt".into()],
            )))
            .with(Arc::new(DeleteJobExecutor)),
        gate: Gatekeeper::new(PolicyConfig::default()),
    }
}

#[test]
fn schedule_tools_are_hidden_but_findable() {
    let h = harness();
    let agent = Agent {
        db: &h.db,
        router: &Router::single(Arc::new(ScriptedModel::ok("m", vec![]))),
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: AgentConfig::default(),
    };
    let names: Vec<String> = agent.tool_specs().iter().map(|s| s.name.clone()).collect();
    for hidden in ["schedule_create", "schedule_list", "schedule_delete"] {
        assert!(!names.contains(&hidden.to_string()), "{names:?}");
    }

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "find_tool",
                json!({"query": "create a scheduled cron job"}),
            )]),
            ChatResponse::text("找到了。"),
        ],
    ));
    let (r, s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "帮我建个定时任务",
    );
    assert_eq!(r.stop, StopReason::Answered);
    let found = h.tool_result(&s, "1");
    assert!(found.contains("schedule_create"), "{found}");
}

#[test]
fn create_queues_approval_and_lands_after_execution() {
    let h = harness();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "schedule_create", "arguments": {
                    "schedule": "daily 09:00",
                    "prompt": "提醒我喝水",
                    "name": "remind"
                }}),
            )]),
            ChatResponse::text("已提交，等你确认。"),
            // Turn-end completeness check intercepts the first finish.
            ChatResponse::text("已提交，等你确认。"),
        ],
    ));
    let (r, s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "每天九点提醒我喝水",
    );
    assert_eq!(r.stop, StopReason::Answered);
    assert_eq!(r.tool_calls, vec!["schedule_create"]);
    // Nothing exists yet: the proposal is waiting in the queue.
    assert!(JobStore::new(&h.db).find("remind").unwrap().is_none());
    assert_eq!(r.approvals.len(), 1, "{:?}", r.approvals);
    let queue = ApprovalQueue::new(&h.db);
    let approval = queue.get(&r.approvals[0]).unwrap();
    assert_eq!(approval.kind, "create_job");
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert!(
        approval.preview.contains("提醒我喝水"),
        "{}",
        approval.preview
    );
    let _ = h.tool_result(&s, "1");

    // Approve + execute: the job lands in the store with its prompt.
    let now = hatch_core::time::now();
    queue.decide(&approval.id, true, "", &now).unwrap();
    execute_approved(&h.db, &h.exec, &approval.id, &now).unwrap();
    let job = JobStore::new(&h.db).find("remind").unwrap().unwrap();
    assert_eq!(job.kind, "agent_prompt");
    assert_eq!(job.params["prompt"], "提醒我喝水");
    assert!(job.next_run_at.is_some());
    assert_eq!(
        queue.get(&approval.id).unwrap().status,
        ApprovalStatus::Executed
    );
}

#[test]
fn list_reads_and_delete_queues_approval() {
    let h = harness();
    let now = hatch_core::time::now();
    let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
    let mut job = hatch_scheduler::NewJob::new("remind", "agent_prompt", "daily 09:00");
    job.params = json!({"prompt": "提醒我喝水"});
    JobStore::new(&h.db).add(&job, &now, &tz).unwrap();

    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "schedule_list", "arguments": {}}),
            )]),
            ChatResponse::calls(vec![call(
                "2",
                "use_tool",
                json!({"name": "schedule_delete", "arguments": {"job": "remind"}}),
            )]),
            ChatResponse::text("删的申请已提交。"),
            // Turn-end completeness check intercepts the first finish.
            ChatResponse::text("删的申请已提交。"),
        ],
    ));
    let (r, s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "看看定时任务，再把 remind 删掉",
    );
    assert_eq!(r.stop, StopReason::Answered);
    let listed = h.tool_result(&s, "1");
    assert!(listed.contains("remind"), "{listed}");
    assert!(listed.contains("next_run_at"), "{listed}");

    // The delete is only a proposal until approved and executed.
    assert!(JobStore::new(&h.db).find("remind").unwrap().is_some());
    assert_eq!(r.approvals.len(), 1, "{:?}", r.approvals);
    let queue = ApprovalQueue::new(&h.db);
    let approval = queue.get(&r.approvals[0]).unwrap();
    assert_eq!(approval.kind, "delete_job");
    queue.decide(&approval.id, true, "", &now).unwrap();
    execute_approved(&h.db, &h.exec, &approval.id, &now).unwrap();
    assert!(JobStore::new(&h.db).find("remind").unwrap().is_none());
}

#[test]
fn rejected_create_never_lands() {
    let h = harness();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "1",
                "use_tool",
                json!({"name": "schedule_create", "arguments": {
                    "schedule": "every 30m",
                    "prompt": "看看邮箱",
                    "name": "mail-check"
                }}),
            )]),
            ChatResponse::text("已提交。"),
        ],
    ));
    let (r, _s) = h.run(
        Router::single(model),
        AgentConfig::default(),
        "每半小时看看邮箱",
    );
    let queue = ApprovalQueue::new(&h.db);
    let now = hatch_core::time::now();
    queue
        .decide(&r.approvals[0], false, "不用了", &now)
        .unwrap();
    assert!(execute_approved(&h.db, &h.exec, &r.approvals[0], &now).is_err());
    assert!(JobStore::new(&h.db).find("mail-check").unwrap().is_none());
}
