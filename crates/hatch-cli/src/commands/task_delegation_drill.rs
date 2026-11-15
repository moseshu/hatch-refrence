//! Drills for task 7 (durable delegation + parallel join):
//!
//! (a) kill after the handoff, before the consume, and after the
//!     consume — the parent advances exactly once;
//! (b) cancel propagation (receipts survive).
//!
//! (c) write conflicts and (d) partial failure live in
//! `task_delegation_merge_drill`; the comparison
//! (delegated-parallel vs single-agent) lives in
//! `task_delegation_compare_drill`.
//!
//! (e) P1: a chat-delegated subagent run bound to a ledger subtask
//! is reconciled into the ledger exactly once. Everything runs on
//! the scripted model / test doubles: no live model, no real mail.

#![cfg(test)]

use super::task_control::cancel_task;
use super::task_delegate_reconcile::report_finished_bound_runs;
use super::task_delegation_drill_helpers::{
    consumption_count, finish_child, make_parent, now, resource_version, result, step_done_events,
    step_status,
};
use hatch_agent::subagent::store::RunResult;
use hatch_agent::subagent::{RunStatus, RunStore};
use hatch_store::Db;
use hatch_tasks::{
    ChildSpec, DelegationStatus, DelegationStore, ExecStatus, StepSpec, TaskContextStore,
    TaskStore, UnitStore,
};
use serde_json::json;
use tempfile::TempDir;

/// (a) Kill after the handoff, before the consume, and after the
/// consume — the parent advances exactly once: one step_done event
/// per step, each evidence merged once, the marker written once.
#[test]
fn kill_at_handoff_consume_boundaries_advances_parent_once() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hatch.db");
    let db = Db::open(&path).unwrap();
    let (parent, lease) = make_parent(&db, &["s1", "s2"]);
    let delegations = DelegationStore::new(&db);
    let d1 = delegations
        .spawn(
            &parent,
            "s1",
            ChildSpec::new("meeting-brief", "分支一", "drill", 0),
            &now(),
        )
        .unwrap();
    let d2 = delegations
        .spawn(
            &parent,
            "s2",
            ChildSpec::new("meeting-brief", "分支二", "drill", 0),
            &now(),
        )
        .unwrap();
    finish_child(&db, &d1.child_task);
    finish_child(&db, &d2.child_task);
    delegations
        .report_result(
            &d1.child_task,
            &result("分支一完成", &["m1"], vec![("a.md", "", "h1")]),
            "",
            &now(),
        )
        .unwrap();
    delegations
        .report_result(
            &d2.child_task,
            &result("分支二完成", &["m2"], vec![("b.md", "", "h2")]),
            "",
            &now(),
        )
        .unwrap();
    // KILL 1: the process dies after the handoff, before any consume.
    drop(db);
    let db = Db::open(&path).unwrap();
    let outcome = DelegationStore::new(&db)
        .consume_ready(&parent, Some(&lease), &now())
        .unwrap();
    assert_eq!(outcome.consumed.len(), 2, "两子任务都应被消费");
    assert!(outcome.conflicts.is_empty());
    assert_eq!(step_status(&db, &parent, "s1"), "done");
    assert_eq!(step_status(&db, &parent, "s2"), "done");
    assert_eq!(step_done_events(&db, &parent, "s1"), 1, "s1 只推进一次");
    assert_eq!(step_done_events(&db, &parent, "s2"), 1, "s2 只推进一次");
    assert_eq!(consumption_count(&db, &parent), 2);
    let ctx = TaskContextStore::new(&db).load(&parent).unwrap().unwrap();
    assert_eq!(ctx.evidence.len(), 2, "证据各合并一次");
    assert_eq!(resource_version(&db, &parent, "a.md").unwrap().0, "h1");
    assert_eq!(resource_version(&db, &parent, "b.md").unwrap().0, "h2");
    // KILL 2: the process dies after the consume; a replay is a no-op.
    drop(db);
    let db = Db::open(&path).unwrap();
    let replay = DelegationStore::new(&db)
        .consume_ready(&parent, Some(&lease), &now())
        .unwrap();
    assert!(replay.consumed.is_empty(), "重放不应重复消费");
    assert_eq!(replay.already.len(), 2, "两个都已消费过");
    assert_eq!(step_done_events(&db, &parent, "s1"), 1, "重放不推进步骤");
    assert_eq!(step_done_events(&db, &parent, "s2"), 1, "重放不推进步骤");
    assert_eq!(consumption_count(&db, &parent), 2, "标记仍是两个");
    let ctx = TaskContextStore::new(&db).load(&parent).unwrap().unwrap();
    assert_eq!(ctx.evidence.len(), 2, "重放不重复合并证据");
    // A direct double consume of one child is the same no-op.
    let one = DelegationStore::new(&db)
        .consume_one(&parent, &d1.child_task, Some(&lease), &now())
        .unwrap();
    assert_eq!(one.already, vec![d1.child_task]);
    assert!(one.consumed.is_empty());
}

