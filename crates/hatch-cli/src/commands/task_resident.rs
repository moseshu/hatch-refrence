//! The resident task worker (P0 product closed loop): the
//! daemon-side loop that claims queued tasks and wakes blocked
//! ones, so a client can submit through the API, disconnect, and
//! read the result later — no `hatch task run` needed.
//!
//! One sweep at a time, oldest task first:
//!
//! * `Queued` → claim and run. A `delegate-join` parent waits until
//!   it has children and none are still out — with no children the
//!   join would finish immediately, which is never what "wait for
//!   the children" means.
//! * `Waiting` → claim and retry, but only while budget and retries
//!   remain: a budget-exhausted wait would only budget-stop again,
//!   and a retry-exhausted wait belongs to reconcile's failure path.
//! * `Blocked` → claim only when something arrived since the block:
//!   a decided approval (`Approved` to settle, `Executed` to finish
//!   the step, `Rejected` / `Failed` / `Expired` to fail it with the
//!   reason) or a done/failed child to consume. A `Paused` task is
//!   never claimed — only an explicit resume moves it to queued —
//!   and `NeedsReview` (P0 #1: outcome unknown) waits for a human,
//!   never for a retry.
//! * `Running` → owned by someone else; the watchdog converges it
//!   when its lease goes stale. Never reclaimed here.
//!
//! The loop sleeps on the app's [`TaskWaker`]: an HTTP approve /
//! resume wakes it immediately, and the sweep interval (config
//! `tasks.resident_sweep_secs`, default 2s) is the fallback for
//! wakes from other processes (CLI approve / consume). After a run
//! it re-scans immediately, so a child finishing wakes its parent
//! without waiting out the interval. A run that errors (a missing
//! world, a claim lost to a CLI run) backs off per task —
//! 2s, 4s, 8s … capped at 60s — instead of hammering the ledger
//! every sweep; a successful run clears the backoff.
//!
//! The worker runs the scripted model only (`WorkerOpts::router`
//! stays `None`): wiring the daemon's configured router in is P1
//! pilot work, and this task's verification is script-model only.
//! All durable state lives in the ledger — the loop itself holds
//! nothing but the backoff map, so a daemon restart is just a fresh
//! loop over the same rows.

use super::task_gmail::run_worker_gmail;
use super::task_run_params::RunParams;
use super::task_worker::{run_worker, WorkerOpts};
use crate::app::App;
use hatch_core::Result;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tasks::{
    BlockedKind, DelegationStatus, DelegationStore, Task, TaskApprovals, TaskStatus, TaskStore,
    UnitStore,
};
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The approvals whose arrival makes a blocked task runnable again:
/// one to settle (`Approved`), one whose execution the worker must
/// still record (`Executed`), and the terminal decisions that fail
/// the waiting step with a reason (`Rejected`, `Failed`, `Expired`).
/// `Invalidated` is deliberately absent — the plan moved on, and
/// the gated step needs an edit before it can run again — and so is
/// `NeedsReview` (P0 #1: the outcome is unknown, only a human can
/// reconcile it).
fn approval_is_decided(status: ApprovalStatus) -> bool {
    matches!(
        status,
        ApprovalStatus::Approved
            | ApprovalStatus::Executed
            | ApprovalStatus::Rejected
            | ApprovalStatus::Failed
            | ApprovalStatus::Expired
    )
}

