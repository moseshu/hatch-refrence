//! Registry of sub-agents running in this process.
//!
//! Async delegation starts a child and returns immediately; the child
//! keeps running after its turn ends. This registry is what remembers it:
//! run id → the per-run cancel token (cancelling the token kills the
//! child within ~200 ms, through the monitor loop's cancel check) and
//! the owning parent, for the concurrency limits. Status lives in the
//! [`RunStore`]; this map only tracks liveness. It is process-scoped —
//! a fresh process starts empty, and stale records are reconciled from
//! the run table instead (see [`reconcile_runs`]).
//!
//! The registry is owned by the [`App`](crate::app::App)-equivalent of the
//! host process (CLI chat, daemon, transport runner) and handed to each
//! turn through [`Delegation`](super::Delegation). Eval and unit tests
//! build a `Delegation` without one and stay on the synchronous path.

use super::store::{RunStatus, RunStore};
use hatch_core::time::Timestamp;
use hatch_core::CancelToken;
use hatch_store::Db;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One running child: how to stop it, and who it belongs to.
#[derive(Clone, Debug)]
pub struct RegistryEntry {
    /// Cancelling this token kills the child (or stops its loop).
    pub cancel: CancelToken,
    /// Top-level session the run belongs to.
    pub session: String,
    /// `session:<id>` or `subagent:<id>` of the delegating agent.
    pub parent_ref: String,
}

/// Live children of this process, by run id.
pub struct ChildRegistry {
    db: Arc<Db>,
    running: Mutex<HashMap<String, RegistryEntry>>,
}

impl std::fmt::Debug for ChildRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.running.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("ChildRegistry").field("running", &n).finish_non_exhaustive()
    }
}

/// What a pause/resume attempt did, so the tool handler, the CLI and
/// the API can all answer the same way. `Done` and `Already` are both
/// successes — a repeated pause/resume is an idempotent no-op, not an
/// error — while `Terminal` and `NotOwned` are refusals with the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    Done,
    Already,
    NotOwned,
    Terminal(RunStatus),
}

impl ChildRegistry {
    pub fn new(db: Arc<Db>) -> Self {
        ChildRegistry { db, running: Mutex::new(HashMap::new()) }
    }

