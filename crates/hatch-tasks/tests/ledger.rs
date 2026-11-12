//! Integration tests for the D6 task ledger: transitions, idempotency,
//! and startup reconcile.

use hatch_core::time::{now, to_rfc3339, Timestamp};
use hatch_store::{params, Db, DbResultExt};
use hatch_tasks::{BlockedKind, Lease, TaskStatus, TaskStore};
use std::time::Duration;

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

fn create(store: &TaskStore, now: &Timestamp) -> hatch_tasks::Task {
    store.create("worker", "发日报", "daemon", "", 3, None, now).unwrap()
}

/// Claim a task the way a worker does: owner `worker`, 60s lease.
fn claim(store: &TaskStore, id: &str, now: &Timestamp) -> hatch_tasks::Task {
    store.start(id, "worker", 60, now).unwrap()
}

#[test]
fn create_defaults_and_get_roundtrip() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let task = create(&store, &t);
    assert!(task.id.starts_with("tsk_"));
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.retries, 0);
    assert_eq!(task.max_retries, 3);
    assert_eq!(task.last_heartbeat, to_rfc3339(&t));
    assert_eq!(task.finished_at, None);
    assert_eq!(task.idempotency_key, None);
    // M0: no lease yet, not blocked.
    assert_eq!(task.lease_owner, "");
    assert_eq!(task.lease_generation, 0);
    assert_eq!(task.blocked_kind, BlockedKind::None);
    assert_eq!(task.lease(), None);
    let got = store.get(&task.id).unwrap().unwrap();
    assert_eq!(got, task);
    assert!(store.get("tsk_nope").unwrap().is_none());
}

#[test]
fn idempotency_key_returns_existing_row() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let first = store
        .create("worker", "发日报", "daemon", "", 3, Some("daily-2026-11-09"), &t)
        .unwrap();
    let second = store
        .create("worker", "发日报（重提）", "daemon", "", 3, Some("daily-2026-11-09"), &t)
        .unwrap();
    assert_eq!(first.id, second.id, "same key = same task, no new row");
    assert_eq!(store.list(None, 100).unwrap().len(), 1);
    assert_eq!(
        store.get_by_key("daily-2026-11-09").unwrap().unwrap().id,
        first.id
    );
    // A different key is a different task; no key is never deduped.
    store.create("worker", "另一件", "daemon", "", 3, Some("other"), &t).unwrap();
    store.create("worker", "无键一", "daemon", "", 3, None, &t).unwrap();
    store.create("worker", "无键二", "daemon", "", 3, None, &t).unwrap();
    assert_eq!(store.list(None, 100).unwrap().len(), 4);
    assert!(store.get_by_key("missing").unwrap().is_none());
}

#[test]
fn happy_path_transitions() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let task = create(&store, &t);
    let task = claim(&store, &task.id, &t);
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.retries, 0, "start does not bump retries");
    assert_eq!(task.lease_generation, 1, "start issues lease generation 1");
    let task = store.wait(&task.id, task.lease().as_ref(), "等上游", &t).unwrap();
    assert_eq!(task.status, TaskStatus::Waiting);
    assert_eq!(task.error, "等上游");
    let task = store.retry(&task.id, "worker", 60, &t).unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.retries, 1, "retry bumps retries");
    assert_eq!(task.lease_generation, 2, "retry issues a fresh lease");
    let task = store
        .block(&task.id, task.lease().as_ref(), BlockedKind::Approval, "等用户确认", &t)
        .unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_kind, BlockedKind::Approval);
    let task = store.unblock(&task.id, "worker", 60, &t).unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.blocked_kind, BlockedKind::None);
    assert_eq!(task.lease_generation, 3, "unblock issues a fresh lease");
    let task = store.finish(&task.id, task.lease().as_ref(), &t).unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert!(task.finished_at.is_some());
    // A queued task can be failed before it ever starts (control plane,
    // no lease).
    let other = create(&store, &t);
    let other = store.fail(&other.id, None, "建出来就发现不用做了", &t).unwrap();
    assert_eq!(other.status, TaskStatus::Failed);
    assert!(other.finished_at.is_some());
}

