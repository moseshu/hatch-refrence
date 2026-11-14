//! Shared helpers for the task 7 drills: a parent join task, child
//! finish/fail, and the ledger reads the drills assert on. Used by
//! `task_delegation_drill` and `task_delegation_merge_drill`.

#![cfg(test)]

use hatch_core::time::Timestamp;
use hatch_store::Db;
use hatch_tasks::{ChildResult, EvidenceRef, FileWrite, StepSpec, TaskStore, UnitStore};
use serde_json::json;

pub(crate) fn now() -> Timestamp {
    hatch_core::time::now()
}

pub(crate) fn result(
    summary: &str,
    evidence: &[&str],
    writes: Vec<(&str, &str, &str)>,
) -> ChildResult {
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
pub(crate) fn make_parent(db: &Db, steps: &[&str]) -> (String, hatch_tasks::Lease) {
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
pub(crate) fn finish_child(db: &Db, child: &str) {
    let store = TaskStore::new(db);
    let claimed = store.start(child, "drill", 300, &now()).unwrap();
    store
        .finish(child, Some(&claimed.lease().unwrap()), &now())
        .unwrap();
}

/// Fail a child task with a reason.
pub(crate) fn fail_child(db: &Db, child: &str, reason: &str) {
    let store = TaskStore::new(db);
    let claimed = store.start(child, "drill", 300, &now()).unwrap();
    store
        .fail(child, Some(&claimed.lease().unwrap()), reason, &now())
        .unwrap();
}

pub(crate) fn step_status(db: &Db, task: &str, step: &str) -> String {
    UnitStore::new(db)
        .get_step(task, step)
        .unwrap()
        .map(|s| s.status.as_str().to_string())
        .unwrap_or_else(|| "missing".to_string())
}

pub(crate) fn step_done_events(db: &Db, task: &str, step: &str) -> usize {
    hatch_tasks::EventLog::new(db)
        .events_since(task, 0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "step_done" && e.step_id == step)
        .count()
}

pub(crate) fn consumption_count(db: &Db, parent: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM task_consumptions WHERE parent_task = ?1",
            hatch_store::params![parent],
            |r| r.get(0),
        )
        .unwrap()
}

pub(crate) fn resource_version(db: &Db, task: &str, resource: &str) -> Option<(String, String)> {
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