    /// The shared database, for monitor threads that outlive the turn.
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// Whether this process currently owns a live child with this id.
    pub fn contains(&self, run_id: &str) -> bool {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).contains_key(run_id)
    }

    /// Whether a run is currently paused (D5): its record says so. The
    /// store is the source of truth — the monitor loop reads this, so a
    /// pause issued from any turn in this process freezes the run's
    /// timeout clock at once. A run this process does not own is never
    /// paused here.
    pub fn is_paused(&self, run_id: &str) -> bool {
        if !self.contains(run_id) {
            return false;
        }
        RunStore::new(&self.db).get(run_id).map(|r| r.is_some_and(|r| r.status == RunStatus::Paused)).unwrap_or(false)
    }

    /// Pause a live child by run id (D5): freeze its process in place
    /// (`SIGSTOP`) and mark the run paused. Nothing is deleted — the
    /// scratch dir, the child's database and the monitor thread all
    /// stay — so a resume continues the same run from exactly where it
    /// stopped. The run's timeout clock does not run while paused (the
    /// monitor loop pushes its deadline forward); a paused run still
    /// counts toward `max_parallel`, because it still holds a process,
    /// a socket and a slot, and resuming must not over-commit the limit.
    /// Idempotent: pausing an already-paused run succeeds with
    /// `Already`. Refuses terminal runs and runs this process does not
    /// own (those may belong to the daemon — pause them from there).
    pub fn pause(&self, run_id: &str) -> ControlOutcome {
        let Some(pid) = self.child_pid(run_id) else {
            return ControlOutcome::NotOwned;
        };
        let store = RunStore::new(&self.db);
        let Ok(Some(rec)) = store.get(run_id) else {
            return ControlOutcome::NotOwned;
        };
        if rec.status.is_terminal() {
            return ControlOutcome::Terminal(rec.status);
        }
        if rec.status == RunStatus::Paused {
            return ControlOutcome::Already;
        }
        if rec.child_pid == 0 {
            return ControlOutcome::NotOwned;
        }
        if !hatch_scheduler::lock::stop_process(pid) {
            // The child died between the lookup and the signal: leave
            // the record alone; the monitor thread will finish it.
            return ControlOutcome::NotOwned;
        }
        match store.pause(run_id) {
            Ok(true) => {
                let _ = self.db.audit("subagent", "paused", &format!("{run_id}: 已暂停（子进程冻结，状态保留）"));
                ControlOutcome::Done
            }
            _ => {
                // The run finished concurrently; unfreeze so the monitor
                // thread can reap it instead of leaving it stopped.
                let _ = hatch_scheduler::lock::continue_process(pid);
                match store.get(run_id) {
                    Ok(Some(r)) if r.status.is_terminal() => ControlOutcome::Terminal(r.status),
                    Ok(Some(r)) if r.status == RunStatus::Paused => ControlOutcome::Already,
                    _ => ControlOutcome::NotOwned,
                }
            }
        }
    }

    /// Resume a paused child by run id (D5): continue its process
    /// (`SIGCONT`) and mark the run running again. The same run id, the
    /// same process, the same turn loop — the work continues from where
    /// the pause froze it, it does not start over. Idempotent:
    /// resuming a running run succeeds with `Already`.
    pub fn resume(&self, run_id: &str) -> ControlOutcome {
        let Some(pid) = self.child_pid(run_id) else {
            return ControlOutcome::NotOwned;
        };
        let store = RunStore::new(&self.db);
        let Ok(Some(rec)) = store.get(run_id) else {
            return ControlOutcome::NotOwned;
        };
        if rec.status.is_terminal() {
            return ControlOutcome::Terminal(rec.status);
        }
        if rec.status == RunStatus::Running {
            return ControlOutcome::Already;
        }
        if rec.status != RunStatus::Paused || rec.child_pid == 0 {
            return ControlOutcome::NotOwned;
        }
        if !hatch_scheduler::lock::continue_process(pid) {
            return ControlOutcome::NotOwned;
        }
        match store.resume(run_id) {
            Ok(true) => {
                let _ = self.db.audit("subagent", "resumed", &format!("{run_id}: 已恢复（同一 run 继续执行）"));
                ControlOutcome::Done
            }
            _ => {
                let _ = hatch_scheduler::lock::stop_process(pid);
                match store.get(run_id) {
                    Ok(Some(r)) if r.status.is_terminal() => ControlOutcome::Terminal(r.status),
                    Ok(Some(r)) if r.status == RunStatus::Running => ControlOutcome::Already,
                    _ => ControlOutcome::NotOwned,
                }
            }
        }
    }

    /// The child pid of a run this process owns, if any.
    fn child_pid(&self, run_id: &str) -> Option<i32> {
        if !self.contains(run_id) {
            return None;
        }
        RunStore::new(&self.db).get(run_id).ok().flatten().map(|r| r.child_pid as i32)
    }

    /// Remember a freshly started child. Returns false if the run id is
    /// already registered (a duplicate start; the caller should not
    /// start it twice).
    pub fn register(&self, run_id: &str, entry: RegistryEntry) -> bool {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).insert(run_id.to_string(), entry).is_none()
    }

    /// Forget a child (it reached a terminal state). Idempotent.
    pub fn unregister(&self, run_id: &str) {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).remove(run_id);
    }

    /// How many children of one parent are currently running (the
    /// `max_parallel` enforcement point).
    pub fn running_for(&self, parent_ref: &str) -> usize {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|e| e.parent_ref == parent_ref)
            .count()
    }

    /// Cancel a live child by run id: its monitor loop kills the child
    /// process (or stops the in-process loop) and the run finishes as
    /// cancelled. Returns false when this process owns no such child.
    pub fn cancel(&self, run_id: &str) -> bool {
        let Some(entry) = self.running.lock().unwrap_or_else(|p| p.into_inner()).get(run_id).cloned() else {
            return false;
        };
        entry.cancel.cancel("用户取消子代理");
        true
    }
}

