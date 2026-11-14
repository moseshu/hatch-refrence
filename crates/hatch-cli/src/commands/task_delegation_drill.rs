//! Drills for task 7 (durable delegation + parallel join):
//!
//! (a) kill after the handoff, before the consume, and after the
//!     consume — the parent advances exactly once;
//! (b) cancel propagation (receipts survive);
//! (c) write conflicts are recorded, never silently overwritten;
//! (d) partial failure — only the failed branch re-runs.
//!
//! The comparison (delegated-parallel vs single-agent) lives in
//! `task_delegation_compare_drill`. Everything runs on the scripted
//! model / test doubles: no live model, no real mail.

#![cfg(test)]

use super::task_control::cancel_task;
use hatch_core::time::Timestamp;
use hatch_store::Db;
use hatch_tasks::{
    ChildResult, ChildSpec, DelegationStore, EvidenceRef, ExecStatus, FileWrite, StepSpec,
    TaskContextStore, TaskStore, UnitStore,
};
use serde_json::json;
use tempfile::TempDir;

fn now() -> Timestamp {
    hatch_core::time::now()
}

fn result(summary: &str, evidence: &[&str], writes: Vec<(&str, &str, &str)>) -> ChildResult {
    ChildResult {
        summary: summary.to_string(),
        evidence: evidence
            .iter()
            .map(|id| EvidenceRef::new("mail", id, "drill"))
            .collect(),
        conclusions: vec![format!("{summary}：已核实")],
        open_items: vec![],
        writes: writes
            .into_iter()
            .map(|(path, base, new)| FileWrite {
                path: path.to_string(),
                base_version: base.to_string(),
                new_version: new.to_string(),
                hash: format!("hash-{new}"),
            })
            .collect(),
    }
}

/// A parent join task with the given steps, claimed by the drill.
fn make_parent(db: &Db, steps: &[&str]) -> (String, hatch_tasks::Lease) {
    let store = TaskStore::new(db);
    let task = store
        .create(
            "delegate-join",
            "父任务：汇总三份调研",
            "drill",
            "",
            3,
            None,
            &now(),
        )
        .unwrap();
    let claimed = store.start(&task.id, "drill", 300, &now()).unwrap();
    let lease = claimed.lease().unwrap();
    let specs: Vec<StepSpec> = steps
        .iter()
        .map(|s| StepSpec {
            step_id: s.to_string(),
            title: format!("分支 {s}"),
            tool: "delegate.child".to_string(),
            arguments: json!({}),
            expected: "子任务完成并已合并".to_string(),
        })
        .collect();
    UnitStore::new(db)
        .ensure_steps(&task.id, &lease, &specs, &now())
        .unwrap();
    (task.id, lease)
}

/// Finish a child task (claim it, then finish it).
fn finish_child(db: &Db, child: &str) {
    let store = TaskStore::new(db);
    let claimed = store.start(child, "drill", 300, &now()).unwrap();
    store
        .finish(child, Some(&claimed.lease().unwrap()), &now())
        .unwrap();
}

/// Fail a child task with a reason.
fn fail_child(db: &Db, child: &str, reason: &str) {
    let store = TaskStore::new(db);
    let claimed = store.start(child, "drill", 300, &now()).unwrap();
    store
        .fail(child, Some(&claimed.lease().unwrap()), reason, &now())
        .unwrap();
}

fn step_status(db: &Db, task: &str, step: &str) -> String {
    UnitStore::new(db)
        .get_step(task, step)
        .unwrap()
        .map(|s| s.status.as_str().to_string())
        .unwrap_or_else(|| "missing".to_string())
}

fn step_done_events(db: &Db, task: &str, step: &str) -> usize {
    hatch_tasks::EventLog::new(db)
        .events_since(task, 0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "step_done" && e.step_id == step)
        .count()
}

fn consumption_count(db: &Db, parent: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM task_consumptions WHERE parent_task = ?1",
            hatch_store::params![parent],
            |r| r.get(0),
        )
        .unwrap()
}

fn resource_version(db: &Db, task: &str, resource: &str) -> Option<(String, String)> {
    use hatch_store::OptionalExtension;
    db.conn()
        .query_row(
            "SELECT version, updated_by FROM task_resource_versions WHERE task_id = ?1 AND resource = ?2",
            hatch_store::params![task, resource],
            |r| Ok((r.get::<_, String>(0).unwrap(), r.get::<_, String>(1).unwrap())),
        )
        .optional()
        .ok()
        .flatten()
}

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