#[test]
fn illegal_transitions_rejected() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    // A lease that matches nothing: the state check fires first for
    // these, so it is only there to satisfy the signature.
    let dummy = Lease { owner: "nobody".into(), generation: 99 };
    let task = create(&store, &t);
    // Skips out of queued.
    assert!(store.finish(&task.id, None, &t).is_err());
    assert!(store.wait(&task.id, None, "x", &t).is_err());
    assert!(store.block(&task.id, None, BlockedKind::Approval, "x", &t).is_err());
    assert!(store.retry(&task.id, "worker", 60, &t).is_err());
    assert!(store.unblock(&task.id, "worker", 60, &t).is_err());
    assert!(store.heartbeat(&task.id, &dummy, &t).is_err());
    // Double start.
    let task = claim(&store, &task.id, &t);
    assert!(store.start(&task.id, "worker", 60, &t).is_err());
    // Terminal states admit nothing.
    let done = store.finish(&task.id, task.lease().as_ref(), &t).unwrap();
    assert!(store.start(&done.id, "worker", 60, &t).is_err());
    assert!(store.retry(&done.id, "worker", 60, &t).is_err());
    assert!(store.heartbeat(&done.id, &dummy, &t).is_err());
    assert!(store.fail(&done.id, None, "x", &t).is_err());
    // waiting → done / blocked are not legal; blocked → waiting is not.
    let w = create(&store, &t);
    let w = claim(&store, &w.id, &t);
    store.wait(&w.id, w.lease().as_ref(), "等", &t).unwrap();
    assert!(store.finish(&w.id, None, &t).is_err());
    assert!(store.block(&w.id, None, BlockedKind::Approval, "x", &t).is_err());
    let b = create(&store, &t);
    let b = claim(&store, &b.id, &t);
    store
        .block(&b.id, b.lease().as_ref(), BlockedKind::Approval, "等", &t)
        .unwrap();
    assert!(store.wait(&b.id, None, "x", &t).is_err());
}

#[test]
fn heartbeat_only_while_running() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let task = create(&store, &t);
    let later = t + Duration::from_secs(60);
    let task = claim(&store, &task.id, &t);
    let task = store.heartbeat(&task.id, &task.lease().unwrap(), &later).unwrap();
    assert_eq!(task.last_heartbeat, to_rfc3339(&later));
}

#[test]
fn reconcile_revives_stale_running_with_retries_left() {
    let db = db();
    let store = TaskStore::new(&db);
    let old = now() - Duration::from_secs(7200);
    let t = now();
    let stale = store.create("worker", "旧任务", "daemon", "", 3, None, &old).unwrap();
    claim(&store, &stale.id, &old);
    // Fresh heartbeat: untouched.
    let fresh = create(&store, &t);
    claim(&store, &fresh.id, &t);
    let report = store.reconcile(&t, 1800).unwrap();
    assert_eq!(report.examined, 2);
    assert_eq!(report.revived, 1);
    assert_eq!(report.failed, 0);
    assert!(report.changed());
    let got = store.get(&stale.id).unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::Waiting);
    assert!(got.error.contains("心跳过期"));
    assert_eq!(store.get(&fresh.id).unwrap().unwrap().status, TaskStatus::Running);
}

#[test]
fn reconcile_fails_stale_running_when_retries_exhausted() {
    let db = db();
    let store = TaskStore::new(&db);
    let old = now() - Duration::from_secs(7200);
    let t = now();
    // max_retries 0: no retry left, so a stale running task fails outright.
    let task = store.create("worker", "重试耗尽", "daemon", "", 0, None, &old).unwrap();
    claim(&store, &task.id, &old);
    let report = store.reconcile(&t, 1800).unwrap();
    assert_eq!((report.examined, report.revived, report.failed), (1, 0, 1));
    let got = store.get(&task.id).unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::Failed);
    assert!(got.finished_at.is_some());
    assert!(got.error.contains("重试次数已用完"));
}