/// Reconcile runs a previous process left open.
///
/// For every run of `parent_ref` still marked spawned/running:
/// * owned by this process (in the registry) → still running, leave it;
/// * a child process that is still alive (`child_pid` answers to a
///   signal-0 check — it may belong to another live process, e.g. the
///   daemon while this is a chat turn) → leave it;
/// * otherwise the owner is gone and the child is dead (its UDS
///   watchdog exits it when the parent dies) → mark it interrupted and
///   store an undelivered handoff, so the next turn learns the run died
///   instead of waiting on it forever.
///
/// Paused runs (D5) are handled separately, in the same call, because
/// a frozen child breaks the "alive ⇒ owned" shortcut above: a paused
/// child is alive but stopped, and its UDS watchdog thread is frozen
/// with it, so it cannot notice that its parent died. A paused run
/// whose owner is gone can never be resumed (resuming needs the
/// owner's monitor thread and child handle), so it is interrupted
/// instead — but only once the owner is proven gone: the run is owned
/// by this process (in the registry), or its child is alive and its
/// owner process is alive ([`owner_alive`](hatch_scheduler::lock::owner_alive)),
/// means leave it alone; otherwise the stopped child is killed, its
/// scratch dir removed, and the run is marked interrupted with a
/// handoff saying it died while paused.
///
/// Best-effort: every failure just leaves the record as it is. Returns
/// how many runs were marked interrupted.
pub fn reconcile_runs(db: &Db, registry: Option<&ChildRegistry>, parent_ref: &str, now: &Timestamp) -> usize {
    let store = RunStore::new(db);
    let Ok(runs) = store.running_for_parent(parent_ref) else {
        return 0;
    };
    let mut n = 0;
    for r in runs {
        if let Some(reg) = registry {
            if reg.contains(&r.id) {
                continue;
            }
        }
        if r.child_pid > 0 && hatch_scheduler::lock::pid_alive(r.child_pid as i32) {
            continue;
        }
        let _ = store.interrupt(&r.id, "父进程重启，子代理中断，未给出结论。", now);
        let text = format!(
            "[子代理 {} · 类型 {} · 状态 interrupted]\n没有完成：父进程重启，子代理中断，未给出结论。",
            r.id, r.agent_type
        );
        let _ = store.save_handoff(&r.id, &text, false, now);
        let _ = db.audit("subagent", "reconciled", &format!("{}: 运行中记录已中断（子进程已死）", r.id));
        n += 1;
    }
    // Paused runs: same shape, different liveness test (see above).
    // A paused run is never a candidate for the task-ledger watchdog
    // (D7 reconciles the ledger, and sub-agent runs are not ledger
    // rows), so this is the only convergence path that can see one —
    // and it must not mistake a live pause for a dead run.
    let Ok(paused) = store.paused_for_parent(parent_ref) else {
        return n;
    };
    for r in paused {
        if let Some(reg) = registry {
            if reg.contains(&r.id) {
                continue;
            }
        }
        if r.child_pid > 0
            && hatch_scheduler::lock::pid_alive(r.child_pid as i32)
            && hatch_scheduler::lock::owner_alive(r.child_pid as i32)
        {
            continue;
        }
        // The owner is gone (or the child is already dead): a stopped
        // orphan cannot resume, so kill what is left of it and close
        // the run. Killing a stopped process works; removing the
        // scratch dir is best-effort (the dead owner cannot do it).
        if r.child_pid > 0 {
            let _ = hatch_scheduler::lock::kill_process(r.child_pid as i32);
        }
        let _ = std::fs::remove_dir_all(super::spawn::scratch_dir(&r.id));
        let _ = store.interrupt(&r.id, "父进程退出时子代理处于暂停状态，无法恢复，已中断。", now);
        let text = format!(
            "[子代理 {} · 类型 {} · 状态 interrupted]\n没有完成：父进程退出时子代理处于暂停状态，无法恢复，已中断。",
            r.id, r.agent_type
        );
        let _ = store.save_handoff(&r.id, &text, false, now);
        let _ = db.audit("subagent", "reconciled", &format!("{}: 暂停中记录已中断（父进程已退出）", r.id));
        n += 1;
    }
    n
}

