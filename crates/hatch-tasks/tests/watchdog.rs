//! Integration tests for the D7 task watchdog: liveness probing over
//! the D6 reconcile — stale `running` tasks converge, fresh ones are
//! left alone, and a probe never fails its caller.

use hatch_core::time::{now, to_rfc3339};
use hatch_store::{params, Db, DbResultExt};
use hatch_tasks::{probe_once, TaskStatus, TaskStore, WatchdogConfig};
use std::time::Duration;

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

/// A `running` task whose heartbeat is `age` old (created `age` ago,
/// then started so its heartbeat is the creation time).
fn running_task(store: &TaskStore, age: Duration, max_retries: u32) -> hatch_tasks::Task {
    let t = now() - age;
    let task = store.create("scheduler", "定时任务", "daemon", "", max_retries, None, &t).unwrap();
    store.start(&task.id, &t).unwrap()
}

fn set_heartbeat(db: &Db, id: &str, ts: &str) {
    db.conn()
        .execute(
            "UPDATE tasks SET last_heartbeat = ?2 WHERE id = ?1",
            params![id, ts],
        )
        .db()
        .unwrap();
}

#[test]
fn watchdog_defaults_are_documented() {
    let cfg = WatchdogConfig::default();
    assert_eq!(cfg.probe_interval_secs, 60);
    assert_eq!(cfg.stale_after_secs, 300);
}

#[test]
fn probe_revives_stale_running_with_retries_left() {
    let db = db();
    let store = TaskStore::new(&db);
    let task = running_task(&store, Duration::from_secs(400), 3);
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (1, 1, 0));
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Waiting);
}

#[test]
fn probe_fails_stale_running_when_retries_exhausted() {
    let db = db();
    let store = TaskStore::new(&db);
    let task = running_task(&store, Duration::from_secs(400), 0);
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (1, 0, 1));
    let got = store.get(&task.id).unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::Failed);
    assert!(got.finished_at.is_some());
}

#[test]
fn probe_leaves_fresh_running_alone() {
    let db = db();
    let store = TaskStore::new(&db);
    let task = running_task(&store, Duration::from_secs(30), 3);
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (1, 0, 0));
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Running);
}

#[test]
fn probe_leaves_other_states_alone_even_when_stale() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    // queued, waiting and blocked rows with ancient heartbeats.
    let queued = store.create("scheduler", "排队", "daemon", "", 3, None, &t).unwrap();
    let waiting = running_task(&store, Duration::from_secs(400), 3);
    let waiting = store.wait(&waiting.id, "等重试", &t).unwrap();
    let blocked = running_task(&store, Duration::from_secs(400), 3);
    let blocked = store.block(&blocked.id, "等审批", &t).unwrap();
    for id in [&queued.id, &waiting.id, &blocked.id] {
        set_heartbeat(&db, id, "2020-01-01T00:00:00Z");
    }
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (0, 0, 0));
    assert_eq!(store.get(&queued.id).unwrap().unwrap().status, TaskStatus::Queued);
    assert_eq!(store.get(&waiting.id).unwrap().unwrap().status, TaskStatus::Waiting);
    assert_eq!(store.get(&blocked.id).unwrap().unwrap().status, TaskStatus::Blocked);
}

#[test]
fn probe_treats_corrupt_heartbeat_as_stale() {
    let db = db();
    let store = TaskStore::new(&db);
    let task = running_task(&store, Duration::from_secs(10), 3);
    set_heartbeat(&db, &task.id, "not-a-timestamp");
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (1, 1, 0));
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Waiting);
}

#[test]
fn probe_on_empty_ledger_is_a_quiet_noop() {
    let db = db();
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (0, 0, 0));
    assert!(!report.changed());
}

#[test]
fn heartbeat_refresh_keeps_a_live_task_out_of_the_watchdog() {
    let db = db();
    let store = TaskStore::new(&db);
    // A task that started 400s ago but heartbeated just now is alive.
    let task = running_task(&store, Duration::from_secs(400), 3);
    let t = now();
    store.heartbeat(&task.id, &t).unwrap();
    assert_eq!(store.get(&task.id).unwrap().unwrap().last_heartbeat, to_rfc3339(&t));
    let report = probe_once(&db, 300);
    assert_eq!((report.examined, report.revived, report.failed), (1, 0, 0));
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Running);
}

#[test]
fn heartbeat_only_while_running_is_rejected() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let task = store.create("scheduler", "排队", "daemon", "", 3, None, &t).unwrap();
    assert!(store.heartbeat(&task.id, &t).is_err(), "queued must not heartbeat");
    let task = store.start(&task.id, &t).unwrap();
    assert!(store.heartbeat(&task.id, &t).is_ok());
    let task = store.finish(&task.id, &t).unwrap();
    assert!(store.heartbeat(&task.id, &t).is_err(), "done must not heartbeat");
}
