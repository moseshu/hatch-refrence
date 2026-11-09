//! [`TaskStore`]: tasks in SQLite, next to approvals and sessions.
//!
//! The store owns the state machine: every transition checks the current
//! status first (see [`TaskStatus::can_transition_to`]), so an illegal
//! move is rejected, never silently applied. `create` is idempotent on
//! `idempotency_key`: re-submitting the same key returns the existing row.
//!
//! Startup reconcile ([`TaskStore::reconcile`]) converges tasks a previous
//! process left in `running`:
//! * heartbeat fresh (younger than `stale_after_secs`) → still owned, leave it;
//! * heartbeat stale and retries left → `waiting` (the system retries it);
//! * heartbeat stale and retries exhausted → `failed` (finished, with a note).
//!
//! An unparseable heartbeat counts as stale — a corrupt timestamp must not
//! pin a task in `running` forever. `waiting` / `blocked` / `queued` are
//! never touched: `waiting` is already retryable, `blocked` waits on
//! something external by design, and `queued` never started. Reconcile is
//! best-effort at the call sites (a DB error never fails a turn or a
//! daemon start); it only returns `Err` to its direct callers, who decide.

use crate::task::{Task, TaskStatus};
use hatch_core::ids;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Db, DbResultExt, OptionalExtension};

/// A `running` task whose heartbeat is older than this is presumed dead
/// at startup reconcile. Thirty minutes: the daemon ticks in seconds and
/// a task doing real work is expected to heartbeat far more often (D7
/// defines the live cadence); the threshold is generous on purpose — the
/// reconcile is a startup safety net, not a liveness probe.
pub const DEFAULT_STALE_AFTER_SECS: u64 = 1800;

/// What [`TaskStore::reconcile`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// `running` tasks examined.
    pub examined: usize,
    /// Stale tasks put back to `waiting` for a retry.
    pub revived: usize,
    /// Stale tasks failed (retries exhausted).
    pub failed: usize,
}

impl ReconcileReport {
    /// Whether anything converged (worth logging at startup).
    pub fn changed(&self) -> bool {
        self.revived + self.failed > 0
    }
}

const COLS: &str = "id, kind, title, owner, parent_task, status, retries, max_retries, last_heartbeat, idempotency_key, error, created_at, updated_at, finished_at";

fn row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        kind: r.get(1)?,
        title: r.get(2)?,
        owner: r.get(3)?,
        parent_task: r.get(4)?,
        status: TaskStatus::parse(&r.get::<_, String>(5)?),
        retries: r.get::<_, i64>(6)? as u32,
        max_retries: r.get::<_, i64>(7)? as u32,
        last_heartbeat: r.get(8)?,
        idempotency_key: r.get::<_, Option<String>>(9)?.filter(|k| !k.is_empty()),
        error: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        finished_at: r.get(13)?,
    })
}

pub struct TaskStore<'a> {
    db: &'a Db,
}