/// Collect this agent's undelivered handoffs (and mark them delivered).
/// A DB error means "no handoffs", never a failed turn.
pub fn collect_handoffs(db: &Db, parent_ref: &str) -> Vec<super::store::PendingHandoff> {
    RunStore::new(db).pending_handoffs(parent_ref).unwrap_or_default()
}

/// Parent reference of the agent running a turn: `session:<id>` for the
/// main agent, `subagent:<run id>` for a sub-agent.
pub fn parent_ref(depth: usize, agent_id: &str, session: &str) -> String {
    if depth == 0 {
        format!("session:{session}")
    } else {
        format!("subagent:{agent_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::RunStatus;
    use super::*;

    fn registry() -> ChildRegistry {
        ChildRegistry::new(Arc::new(Db::open_in_memory().unwrap()))
    }

    fn entry(session: &str, parent_ref: &str) -> RegistryEntry {
        RegistryEntry { cancel: CancelToken::new(), session: session.to_string(), parent_ref: parent_ref.to_string() }
    }

    #[test]
    fn register_cancel_and_counts() {
        let reg = registry();
        assert!(reg.register("sa_1", entry("s", "session:s")));
        assert!(!reg.register("sa_1", entry("s", "session:s")), "duplicate start is refused");
        assert!(reg.contains("sa_1"));
        reg.register("sa_2", entry("s", "session:s"));
        reg.register("sa_3", entry("t", "session:t"));
        assert_eq!(reg.running_for("session:s"), 2);
        assert_eq!(reg.running_for("session:t"), 1);
        assert!(reg.cancel("sa_1"));
        assert!(!reg.cancel("sa_nope"));
        let token = reg.running.lock().unwrap().get("sa_1").unwrap().cancel.clone();
        assert!(token.is_cancelled());
        reg.unregister("sa_1");
        assert!(!reg.contains("sa_1"));
        assert_eq!(reg.running_for("session:s"), 1);
    }

    #[test]
    fn reconcile_marks_only_dead_runs_interrupted() {
        let db = Db::open_in_memory().unwrap();
        let store = RunStore::new(&db);
        let now = hatch_core::time::now();
        // A stale in-process run (no child pid): its owner is gone.
        store.create("sa_dead", "session:s", "s", 1, "explore", "旧任务", RunStatus::Running, &now).unwrap();
        // A run whose child is this very test process: still alive.
        store.create("sa_live", "session:s", "s", 1, "explore", "活任务", RunStatus::Running, &now).unwrap();
        store.set_child_pid("sa_live", std::process::id()).unwrap();
        // A finished run is never touched.
        store.create("sa_done", "session:s", "s", 1, "explore", "已完成", RunStatus::Completed, &now).unwrap();
        let reg = registry();
        let n = reconcile_runs(&db, Some(&reg), "session:s", &now);
        assert_eq!(n, 1);
        assert_eq!(store.get("sa_dead").unwrap().unwrap().status, RunStatus::Interrupted);
        assert_eq!(store.get("sa_live").unwrap().unwrap().status, RunStatus::Running);
        assert_eq!(store.get("sa_done").unwrap().unwrap().status, RunStatus::Completed);
        // The interrupted run's handoff is waiting for the next turn…
        let pending = store.pending_handoffs("session:s").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].run_id, "sa_dead");
        assert!(pending[0].handoff.contains("interrupted"));
        // …and delivered exactly once.
        assert!(store.pending_handoffs("session:s").unwrap().is_empty());
    }

}