/// (b) Cancel propagates to every non-terminal child; a child's
/// completed step and its receipt stay readable for reconciliation.
#[test]
fn cancel_propagates_to_children_and_keeps_receipts() {
    let db = Db::open_in_memory().unwrap();
    let (parent, _lease) = make_parent(&db, &["s1", "s2"]);
    let delegations = DelegationStore::new(&db);
    let d1 = delegations
        .spawn(
            &parent,
            "s1",
            ChildSpec::new("meeting-brief", "分支一", "drill", 0),
            &now(),
        )
        .unwrap();
    let d2 = delegations
        .spawn(
            &parent,
            "s2",
            ChildSpec::new("meeting-brief", "分支二", "drill", 0),
            &now(),
        )
        .unwrap();
    // Child 1 is running and already submitted one external action
    // (a done step with a receipt); child 2 never started.
    let store = TaskStore::new(&db);
    let claimed = store.start(&d1.child_task, "drill", 300, &now()).unwrap();
    let lease1 = claimed.lease().unwrap();
    UnitStore::new(&db)
        .ensure_steps(
            &d1.child_task,
            &lease1,
            &[StepSpec {
                step_id: "send".to_string(),
                title: "发邮件".to_string(),
                tool: "mail.send".to_string(),
                arguments: json!({}),
                expected: "已发送".to_string(),
            }],
            &now(),
        )
        .unwrap();
    UnitStore::new(&db)
        .complete_step(
            &d1.child_task,
            &lease1,
            "send",
            ExecStatus::Done,
            "已发送（供应商回执 rcpt-1）",
            &now(),
        )
        .unwrap();
    UnitStore::new(&db)
        .set_step_receipt(&d1.child_task, &lease1, "send", "rcpt-1", &now())
        .unwrap();
    let msg = cancel_task(&db, &parent, "用户取消", &now()).unwrap();
    assert!(msg.contains("2 个子任务已取消"), "应取消两个子任务：{msg}");
    let store = TaskStore::new(&db);
    assert_eq!(
        store.get(&d1.child_task).unwrap().unwrap().status,
        hatch_tasks::TaskStatus::Cancelled
    );
    assert_eq!(
        store.get(&d2.child_task).unwrap().unwrap().status,
        hatch_tasks::TaskStatus::Cancelled
    );
    // The submitted action's receipt survives the cancel.
    let step = UnitStore::new(&db)
        .get_step(&d1.child_task, "send")
        .unwrap()
        .unwrap();
    assert_eq!(step.status, ExecStatus::Done);
    assert_eq!(step.receipt_id, "rcpt-1");
    // The parent records the propagation.
    let events = hatch_tasks::EventLog::new(&db)
        .events_since(&parent, 0, 100)
        .unwrap();
    assert!(
        events.iter().any(|e| e.kind == "children_cancelled"),
        "父任务应有 children_cancelled 事件"
    );
}

/// (e) P1: a chat-delegated subagent run bound to a ledger subtask
/// (schema v21 `subagent_runs.subtask_id`) is reconciled into the
/// ledger exactly once; a cancelled run is skipped (cancel
/// propagation owns it); cancelling the parent cancels the
/// still-open delegation.
#[test]
fn bound_subagent_run_is_consumed_exactly_once() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hatch.db");
    let db = Db::open(&path).unwrap();
    let (parent, lease) = make_parent(&db, &["s1", "s2"]);
    let delegations = DelegationStore::new(&db);
    let d1 = delegations
        .spawn(&parent, "s1", ChildSpec::new("meeting-brief", "分支一", "drill", 0), &now())
        .unwrap();
    let d2 = delegations
        .spawn(&parent, "s2", ChildSpec::new("meeting-brief", "分支二", "drill", 0), &now())
        .unwrap();
    // The chat delegate tool bound each run to its child task id
    // (what `prepare_one` does via `bind_subtask`): one finished
    // with an answer, one cancelled.
    let runs = RunStore::new(&db);
    runs.create("run-1", "sess-parent", "sess-1", 1, "explore", "数文件", RunStatus::Running, &now())
        .unwrap();
    runs.bind_subtask("run-1", &d1.child_task).unwrap();
    runs.finish(
        "run-1",
        RunStatus::Completed,
        &RunResult { answer: "结论：文件共 3 个".into(), ..Default::default() },
    )
    .unwrap();
    runs.create("run-2", "sess-parent", "sess-2", 1, "explore", "数文件", RunStatus::Running, &now())
        .unwrap();
    runs.bind_subtask("run-2", &d2.child_task).unwrap();
    runs.finish("run-2", RunStatus::Cancelled, &RunResult::default()).unwrap();
    // Reconcile: the completed run reports into the ledger, the
    // cancelled one is skipped.
    report_finished_bound_runs(&db, &parent, &now()).unwrap();
    let d1r = delegations.get_by_child(&d1.child_task).unwrap().unwrap();
    assert_eq!(d1r.status, DelegationStatus::Done);
    let result = d1r.result.expect("完成的子任务应有结果");
    assert!(
        result.conclusions.iter().any(|c| c.contains("文件共 3 个")),
        "结果应含子代理的回答：{result:?}"
    );
    let d2r = delegations.get_by_child(&d2.child_task).unwrap().unwrap();
    assert!(!d2r.status.is_terminal(), "取消的运行不应上报：{:?}", d2r.status);
    // Consume exactly once: the first consume takes it, the second
    // reports it as already consumed.
    let outcome = delegations.consume_ready(&parent, Some(&lease), &now()).unwrap();
    assert_eq!(outcome.consumed.len(), 1, "应消费一个子任务");
    assert_eq!(step_status(&db, &parent, "s1"), "done");
    let outcome = delegations.consume_ready(&parent, Some(&lease), &now()).unwrap();
    assert!(outcome.consumed.is_empty(), "重复消费应为空");
    assert_eq!(outcome.already.len(), 1, "重复消费应记为已消费过");
    // Reconciling again is a no-op (the delegation is terminal).
    report_finished_bound_runs(&db, &parent, &now()).unwrap();
    // Cancel propagates to the still-open delegation.
    cancel_task(&db, &parent, "用户取消", &now()).unwrap();
    let d2r = delegations.get_by_child(&d2.child_task).unwrap().unwrap();
    assert_eq!(d2r.status, DelegationStatus::Cancelled);
}
