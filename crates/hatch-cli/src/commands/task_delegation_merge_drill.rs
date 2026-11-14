//! Drills for task 7 (durable delegation + parallel join):
//!
//! (c) write conflicts are recorded, never silently overwritten;
//! (d) partial failure — only the failed branch re-runs.
//!
//! (a) kill-at-boundaries and (b) cancel propagation live in
//! `task_delegation_drill`; the comparison lives in
//! `task_delegation_compare_drill`. Everything runs on the scripted
//! model / test doubles: no live model, no real mail.

#![cfg(test)]

use super::task_delegation_drill_helpers::{
    fail_child, finish_child, make_parent, now, resource_version, result, step_status,
};
use hatch_store::Db;
use hatch_tasks::{ChildSpec, DelegationStore, TaskContextStore};

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
