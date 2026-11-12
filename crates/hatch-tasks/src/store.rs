//! [`TaskStore`]: tasks in SQLite, next to approvals and sessions.
//!
//! The store owns the state machine: every transition checks the current
//! status first (see [`TaskStatus::can_transition_to`]), so an illegal
//! move is rejected, never silently applied. `create` is idempotent on
//! `idempotency_key`: re-submitting the same key returns the existing row.
//!
//! Atomic claim, M0 (schema v12): `start` / `retry` / `unblock` issue
//! an execution lease — one UPDATE conditioned on the expected source
//! status (`WHERE id = ? AND status = 'queued'`), so two workers racing
//! to claim the same task cannot both win: the loser's UPDATE matches
//! zero rows. The lease (owner + generation + expiry) is then the
//! credential for every guarded write: `heartbeat` and the `running`
//! transitions (`finish` / `fail` / `wait` / `block`) take the caller's
//! [`Lease`] and their UPDATEs are conditioned on owner + generation,
//! so a stale owner (superseded by `reclaim` after its lease expired)
//! writes nothing. Control-plane callers (reconcile, user cancel) pass
//! `None` — they hold no lease and need none.
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

use crate::task::{BlockedKind, Lease, Task, TaskStatus};
use chrono::Duration;
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

const COLS: &str = "id, kind, title, owner, parent_task, status, retries, max_retries, last_heartbeat, idempotency_key, error, created_at, updated_at, finished_at, lease_owner, lease_generation, lease_ttl_secs, lease_expires_at, blocked_kind";

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
        lease_owner: r.get(14)?,
        lease_generation: r.get::<_, i64>(15)? as u64,
        lease_ttl_secs: r.get::<_, i64>(16)? as u64,
        lease_expires_at: r.get(17)?,
        blocked_kind: BlockedKind::parse(&r.get::<_, String>(18)?),
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
            .prepare(&format!(
                "SELECT {COLS} FROM tasks WHERE idempotency_key = ?1"
            ))
            .db()?;
        st.query_row(params![key], row).optional().db()
    }

    /// All tasks, optionally filtered by status, newest first.
    pub fn list(&self, status: Option<TaskStatus>, limit: usize) -> Result<Vec<Task>> {
        let conn = self.db.conn();
        let mut st = match status {
            Some(_) => conn
                .prepare(&format!(
                    "SELECT {COLS} FROM tasks WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2"
                ))
                .db()?,
            None => conn
                .prepare(&format!(
                    "SELECT {COLS} FROM tasks ORDER BY created_at DESC LIMIT ?1"
                ))
                .db()?,
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
                "SELECT {COLS} FROM tasks WHERE status NOT IN ('done','failed','cancelled') ORDER BY created_at"
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

    /// The atomic claim: one UPDATE conditioned on the expected source
    /// status, issuing a fresh lease (generation + 1, expiry = now +
    /// `ttl_secs`). Two workers racing to claim the same task cannot
    /// both win — the loser's UPDATE matches zero rows and this returns
    /// `Err`. `bump_retries` is true only for `retry` (a restart after
    /// waiting, which is what the retry budget counts). `blocked_kind`
    /// is `Some` only for `unblock`, which clears the blocking reason in
    /// the same UPDATE (one write, never a half-cleared row).
    /// The seven parameters are the claim's dimensions (identity,
    /// source state, lease, retry/boundary flags); a parameter struct
    /// would churn every transition caller for no gain.
    #[allow(clippy::too_many_arguments)]
    fn claim(
        &self,
        id: &str,
        from: TaskStatus,
        owner: &str,
        ttl_secs: u64,
        bump_retries: bool,
        blocked_kind: Option<BlockedKind>,
        now: &Timestamp,
    ) -> Result<Task> {
        let ts = to_rfc3339(now);
        let expires = to_rfc3339(&(*now + Duration::seconds(ttl_secs as i64)));
        let retries_sql = if bump_retries {
            ", retries = retries + 1"
        } else {
            ""
        };
        // Safe to splice: `as_str` is a fixed vocabulary string, never
        // user input (see `BlockedKind`).
        let blocked_sql = match blocked_kind {
            Some(k) => format!(", blocked_kind = '{}'", k.as_str()),
            None => String::new(),
        };
        let changed = self
            .db
            .conn()
            .execute(
                &format!(
                    "UPDATE tasks SET status = 'running', lease_owner = ?2, lease_generation = lease_generation + 1, lease_ttl_secs = ?3, lease_expires_at = ?4, last_heartbeat = ?5, updated_at = ?5{retries_sql}{blocked_sql} WHERE id = ?1 AND status = ?6"
                ),
                params![id, owner, ttl_secs as i64, expires, ts, from.as_str()],
            )
            .db()?;
        if changed == 0 {
            let task = self.load(id)?;
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，不能认领（需要 {}；可能已被其他 worker 认领）",
                task.id,
                task.status.as_str(),
                from.as_str(),
            )));
        }
        self.load(id)
    }

    /// A guarded status move out of `running`: the UPDATE is conditioned
    /// on the caller's lease (owner + generation) as well as the status,
    /// so a stale owner — one superseded by `reclaim` after its lease
    /// expired — matches zero rows and gets `Err` instead of overwriting
    /// its successor. `lease = None` is the control-plane path (reconcile,
    /// user cancel): no lease to check, the load + `must_be` above is the
    /// only guard, which is enough because those callers are single-owner
    /// by construction. `blocked_kind` is `Some` only for `block`, which
    /// records the reason in the same UPDATE.
    fn transition(
        &self,
        id: &str,
        to: TaskStatus,
        lease: Option<&Lease>,
        blocked_kind: Option<BlockedKind>,
        error: &str,
        now: &Timestamp,
    ) -> Result<Task> {
        let finished = if to.is_terminal() {
            Some(to_rfc3339(now))
        } else {
            None
        };
        let ts = to_rfc3339(now);
        // Safe to splice: `as_str` is a fixed vocabulary string, never
        // user input (see `BlockedKind`).
        let blocked_sql = match blocked_kind {
            Some(k) => format!(", blocked_kind = '{}'", k.as_str()),
            None => String::new(),
        };
        let changed = match lease {
            Some(l) => self
                .db
                .conn()
                .execute(
                    &format!(
                        "UPDATE tasks SET status = ?2, error = ?3, updated_at = ?4, finished_at = COALESCE(?5, finished_at){blocked_sql} WHERE id = ?1 AND status = 'running' AND lease_owner = ?6 AND lease_generation = ?7"
                    ),
                    params![id, to.as_str(), error, ts, finished, l.owner, l.generation as i64],
                )
                .db()?,
            None => self
                .db
                .conn()
                .execute(
                    &format!(
                        "UPDATE tasks SET status = ?2, error = ?3, updated_at = ?4, finished_at = COALESCE(?5, finished_at){blocked_sql} WHERE id = ?1"
                    ),
                    params![id, to.as_str(), error, ts, finished],
                )
                .db()?,
        };
        if changed == 0 {
            let task = self.load(id)?;
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，租约已失效或已被他人接管，不能转为 {}",
                task.id,
                task.status.as_str(),
                to.as_str(),
            )));
        }
        self.load(id)
    }

    /// queued → running. Issues a fresh lease for `owner` (TTL
    /// `ttl_secs`); does not bump `retries` (first start, not a retry).
    /// Atomic: a second worker claiming the same task gets `Err`.
    pub fn start(&self, id: &str, owner: &str, ttl_secs: u64, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Queued], "开始")?;
        self.claim(id, TaskStatus::Queued, owner, ttl_secs, false, None, now)
    }

    /// running → waiting. The system will pick it up again (via `retry`).
    /// The caller must hold the lease (`Some`); reconcile passes `None`.
    pub fn wait(
        &self,
        id: &str,
        lease: Option<&Lease>,
        note: &str,
        now: &Timestamp,
    ) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "挂起等待")?;
        self.transition(id, TaskStatus::Waiting, lease, None, note, now)
    }

    /// running → blocked: waiting on something external. Records the
    /// structured reason (`blocked_kind`) as well as the note, so the
    /// task list can show *why* it waits without parsing free text.
    /// Reconcile never moves it back.
    pub fn block(
        &self,
        id: &str,
        lease: Option<&Lease>,
        kind: BlockedKind,
        reason: &str,
        now: &Timestamp,
    ) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "阻塞")?;
        self.transition(id, TaskStatus::Blocked, lease, Some(kind), reason, now)
    }

    /// blocked → running: the thing it waited on has arrived. Issues a
    /// fresh lease (the old one died with the block) and clears
    /// `blocked_kind` in the same UPDATE.
    pub fn unblock(&self, id: &str, owner: &str, ttl_secs: u64, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Blocked], "解除阻塞")?;
        self.claim(
            id,
            TaskStatus::Blocked,
            owner,
            ttl_secs,
            false,
            Some(BlockedKind::None),
            now,
        )
    }

    /// waiting → running. Issues a fresh lease and bumps `retries`: this
    /// is a restart after waiting, which is what the retry budget counts.
    pub fn retry(&self, id: &str, owner: &str, ttl_secs: u64, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Waiting], "重试")?;
        self.claim(id, TaskStatus::Waiting, owner, ttl_secs, true, None, now)
    }

    /// running → done. The caller must hold the lease (`Some`); a stale
    /// owner gets `Err` and writes nothing.
    pub fn finish(&self, id: &str, lease: Option<&Lease>, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(&task, &[TaskStatus::Running], "完成")?;
        self.transition(id, TaskStatus::Done, lease, None, "", now)
    }

    /// queued / running / waiting / blocked → failed. Terminal, with the
    /// reason recorded and `finished_at` set. A running task needs the
    /// lease (`Some`); reconcile passes `None`.
    pub fn fail(
        &self,
        id: &str,
        lease: Option<&Lease>,
        error: &str,
        now: &Timestamp,
    ) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(
            &task,
            &[
                TaskStatus::Queued,
                TaskStatus::Running,
                TaskStatus::Waiting,
                TaskStatus::Blocked,
            ],
            "标记失败",
        )?;
        let done = self.transition(id, TaskStatus::Failed, lease, None, error, now)?;
        let _ = self
            .db
            .audit("task", "task_failed", &format!("{}: {error}", task.id));
        Ok(done)
    }

    /// queued / running / waiting / blocked → cancelled (user cancel,
    /// §7.2). Terminal. A control-plane write: it needs no lease —
    /// cancelling is the user's decision, not the worker's — and a
    /// worker that notices it stops claiming new steps and lets
    /// in-flight calls reconcile against the ledger. The worker
    /// propagation (stop claiming, keep reconciling) is M1 work.
    pub fn cancel(&self, id: &str, note: &str, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        self.must_be(
            &task,
            &[
                TaskStatus::Queued,
                TaskStatus::Running,
                TaskStatus::Waiting,
                TaskStatus::Blocked,
            ],
            "取消",
        )?;
        self.transition(id, TaskStatus::Cancelled, None, None, note, now)
    }

    /// Refresh the heartbeat of a running task: proof its owner is alive,
    /// and extend the lease expiry by the stored TTL. Only the lease
    /// holder may heartbeat — a stale owner gets `Err`. Expiry alone
    /// does not invalidate the owner (only `reclaim` can change hands),
    /// so a heartbeat that arrives just after the expiry still counts.
    /// Only `running` tasks heartbeat; a queued task never started and a
    /// waiting / blocked one is paused by design. D7 owns live heartbeat
    /// updates and liveness probing; D6 only stores the field.
    pub fn heartbeat(&self, id: &str, lease: &Lease, now: &Timestamp) -> Result<Task> {
        let task = self.load(id)?;
        if task.status != TaskStatus::Running {
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，不能心跳（只有执行中的任务心跳）",
                task.id,
                task.status.as_str()
            )));
        }
        let expires = to_rfc3339(&(*now + Duration::seconds(task.lease_ttl_secs as i64)));
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE tasks SET last_heartbeat = ?2, lease_expires_at = ?3, updated_at = ?2 WHERE id = ?1 AND status = 'running' AND lease_owner = ?4 AND lease_generation = ?5",
                params![id, to_rfc3339(now), expires, lease.owner, lease.generation as i64],
            )
            .db()?;
        if changed == 0 {
            return Err(Error::Invalid(format!(
                "task {id} 的租约已失效或已被他人接管，不能心跳",
            )));
        }
        self.load(id)
    }

    /// Take over a `running` task whose lease expired: issues a fresh
    /// lease (generation + 1) to `owner`. The expiry is compared as an
    /// RFC3339 string — `to_rfc3339` is fixed-width (`...T...Z`), so
    /// lexicographic order is chronological order. Fails while the lease
    /// is still valid, so a live owner can never be stolen from.
    pub fn reclaim(&self, id: &str, owner: &str, ttl_secs: u64, now: &Timestamp) -> Result<Task> {
        let ts = to_rfc3339(now);
        let expires = to_rfc3339(&(*now + Duration::seconds(ttl_secs as i64)));
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE tasks SET lease_owner = ?2, lease_generation = lease_generation + 1, lease_ttl_secs = ?3, lease_expires_at = ?4, last_heartbeat = ?5, updated_at = ?5 WHERE id = ?1 AND status = 'running' AND lease_expires_at <> '' AND lease_expires_at < ?5",
                params![id, owner, ttl_secs as i64, expires, ts],
            )
            .db()?;
        if changed == 0 {
            let task = self.load(id)?;
            return Err(Error::Invalid(format!(
                "task {} 处于 {}，租约未过期，不能接管",
                task.id,
                task.status.as_str(),
            )));
        }
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
                if self.wait(&task.id, None, &note, now).is_ok() {
                    report.revived += 1;
                    let _ = self.db.audit(
                        "task",
                        "task_reconciled",
                        &format!("{}: 运行中记录已放回等待", task.id),
                    );
                }
            } else {
                let note = format!(
                    "心跳过期（上次心跳 {}）且重试次数已用完（{}/{}）",
                    task.last_heartbeat, task.retries, task.max_retries
                );
                if self.fail(&task.id, None, &note, now).is_ok() {
                    report.failed += 1;
                    let _ = self.db.audit(
                        "task",
                        "task_reconciled",
                        &format!("{}: 运行中记录已标记失败", task.id),
                    );
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
        let task = store
            .create("worker", "发日报", "daemon", "", 3, None, &t)
            .unwrap();
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
        assert_eq!(task.lease_expires_at, "");
        assert_eq!(task.blocked_kind, BlockedKind::None);
        assert_eq!(task.lease(), None);
    }

    /// M0: eight workers race to claim one queued task — exactly one
    /// wins. This is the test the old read-then-update `start` failed:
    /// two threads could both read `queued` before either wrote.
    #[test]
    fn concurrent_claim_exactly_one_winner() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let t = now();
        let task = store
            .create("worker", "竞态", "daemon", "", 3, None, &t)
            .unwrap();
        let id = task.id.clone();
        // Pre-bind the shared reference: a `move` closure that writes
        // `&db` would otherwise try to move the owned `Db` (and `store`
        // already borrows it).
        let db_ref = &db;
        let results: Vec<_> = std::thread::scope(|s| {
            (0..8)
                .map(|i| {
                    let id = id.clone();
                    s.spawn(move || {
                        let t = now();
                        TaskStore::new(db_ref).start(&id, &format!("worker-{i}"), 60, &t)
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|r| r.is_err()).count(), 7);
        let winner = store.get(&id).unwrap().unwrap();
        assert_eq!(winner.status, TaskStatus::Running);
        assert_eq!(winner.lease_generation, 1);
        assert!(!winner.lease_owner.is_empty());
    }

    /// M0: a stale lease writes nothing. A claims (TTL 1s); B cannot
    /// claim or reclaim while the lease is valid; after expiry B
    /// reclaims (generation 2) and A's heartbeat + finish are rejected,
    /// while B finishes cleanly.
    #[test]
    fn stale_lease_writes_rejected_and_reclaim_works() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let t0 = now();
        let task = store
            .create("worker", "租约", "daemon", "", 3, None, &t0)
            .unwrap();
        let a = store.start(&task.id, "worker-a", 1, &t0).unwrap();
        let lease_a = a.lease().unwrap();
        assert_eq!(lease_a.generation, 1);
        assert!(store.start(&task.id, "worker-b", 60, &t0).is_err());
        assert!(store.reclaim(&task.id, "worker-b", 60, &t0).is_err());
        // A's heartbeat still works (only the generation guards it) and
        // extends the expiry by the stored TTL.
        let t1 = t0 + Duration::seconds(1);
        let a2 = store.heartbeat(&task.id, &lease_a, &t1).unwrap();
        assert_eq!(a2.lease_generation, 1);
        // After the extended expiry, B reclaims; A's lease is stale.
        let t2 = t1 + Duration::seconds(2);
        let b = store.reclaim(&task.id, "worker-b", 60, &t2).unwrap();
        let lease_b = b.lease().unwrap();
        assert_eq!(lease_b.owner, "worker-b");
        assert_eq!(lease_b.generation, 2);
        assert!(store.heartbeat(&task.id, &lease_a, &t2).is_err());
        assert!(store.finish(&task.id, Some(&lease_a), &t2).is_err());
        assert!(store
            .fail(&task.id, Some(&lease_a), "旧主失败", &t2)
            .is_err());
        let done = store.finish(&task.id, Some(&lease_b), &t2).unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.finished_at.is_some());
    }

    /// M0: `block` records the structured reason and `unblock` clears
    /// it in the same UPDATE that re-issues the lease.
    #[test]
    fn block_records_kind_and_unblock_clears_it() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let t = now();
        let task = store
            .create("worker", "阻塞", "daemon", "", 3, None, &t)
            .unwrap();
        let running = store.start(&task.id, "worker-a", 60, &t).unwrap();
        let lease = running.lease().unwrap();
        let blocked = store
            .block(&task.id, Some(&lease), BlockedKind::Approval, "等审批", &t)
            .unwrap();
        assert_eq!(blocked.status, TaskStatus::Blocked);
        assert_eq!(blocked.blocked_kind, BlockedKind::Approval);
        let back = store.unblock(&task.id, "worker-a", 60, &t).unwrap();
        assert_eq!(back.status, TaskStatus::Running);
        assert_eq!(back.blocked_kind, BlockedKind::None);
        assert_eq!(back.lease_generation, 2);
    }

    /// M0: cancel is terminal, needs no lease (control plane), and
    /// nothing moves out of it afterwards.
    #[test]
    fn cancel_is_terminal_and_needs_no_lease() {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let t = now();
        let task = store
            .create("worker", "取消", "daemon", "", 3, None, &t)
            .unwrap();
        let cancelled = store.cancel(&task.id, "用户取消", &t).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert!(cancelled.status.is_terminal());
        assert!(cancelled.finished_at.is_some());
        assert!(store.start(&task.id, "worker-a", 60, &t).is_err());
        assert!(store.cancel(&task.id, "再取消", &t).is_err());
        // A cancelled task is not open (reconcile never sees it).
        assert!(store.open().unwrap().iter().all(|t| t.id != task.id));
    }
}