#[test]
fn reconcile_leaves_other_states_alone() {
    let db = db();
    let store = TaskStore::new(&db);
    let old = now() - Duration::from_secs(7200);
    let t = now();
    // waiting / blocked / queued with ancient heartbeats: reconcile must
    // not touch them — waiting is already retryable, blocked waits on
    // something external by design, queued never started.
    let w = create(&store, &old);
    let w = claim(&store, &w.id, &old);
    store.wait(&w.id, w.lease().as_ref(), "等", &old).unwrap();
    let b = create(&store, &old);
    let b = claim(&store, &b.id, &old);
    store
        .block(&b.id, b.lease().as_ref(), BlockedKind::Approval, "等用户", &old)
        .unwrap();
    let q = create(&store, &old);
    let report = store.reconcile(&t, 1800).unwrap();
    assert_eq!((report.examined, report.revived, report.failed), (0, 0, 0));
    assert!(!report.changed());
    assert_eq!(store.get(&w.id).unwrap().unwrap().status, TaskStatus::Waiting);
    assert_eq!(store.get(&b.id).unwrap().unwrap().status, TaskStatus::Blocked);
    assert_eq!(store.get(&q.id).unwrap().unwrap().status, TaskStatus::Queued);
}

#[test]
fn reconcile_treats_corrupt_heartbeat_as_stale() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let task = create(&store, &t);
    claim(&store, &task.id, &t);
    db.conn()
        .execute(
            "UPDATE tasks SET last_heartbeat = 'garbage' WHERE id = ?1",
            params![task.id],
        )
        .db()
        .unwrap();
    let report = store.reconcile(&t, 1800).unwrap();
    assert_eq!(report.revived, 1);
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Waiting);
}

#[test]
fn parent_children_link() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let parent = create(&store, &t);
    let c1 = store.create("subagent", "子一", "daemon", &parent.id, 3, None, &t).unwrap();
    let c2 = store.create("subagent", "子二", "daemon", &parent.id, 3, None, &t).unwrap();
    let kids = store.children(&parent.id).unwrap();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0].id, c1.id);
    assert_eq!(kids[1].id, c2.id);
    assert!(store.children("tsk_nope").unwrap().is_empty());
}

/// v11: a database from before the `tasks` table gains it on migrate
/// (migrate runs the full DDL, so a new table needs no ALTER path).
/// Lives here, not in hatch-store's tests: that file is at the 400-line
/// limit and this exercises the real migrate path end to end.
#[test]
fn schema_v11_creates_tasks_table_on_migrate() {
    let db = db();
    db.conn().execute("DROP TABLE tasks", []).db().unwrap();
    db.migrate().unwrap();
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let store = TaskStore::new(&db);
    let task = create(&store, &now());
    assert_eq!(store.get(&task.id).unwrap().unwrap().status, TaskStatus::Queued);
    // Migrate twice: idempotent.
    db.migrate().unwrap();
}

/// v12 (M0): a database whose `tasks` table still has the v11 shape
/// gains the lease columns + `blocked_kind` on migrate, and the old row
/// stays readable with the documented defaults (no lease, not blocked).
#[test]
fn schema_v12_adds_lease_columns_to_existing_tasks_table() {
    let db = db();
    let store = TaskStore::new(&db);
    let t = now();
    let old = create(&store, &t);
    db.conn().execute("DROP TABLE tasks", []).db().unwrap();
    db.conn()
        .execute(
            "CREATE TABLE tasks (
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, title TEXT NOT NULL,
                owner TEXT NOT NULL, parent_task TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL, retries INTEGER NOT NULL DEFAULT 0,
                max_retries INTEGER NOT NULL DEFAULT 3,
                last_heartbeat TEXT NOT NULL, idempotency_key TEXT UNIQUE,
                error TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, finished_at TEXT
            )",
            [],
        )
        .db()
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO tasks(id, kind, title, owner, parent_task, status, retries, max_retries, last_heartbeat, idempotency_key, error, created_at, updated_at, finished_at) VALUES(?1,'worker','发日报','daemon','', 'queued', 0, 3, ?2, NULL, '', ?2, ?2, NULL)",
            params![old.id, to_rfc3339(&t)],
        )
        .db()
        .unwrap();
    db.migrate().unwrap();
    let got = store.get(&old.id).unwrap().unwrap();
    assert_eq!(got.status, TaskStatus::Queued);
    assert_eq!(got.lease_owner, "");
    assert_eq!(got.lease_generation, 0);
    assert_eq!(got.blocked_kind, BlockedKind::None);
    // The new columns exist (migrate twice: idempotent).
    db.migrate().unwrap();
    let cols: Vec<String> = db
        .conn()
        .prepare("PRAGMA table_info(tasks)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for col in ["lease_owner", "lease_generation", "lease_ttl_secs", "lease_expires_at", "blocked_kind"] {
        assert!(cols.contains(&col.to_string()), "missing column {col}");
    }
}