/// (c) Two children write the same resource: the first applies, the
/// second is a conflict — recorded, never silently overwritten — and
/// the second child's evidence still merges.
#[test]
fn write_conflict_is_recorded_not_silently_overwritten() {
    let db = Db::open_in_memory().unwrap();
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
            &result("分支一完成", &["m1"], vec![("report.md", "", "v-a")]),
            "",
            &now(),
        )
        .unwrap();
    delegations
        .report_result(
            &d2.child_task,
            &result("分支二完成", &["m2"], vec![("report.md", "", "v-b")]),
            "",
            &now(),
        )
        .unwrap();
    let outcome = DelegationStore::new(&db)
        .consume_ready(&parent, Some(&lease), &now())
        .unwrap();
    assert_eq!(outcome.consumed.len(), 2, "两个子任务都被消费");
    assert_eq!(outcome.conflicts.len(), 1, "恰好一处冲突");
    let conflict = &outcome.conflicts[0];
    assert_eq!(conflict.path, "report.md");
    assert_eq!(conflict.current_version, "v-a");
    assert_eq!(conflict.current_writer, d1.child_task);
    assert_eq!(conflict.attempted_version, "v-b");
    // The first writer's version stands; the second never overwrote it.
    let (version, writer) = resource_version(&db, &parent, "report.md").unwrap();
    assert_eq!(version, "v-a");
    assert_eq!(writer, d1.child_task);
    // The conflict is recorded in the parent context, and the second
    // child's evidence still merged (only the write was refused).
    let ctx = TaskContextStore::new(&db).load(&parent).unwrap().unwrap();
    assert_eq!(ctx.evidence.len(), 2);
    assert!(
        ctx.open_items
            .iter()
            .any(|i| i.contains("report.md") && i.contains("v-b")),
        "冲突应记入未完成事项：{:?}",
        ctx.open_items
    );
}

/// (d) Partial failure: one branch done, one permanently failed, one
/// transiently failed. Only the transient branch re-runs; the parent
/// proceeds with what completed and the gap is recorded.
#[test]
fn partial_failure_reruns_only_the_failed_branch() {
    let db = Db::open_in_memory().unwrap();
    let (parent, lease) = make_parent(&db, &["a", "b", "c"]);
    let delegations = DelegationStore::new(&db);
    let da = delegations
        .spawn(
            &parent,
            "a",
            ChildSpec::new("meeting-brief", "分支甲", "drill", 0),
            &now(),
        )
        .unwrap();
    let db_ = delegations
        .spawn(
            &parent,
            "b",
            ChildSpec::new("meeting-brief", "分支乙", "drill", 0),
            &now(),
        )
        .unwrap();
    let dc = delegations
        .spawn(
            &parent,
            "c",
            ChildSpec::new("meeting-brief", "分支丙", "drill", 0),
            &now(),
        )
        .unwrap();
    finish_child(&db, &da.child_task);
    fail_child(&db, &db_.child_task, "参数错误，无法重试");
    fail_child(&db, &dc.child_task, "超时");
    delegations
        .report_result(
            &da.child_task,
            &result("甲完成", &["m-a"], vec![]),
            "",
            &now(),
        )
        .unwrap();
    delegations
        .report_result(
            &db_.child_task,
            &result("乙失败", &[], vec![]),
            "permanent",
            &now(),
        )
        .unwrap();
    delegations
        .report_result(
            &dc.child_task,
            &result("丙失败", &[], vec![]),
            "transient",
            &now(),
        )
        .unwrap();
    let consumed = DelegationStore::new(&db)
        .consume_ready(&parent, Some(&lease), &now())
        .unwrap();
    assert_eq!(consumed.consumed, vec![da.child_task.clone()]);
    let report = DelegationStore::new(&db)
        .handle_failed(&parent, Some(&lease), &now())
        .unwrap();
    assert_eq!(report.failed_branches, vec!["b".to_string()]);
    assert_eq!(report.respawned.len(), 1, "只补跑暂时失败的丙分支");
    assert!(report.blocked_reason.is_none());
    // A is untouched, B's step failed with the gap recorded, C has a
    // fresh child and the old delegation is superseded.
    assert_eq!(step_status(&db, &parent, "a"), "done");
    assert_eq!(step_status(&db, &parent, "b"), "failed");
    let list = DelegationStore::new(&db).list_for_parent(&parent).unwrap();
    let c_rows: Vec<_> = list.iter().filter(|d| d.step_id == "c").collect();
    assert_eq!(c_rows.len(), 2, "丙分支应有旧（superseded）+ 新两条记录");
    assert!(c_rows
        .iter()
        .any(|d| d.status == hatch_tasks::DelegationStatus::Superseded));
    let new_c = c_rows
        .iter()
        .find(|d| d.status == hatch_tasks::DelegationStatus::Delegated)
        .unwrap();
    assert_ne!(new_c.child_task, dc.child_task);
    let ctx = TaskContextStore::new(&db).load(&parent).unwrap().unwrap();
    assert!(
        ctx.open_items
            .iter()
            .any(|i| i.contains("分支 b") && i.contains("永久失败")),
        "乙分支的缺口应记入未完成事项：{:?}",
        ctx.open_items
    );
    // The re-spawned branch completes and consumes; A and B are not
    // touched again.
    finish_child(&db, &new_c.child_task);
    DelegationStore::new(&db)
        .report_result(
            &new_c.child_task,
            &result("丙补跑完成", &["m-c"], vec![]),
            "",
            &now(),
        )
        .unwrap();
    let second = DelegationStore::new(&db)
        .consume_ready(&parent, Some(&lease), &now())
        .unwrap();
    assert_eq!(second.consumed, vec![new_c.child_task.clone()]);
    assert_eq!(step_status(&db, &parent, "c"), "done");
    assert_eq!(step_status(&db, &parent, "a"), "done");
    assert_eq!(step_status(&db, &parent, "b"), "failed");
}