/// Whether a task is ready for the resident worker to claim. A
/// read-only probe — the claim itself happens inside
/// `run_worker_core`, atomically, so a CLI run racing the resident
/// worker cannot both win.
fn is_ready(app: &App, task: &Task) -> Result<bool> {
    let db = &app.db;
    match task.status {
        TaskStatus::Queued => {
            if task.kind == "delegate-join" {
                let ds = DelegationStore::new(db).list_for_parent(&task.id)?;
                if ds.is_empty() {
                    return Ok(false);
                }
                // Ready only once nothing is still out: with a child
                // still running, the join would only flip to blocked.
                Ok(ds.iter().all(|d| {
                    !matches!(
                        d.status,
                        DelegationStatus::Delegated | DelegationStatus::Running
                    )
                }))
            } else {
                Ok(true)
            }
        }
        TaskStatus::Waiting => {
            if task.retries >= task.max_retries {
                return Ok(false);
            }
            if task.max_model_calls == 0 {
                return Ok(true);
            }
            let used = UnitStore::new(db).total_model_calls(&task.id)?;
            Ok(used < task.max_model_calls)
        }
        TaskStatus::Blocked => {
            // A decided approval linked to the task. A dangling
            // link (the approval row is gone) is skipped, not an
            // error — the probe must not fail the whole sweep.
            for link in TaskApprovals::new(db).for_task(&task.id)? {
                let Ok(approval) = ApprovalQueue::new(db).get(&link.approval_id) else {
                    continue;
                };
                if approval_is_decided(approval.status) {
                    return Ok(true);
                }
            }
            // A done or failed child to consume (delegate-join).
            if matches!(
                task.blocked_kind,
                BlockedKind::Children | BlockedKind::Verification
            ) {
                let ds = DelegationStore::new(db).list_for_parent(&task.id)?;
                return Ok(ds.iter().any(|d| {
                    matches!(d.status, DelegationStatus::Done | DelegationStatus::Failed)
                }));
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Run one ready task, from its persisted parameters. Returns the
/// report's task id on a run (the task may have finished, blocked,
/// or waited — all are progress, not errors); `Err` only when the
/// run itself could not start (a missing world, a lost claim race).
fn run_ready_task(app: &Arc<App>, task: &Task, params: &RunParams) -> Result<String> {
    let out_dir = params.out_dir_for(&app.home, &task.id);
    let _ = std::fs::create_dir_all(&out_dir);
    let opts = WorkerOpts {
        task_id: Some(task.id.clone()),
        kind: None,
        lease_ttl_secs: params.lease_ttl_secs,
        now_override: None,
        kill: Default::default(),
        router: None,
        app: Some(Arc::clone(app)),
        max_model_calls: if params.max_model_calls > 0 {
            Some(params.max_model_calls)
        } else {
            None
        },
    };
    let report = if params.source == "gmail" {
        let run = params.to_gmail_run()?;
        run_worker_gmail(&app.db, &run, &out_dir, opts)?
    } else {
        let world = params.world.as_ref().ok_or_else(|| {
            hatch_core::Error::Invalid(format!("task {} 的执行参数缺少 world", task.id))
        })?;
        run_worker(&app.db, world, &out_dir, opts)?
    };
    Ok(report.task_id)
}

/// The resident worker: one sweep loop over the ledger. Holds no
/// durable state — only the per-task error backoff.
pub struct ResidentWorker {
    app: Arc<App>,
    sweep: Duration,
    backoff: HashMap<String, (u32, Instant)>,
}

impl ResidentWorker {
    pub fn new(app: &Arc<App>) -> Self {
        ResidentWorker {
            app: Arc::clone(app),
            sweep: Duration::from_secs(app.config.tasks.resident_sweep_secs.max(1)),
            backoff: HashMap::new(),
        }
    }

    /// One sweep: run the first ready task whose backoff expired.
    /// Returns true when a run was attempted (even when it errored —
    /// the task may need the backoff, not another immediate try).
    /// Tests drive the loop through this method; the daemon thread
    /// uses [`ResidentWorker::run_loop`].
    pub fn sweep_once(&mut self) -> bool {
        let tasks = match TaskStore::new(&self.app.db).open() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("常驻 worker 读任务列表失败：{e:#}");
                return false;
            }
        };
        for task in &tasks {
            if let Some((_, until)) = self.backoff.get(&task.id) {
                if Instant::now() < *until {
                    continue;
                }
            }
            // No persisted parameters (a pre-v20 CLI task, or a
            // delegation whose parent had none): the worker cannot
            // know the world or account, so it skips silently — the
            // task stays runnable via an explicit `task run`.
            let params = match RunParams::load(&self.app.db, &task.id) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("常驻 worker 读 task {} 的执行参数失败：{e:#}", task.id);
                    continue;
                }
            };
            let ready = match is_ready(&self.app, task) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("常驻 worker 检查 task {} 失败：{e:#}", task.id);
                    continue;
                }
            };
            if !ready {
                continue;
            }
            match run_ready_task(&self.app, task, &params) {
                Ok(_) => {
                    self.backoff.remove(&task.id);
                }
                Err(e) => {
                    // A claim lost to a CLI run is transient; a
                    // missing world is a configuration error. Both
                    // back off instead of hammering the ledger.
                    let failures = self.backoff.get(&task.id).map(|(n, _)| *n).unwrap_or(0) + 1;
                    let secs = 2u64.saturating_pow(failures.min(6)).min(60);
                    eprintln!(
                        "常驻 worker 运行 task {} 失败（第 {failures} 次，{secs}s 后重试）：{e:#}",
                        task.id
                    );
                    self.backoff.insert(
                        task.id.clone(),
                        (failures, Instant::now() + Duration::from_secs(secs)),
                    );
                }
            }
            return true;
        }
        false
    }

    /// The loop: sweep, and when nothing ran, sleep on the waker
    /// until a wake arrives or the sweep interval passes. A run
    /// re-scans immediately (a child finishing wakes its parent
    /// without waiting out the interval). Stops when the stop
    /// channel fires or its sender is dropped. The wait is sliced
    /// into 100ms pieces so a long sweep interval never delays
    /// shutdown (the task-5 lesson: a thread that cannot be joined
    /// hangs the daemon stop).
    pub fn run_loop(&mut self, stop_rx: &mpsc::Receiver<()>) {
        loop {
            if stop_requested(stop_rx) {
                break;
            }
            if self.sweep_once() {
                continue;
            }
            let deadline = Instant::now() + self.sweep;
            while Instant::now() < deadline {
                // `wait` returns true on a wake (and consumes the
                // flag): break out and sweep now instead of waiting
                // out the interval.
                if self.app.task_waker.wait(Duration::from_millis(100)) {
                    break;
                }
                if stop_requested(stop_rx) {
                    return;
                }
            }
        }
    }
}

fn stop_requested(stop_rx: &mpsc::Receiver<()>) -> bool {
    match stop_rx.try_recv() {
        Ok(()) => true,
        Err(mpsc::TryRecvError::Empty) => false,
        Err(mpsc::TryRecvError::Disconnected) => true,
    }
}

/// Spawn the resident worker thread (daemon `run`, not `--once`),
/// mirroring [`spawn_task_watchdog`]: the caller drops the sender
/// and joins the handle to stop it cleanly.
pub fn spawn_resident_worker(app: &Arc<App>) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let app = Arc::clone(app);
    let handle = std::thread::spawn(move || {
        let mut worker = ResidentWorker::new(&app);
        worker.run_loop(&stop_rx);
    });
    (stop_tx, handle)
}

/// The waker, re-exported for the API handlers (an approve / resume
/// wakes the worker immediately instead of waiting out the sweep).
pub fn wake(app: &App) {
    app.task_waker.wake();
}