impl<'a> TaskStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        TaskStore { db }
    }

    /// Create a task in `queued`. When `idempotency_key` is given and a
    /// task with that key already exists, the existing row is returned
    /// and no new row is created — the same key submitted twice is one
    /// task, not two. `None` (or empty) means "no dedup": always a new row.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        kind: &str,
        title: &str,
        owner: &str,
        parent_task: &str,
        max_retries: u32,
        idempotency_key: Option<&str>,
        now: &Timestamp,
    ) -> Result<Task> {
        let key = idempotency_key.filter(|k| !k.is_empty());
        if let Some(k) = key {
            if let Some(existing) = self.get_by_key(k)? {
                return Ok(existing);
            }
        }
        let id = ids::random_id("tsk");
        let ts = to_rfc3339(now);
        self.db
            .conn()
            .execute(
                "INSERT INTO tasks(id, kind, title, owner, parent_task, status, retries, max_retries, last_heartbeat, idempotency_key, error, created_at, updated_at, finished_at) VALUES(?1,?2,?3,?4,?5,'queued',0,?6,?7,?8,'',?7,?7,NULL)",
                params![id, kind, title, owner, parent_task, max_retries as i64, ts, key],
            )
            .db()?;
        Ok(self.get(&id)?.expect("just-inserted task"))
    }

    pub fn get(&self, id: &str) -> Result<Option<Task>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!("SELECT {COLS} FROM tasks WHERE id = ?1"))
            .db()?;
        st.query_row(params![id], row).optional().db()
    }

    pub fn get_by_key(&self, key: &str) -> Result<Option<Task>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!("SELECT {COLS} FROM tasks WHERE idempotency_key = ?1"))
            .db()?;
        st.query_row(params![key], row).optional().db()
    }

    /// All tasks, optionally filtered by status, newest first.
    pub fn list(&self, status: Option<TaskStatus>, limit: usize) -> Result<Vec<Task>> {
        let conn = self.db.conn();
        let mut st = match status {
            Some(_) => conn.prepare(&format!(
                "SELECT {COLS} FROM tasks WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2"
            )).db()?,
            None => conn.prepare(&format!(
                "SELECT {COLS} FROM tasks ORDER BY created_at DESC LIMIT ?1"
            )).db()?,
        };
        let rows = match status {
            Some(s) => st.query_map(params![s.as_str(), limit as i64], row).db()?,
            None => st.query_map(params![limit as i64], row).db()?,
        };
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// Tasks that are not done: the set reconcile looks at.
    pub fn open(&self) -> Result<Vec<Task>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLS} FROM tasks WHERE status NOT IN ('done','failed') ORDER BY created_at"
            ))
            .db()?;
        let rows = st.query_map([], row).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// Children of one task, oldest first.
    pub fn children(&self, parent_task: &str) -> Result<Vec<Task>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLS} FROM tasks WHERE parent_task = ?1 ORDER BY created_at"
            ))
            .db()?;
        let rows = st.query_map(params![parent_task], row).db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }

    /// Enforce the exact source state for one transition. The generic
    /// table ([`TaskStatus::can_transition_to`]) cannot tell `start` from
    /// `retry` from `unblock` — all three land in `running` — so each
    /// method names its own source state here.
    fn must_be(&self, task: &Task, allowed: &[TaskStatus], action: &str) -> Result<()> {
        if !allowed.contains(&task.status) {
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，不能{action}",
                task.id,
                task.status.as_str(),
            )));
        }
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Task> {
        self.get(id)?
            .ok_or_else(|| Error::NotFound(format!("task {id} 不存在")))
    }

    fn set_status(&self, id: &str, to: TaskStatus, error: &str, now: &Timestamp) -> Result<Task> {
        let finished = if to.is_terminal() { Some(to_rfc3339(now)) } else { None };
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET status = ?2, error = ?3, updated_at = ?4, finished_at = COALESCE(?5, finished_at) WHERE id = ?1",
                params![id, to.as_str(), error, to_rfc3339(now), finished],
            )
            .db()?;
        self.load(id)
    }

    /// queued → running. Does not bump `retries` (first start, not a retry).
    pub fn start(&self, id: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Queued], "开始")?;
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET status = 'running', last_heartbeat = ?2, updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.load(id)
    }

    /// running → waiting. The system will pick it up again (via `retry`).
    pub fn wait(&self, id: &str, note: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "挂起等待")?;
        self.set_status(id, TaskStatus::Waiting, note, now)
    }

    /// running → blocked: waiting on something external (an approval, the
    /// user, a dependency). Reconcile never moves it back.
    pub fn block(&self, id: &str, reason: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "阻塞")?;
        self.set_status(id, TaskStatus::Blocked, reason, now)
    }

    /// blocked → running: the thing it waited on has arrived.
    pub fn unblock(&self, id: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Blocked], "解除阻塞")?;
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET status = 'running', last_heartbeat = ?2, updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.load(id)
    }

    /// waiting → running. Bumps `retries`: this is a restart after waiting,
    /// which is what the retry budget counts.
    pub fn retry(&self, id: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Waiting], "重试")?;
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET status = 'running', retries = retries + 1, last_heartbeat = ?2, updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.load(id)
    }

    /// running → done.
    pub fn finish(&self, id: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "完成")?;
        self.set_status(id, TaskStatus::Done, "", now)
    }

    /// queued / running / waiting / blocked → failed. Terminal, with the
    /// reason recorded and `finished_at` set.
    pub fn fail(&self, id: &str, error: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(
            &task,
            &[TaskStatus::Queued, TaskStatus::Running, TaskStatus::Waiting, TaskStatus::Blocked],
            "标记失败",
        )?;
        let done = self.set_status(id, TaskStatus::Failed, error, now)?;
        let _ = self.db.audit(
            "task",
            "task_failed",
            &format!("{}: {error}", task.id),
        );
        Ok(done)
    }

    /// Refresh the heartbeat of a running task: proof its owner is alive.
    /// Only `running` tasks heartbeat — a queued task never started and a
    /// waiting / blocked one is paused by design. D7 owns live heartbeat
    /// updates and liveness probing; D6 only stores the field.
    pub fn heartbeat(&self, id: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        if task.status != TaskStatus::Running {
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，不能心跳（只有执行中的任务心跳）",
                task.id,
                task.status.as_str()
            )));
        }
        self.db
            .conn()
            .execute(
                "UPDATE tasks SET last_heartbeat = ?2, updated_at = ?2 WHERE id = ?1",
                params![id, to_rfc3339(now)],
            )
            .db()?;
        self.load(id)
    }

    /// Converge tasks a previous process left in `running` (see module
    /// docs for the rules). Returns what it did; `Ok` with zero counts
    /// is the common case (nothing stale). Best-effort per task: one bad
    /// row never stops the rest.
    pub fn reconcile(&self, now: &Timestamp, stale_after_secs: u64) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        for task in self.open()? {
            if task.status != TaskStatus::Running {
                continue;
            }
            report.examined += 1;
            let stale = match parse_rfc3339(&task.last_heartbeat) {
                Ok(t) => now.signed_duration_since(t).num_seconds() >= stale_after_secs as i64,
                Err(_) => true,
            };
            if !stale {
                continue;
            }
            if task.retries < task.max_retries {
                let note = format!(
                    "心跳过期（上次心跳 {}），进程可能已崩溃；放回等待重试",
                    task.last_heartbeat
                );
                if self.wait(&task.id, &note, now).is_ok() {
                    report.revived += 1;
                    let _ = self.db.audit("task", "task_reconciled", &format!("{}: 运行中记录已放回等待", task.id));
                }
            } else {
                let note = format!(
                    "心跳过期（上次心跳 {}）且重试次数已用完（{}/{}）",
                    task.last_heartbeat, task.retries, task.max_retries
                );
                if self.fail(&task.id, &note, now).is_ok() {
                    report.failed += 1;
                    let _ = self.db.audit("task", "task_reconciled", &format!("{}: 运行中记录已标记失败", task.id));
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;

    #[test]
    fn create_defaults_to_queued() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let t = now();
        let task = store.create("worker", "发日报", "daemon", "", 3, None, &t).unwrap();
        assert!(task.id.starts_with("tsk_"));
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.retries, 0);
        assert_eq!(task.max_retries, 3);
        assert_eq!(task.last_heartbeat, to_rfc3339(&t));
        assert_eq!(task.finished_at, None);
        assert_eq!(task.idempotency_key, None);
    }
}
