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

use super::store::RunStore;
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

impl ChildRegistry {
    pub fn new(db: Arc<Db>) -> Self {
        ChildRegistry { db, running: Mutex::new(HashMap::new()) }
    }

    /// The shared database, for monitor threads that outlive the turn.
    pub fn db(&self) -> &Arc<Db> {
        &self.db
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

    /// Whether this process currently owns a live child with this id.
    pub fn contains(&self, run_id: &str) -> bool {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).contains_key(run_id)
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
