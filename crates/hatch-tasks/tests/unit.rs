//! Integration tests for the M0 durable execution unit: lease-guarded
//! writes, per-call resume, and step/event atomicity.

use hatch_core::time::now;
use hatch_store::Db;
use hatch_tasks::{
    EventLog, ExecStatus, StepSpec, TaskStore, UnitCallSpec, UnitStore, DEFAULT_LEASE_TTL_SECS,
};
use serde_json::json;

fn setup() -> (Db, String, hatch_tasks::Lease) {
    let db = Db::open_in_memory().unwrap();
    let store = TaskStore::new(&db);
    let t = now();
    let task = store
        .create("worker", "简报", "daemon", "", 3, None, &t)
        .unwrap();
    let claimed = store
        .start(&task.id, "worker", DEFAULT_LEASE_TTL_SECS, &t)
        .unwrap();
    (db, task.id, claimed.lease().unwrap())
}

fn specs() -> Vec<StepSpec> {
    vec![
        StepSpec {
            step_id: "triage".into(),
            title: "分诊收件箱".into(),
            tool: "triage.run".into(),
            arguments: json!({"mailbox": "fixture"}),
        },
        StepSpec {
            step_id: "brief".into(),
            title: "组装简报".into(),
            tool: "brief.assemble".into(),
            arguments: json!({}),
        },
    ]
}

fn calls() -> Vec<UnitCallSpec> {
    vec![
        UnitCallSpec {
            id: "c1".into(),
            tool: "mail.read".into(),
            arguments: json!({"id": "m201"}),
        },
        UnitCallSpec {
            id: "c2".into(),
            tool: "mail.read".into(),
            arguments: json!({"id": "m202"}),
        },
        UnitCallSpec {
            id: "c3".into(),
            tool: "calendar.read".into(),
            arguments: json!({"day": "tomorrow"}),
        },
    ]
}

#[test]
fn lease_guards_all_unit_writes() {
    let (db, task_id, lease) = setup();
    let units = UnitStore::new(&db);
    let t = now();
    // A stale lease (wrong owner, wrong generation) writes nothing.
    let stale = hatch_tasks::Lease {
        owner: "intruder".into(),
        generation: 99,
    };
    assert!(units.ensure_steps(&task_id, &stale, &specs(), &t).is_err());
    assert!(units
        .begin_unit(&task_id, &stale, "triage", &calls(), &t)
        .is_err());
    let log = EventLog::new(&db);
    assert!(log
        .append_event(&task_id, &stale, "step_started", "triage", "x", &t)
        .is_err());
    assert!(units
        .record_result(&task_id, &stale, 1, "c1", "mail.read", "ok", "system", &t)
        .is_err());
    // The real lease works.
    let steps = units.ensure_steps(&task_id, &lease, &specs(), &t).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].step_id, "triage");
    assert_eq!(steps[0].status, ExecStatus::Pending);
    // ensure_steps is idempotent: no duplicate steps.
    let again = units.ensure_steps(&task_id, &lease, &specs(), &t).unwrap();
    assert_eq!(again.len(), 2);
}

#[test]
fn unit_resume_skips_completed_calls() {
    let (db, task_id, lease) = setup();
    let units = UnitStore::new(&db);
    let t = now();
    units.ensure_steps(&task_id, &lease, &specs(), &t).unwrap();
    let step = units.start_step(&task_id, &lease, "triage", &t).unwrap();
    assert_eq!(step.status, ExecStatus::Running);
    assert_eq!(step.attempts, 1);
    let unit = units
        .begin_unit(&task_id, &lease, "triage", &calls(), &t)
        .unwrap();
    assert_eq!(unit.unit_seq, 1);
    assert_eq!(unit.status, ExecStatus::Pending);
    // Record two of three results; the third stays pending.
    assert!(units
        .record_result(
            &task_id,
            &lease,
            1,
            "c1",
            "mail.read",
            "方案 B 通过",
            "system",
            &t
        )
        .unwrap());
    assert!(units
        .record_result(
            &task_id,
            &lease,
            1,
            "c2",
            "mail.read",
            "预算 5 万",
            "system",
            &t
        )
        .unwrap());
    let pending = units.pending_calls(&task_id, 1).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "c3");
    // Re-recording the same result is a no-op (dedup by primary key).
    assert!(!units
        .record_result(
            &task_id,
            &lease,
            1,
            "c1",
            "mail.read",
            "方案 B 通过",
            "system",
            &t
        )
        .unwrap());
    assert_eq!(units.unit_results(&task_id, 1).unwrap().len(), 2);
    // Resume position: still on triage, one call left.
    let pos = units.resume_position(&task_id).unwrap().unwrap();
    assert_eq!(pos.step_id, "triage");
    assert_eq!(pos.unit_seq, Some(1));
    assert_eq!(pos.pending_calls.len(), 1);
    // Finish the last call and the unit; then complete the step.
    units
        .record_result(
            &task_id,
            &lease,
            1,
            "c3",
            "calendar.read",
            "项目评审会",
            "system",
            &t,
        )
        .unwrap();
    assert!(units.pending_calls(&task_id, 1).unwrap().is_empty());
    units
        .finish_unit(&task_id, &lease, 1, ExecStatus::Done, 2, 3, &t)
        .unwrap();
    let (done, event) = units
        .complete_step(&task_id, &lease, "triage", ExecStatus::Done, "3 封已读", &t)
        .unwrap();
    assert_eq!(done.status, ExecStatus::Done);
    assert_eq!(event.kind, "step_done");
    assert_eq!(event.seq, 1);
    // Resume position moved to the next step (no unit yet).
    let pos = units.resume_position(&task_id).unwrap().unwrap();
    assert_eq!(pos.step_id, "brief");
    assert_eq!(pos.unit_seq, None);
}

#[test]
fn events_are_sequential_and_cursorable() {
    let (db, task_id, lease) = setup();
    let units = UnitStore::new(&db);
    let log = EventLog::new(&db);
    let t = now();
    units.ensure_steps(&task_id, &lease, &specs(), &t).unwrap();
    let e1 = log
        .append_event(&task_id, &lease, "task_started", "", "开始", &t)
        .unwrap();
    let e2 = log
        .append_event(&task_id, &lease, "step_started", "triage", "分诊开始", &t)
        .unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    let all = log.events_since(&task_id, 0, 100).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].kind, "task_started");
    // A reconnect from cursor 1 sees only the second event.
    let after = log.events_since(&task_id, 1, 100).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].kind, "step_started");
    assert!(log.events_since(&task_id, 2, 100).unwrap().is_empty());
}

#[test]
fn resume_position_is_none_when_all_steps_done() {
    let (db, task_id, lease) = setup();
    let units = UnitStore::new(&db);
    let t = now();
    units.ensure_steps(&task_id, &lease, &specs(), &t).unwrap();
    for step in ["triage", "brief"] {
        units.start_step(&task_id, &lease, step, &t).unwrap();
        units
            .complete_step(&task_id, &lease, step, ExecStatus::Done, "ok", &t)
            .unwrap();
    }
    assert_eq!(units.resume_position(&task_id).unwrap(), None);
}
