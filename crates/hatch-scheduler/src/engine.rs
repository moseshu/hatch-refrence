//! The run engine: decides which jobs fire on a tick and records the outcome.
//!
//! Semantics, per due job:
//!
//! * **Every run ends.** A watchdog cancels the run's token after the job's
//!   `timeout_secs`; handlers stop at their cancellation points.
//! * **Every run is on the ledger.** A run writes one `tasks` row (P1 D6,
//!   idempotency key = the run id) and heartbeats it while it works (P1
//!   D7), so the task watchdog can tell a live run from a dead one.
//!   That liveness watchdog is a different watchdog from the timeout
//!   one above: the timeout cancels a run that takes too long, the
//!   liveness one converges a run whose owner died.
//! * **Failure is not success.** A failed run leaves the firing pending and
//!   schedules a retry with exponential back-off (1, 2, 4 … 60 minutes).
//!   After `max_failures` consecutive failures the job is disabled and the
//!   user is told why.
//! * **No stampede after downtime.** Firings older than the grace window are
//!   recorded as *missed* (`job_runs.status = missed`, with the count). With
//!   `catch_up = once` the job then runs a single time; with `none` it waits
//!   for its next regular firing.
//! * **Cancelled is its own state.** A run stopped by the daemon shutting
//!   down is neither success nor failure; it is re-run on the next start.

use crate::job::{CatchUp, Job, RunStatus, Trigger};
use crate::store::JobStore;
use chrono::Duration;
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result};
use hatch_store::Db;
use serde::Serialize;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;

/// Reason given to a run's token when its time limit is reached.
pub const TIMEOUT_REASON: &str = "job timeout";
/// Default seconds between ledger heartbeats of a running job (P1 D7).
/// 60s: matches the daemon tick and the `[tasks] heartbeat_secs`
/// default — a live run can never look stale to the watchdog.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 60;
/// Upper bound on firings enumerated for one job in one tick.
const MAX_FIRINGS_PER_TICK: usize = 100_000;

/// What a handler reports for a run that ended normally.
#[derive(Clone, Debug, PartialEq)]
pub enum JobOutcome {
    Done { summary: String },
    /// Nothing to do (e.g. already produced today); counts as success.
    Skipped { reason: String },
}

/// Everything a handler may use during one run.
pub struct JobContext<'a> {
    pub job: &'a Job,
    pub db: &'a Db,
    pub now: Timestamp,
    pub tz: Tz,
    pub cancel: CancelToken,
    pub trigger: Trigger,
    pub run_id: String,
}

impl JobContext<'_> {
    pub fn dedup_seen(&self, key: &str) -> Result<bool> {
        JobStore::new(self.db).dedup_seen(&self.job.id, key)
    }

    /// Mark `key` as produced; `false` if it already was.
    pub fn dedup_mark(&self, key: &str) -> Result<bool> {
        JobStore::new(self.db).dedup_mark(&self.job.id, key, &self.now)
    }
}

/// Implemented by each job kind.
pub trait JobHandler: Send + Sync {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome>;
}

/// A handler from a closure (tests, evaluation, simple built-ins).
pub struct FnHandler<F>(pub F);

impl<F> JobHandler for FnHandler<F>
where
    F: Fn(&JobContext) -> Result<JobOutcome> + Send + Sync,
{
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        (self.0)(ctx)
    }
}

/// Called when the engine has something to tell the user (a job disabled).
pub type Notice = Arc<dyn Fn(&Job, &str) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    pub job_id: String,
    pub job_name: String,
    pub run_id: String,
    pub trigger: Trigger,
    pub status: RunStatus,
    pub summary: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TickReport {
    pub runs: Vec<RunResult>,
    /// `(job name, firings missed)`.
    pub missed: Vec<(String, u32)>,
    /// `(job name, reason)`.
    pub disabled: Vec<(String, String)>,
    /// The tick stopped before visiting every due job (cancelled).
    pub stopped_early: bool,
}

pub struct Engine<'a> {
    db: &'a Db,
    tz: Tz,
    handlers: BTreeMap<String, Arc<dyn JobHandler>>,
    /// Firings at most this late still count as on time.
    pub grace: Duration,
    /// Seconds between ledger heartbeats while a job runs (P1 D7).
    pub heartbeat_secs: std::time::Duration,
    notice: Option<Notice>,
}

fn retry_delay(failures: u32) -> Duration {
    Duration::minutes(1i64 << failures.saturating_sub(1).min(6)).min(Duration::minutes(60))
}

impl<'a> Engine<'a> {
    pub fn new(db: &'a Db, tz: Tz) -> Self {
        Engine {
            db,
            tz,
            handlers: BTreeMap::new(),
            grace: Duration::minutes(10),
            heartbeat_secs: std::time::Duration::from_secs(DEFAULT_HEARTBEAT_SECS),
            notice: None,
        }
    }

    /// How often a running job proves it is alive to the task ledger
    /// (P1 D7). 0 disables the heartbeat thread (tests).
    pub fn with_heartbeat_secs(mut self, secs: u64) -> Self {
        self.heartbeat_secs = std::time::Duration::from_secs(secs);
        self
    }

    pub fn register(mut self, kind: &str, handler: Arc<dyn JobHandler>) -> Self {
        self.handlers.insert(kind.to_string(), handler);
        self
    }

    pub fn with_notice(mut self, notice: Notice) -> Self {
        self.notice = Some(notice);
        self
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    pub fn kinds(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    pub fn tz(&self) -> Tz {
        self.tz
    }

    /// Handle every due job once.
    pub fn tick(&self, now: &Timestamp, cancel: &CancelToken) -> Result<TickReport> {
        let store = JobStore::new(self.db);
        let mut report = TickReport::default();
        for job in store.due(now)? {
            if cancel.is_cancelled() {
                report.stopped_early = true;
                break;
            }
            self.handle_due(&store, job, now, cancel, &mut report)?;
        }
        Ok(report)
    }

    fn handle_due(&self, store: &JobStore, job: Job, now: &Timestamp, cancel: &CancelToken, report: &mut TickReport) -> Result<()> {
        let Some(first) = job.next_run_at else { return Ok(()) };
        let (firings, capped) = job.schedule.firings_between(&first, now, &job.anchor_at, &self.tz, MAX_FIRINGS_PER_TICK);
        let last = *firings.last().unwrap_or(&first);
        let pending_retry = job.consecutive_failures > 0 || job.last_status == RunStatus::Cancelled.as_str();
        let (trigger, missed) = if pending_retry {
            (Some(Trigger::Retry), firings.len().saturating_sub(1))
        } else if *now - last <= self.grace {
            (Some(Trigger::Scheduled), firings.len() - 1)
        } else {
            let t = (job.catch_up == CatchUp::Once).then_some(Trigger::CatchUp);
            (t, firings.len())
        };
        if missed > 0 {
            let count = missed as u32;
            let missed_last = if trigger.is_some() && !pending_retry && trigger != Some(Trigger::CatchUp) {
                firings[firings.len() - 2]
            } else {
                last
            };
            store.record_missed(&job.id, count, &first, &missed_last, now)?;
            let note = if capped { format!("≥{count}") } else { count.to_string() };
            self.db.audit("scheduler", "job_missed", &format!("{} missed {note} firing(s)", job.name))?;
            report.missed.push((job.name.clone(), count));
        }
        let next = job.schedule.next_after(now, &job.anchor_at, &self.tz);
        let Some(trigger) = trigger else {
            store.set_state(&job.id, next, None, 0, None, RunStatus::Missed.as_str())?;
            return Ok(());
        };
        let result = self.execute(&job, trigger, Some(last), now, cancel)?;
        match result.status {
            RunStatus::Ok | RunStatus::Skipped => {
                store.set_state(&job.id, next, None, 0, Some(*now), result.status.as_str())?;
            }
            RunStatus::Cancelled => {
                // Keep the firing pending; it is re-run on the next start.
                store.set_state(&job.id, job.next_run_at, None, job.consecutive_failures, Some(*now), result.status.as_str())?;
            }
            _ => {
                let failures = job.consecutive_failures + 1;
                store.set_state(&job.id, job.next_run_at, Some(*now + retry_delay(failures)), failures, Some(*now), "failed")?;
                if failures >= job.max_failures {
                    let reason = format!("连续失败 {failures} 次，已自动禁用。最后一次错误：{}", result.error);
                    store.set_enabled(&job.id, false, &reason, now, &self.tz)?;
                    if let Some(n) = &self.notice {
                        n(&job, &format!("定时任务「{}」{reason}。修复后用 `hatch jobs enable {}` 重新启用。", job.name, job.name));
                    }
                    report.disabled.push((job.name.clone(), reason));
                }
            }
        }
        report.runs.push(result);
        Ok(())
    }

    /// Run a job right away (`hatch jobs run-now`). The schedule is untouched
    /// and a failure does not count towards auto-disable.
    pub fn run_now(&self, id_or_name: &str, now: &Timestamp, cancel: &CancelToken) -> Result<RunResult> {
        let store = JobStore::new(self.db);
        let job = store.get(id_or_name)?;
        let r = self.execute(&job, Trigger::Manual, None, now, cancel)?;
        store.set_state(&job.id, job.next_run_at, job.retry_at, job.consecutive_failures, Some(*now), r.status.as_str())?;
        Ok(r)
    }

    fn execute(&self, job: &Job, trigger: Trigger, scheduled_for: Option<Timestamp>, now: &Timestamp, cancel: &CancelToken) -> Result<RunResult> {
        let store = JobStore::new(self.db);
        let run_id = store.start_run(&job.id, trigger.as_str(), scheduled_for, now)?;
        // P1 D7: the ledger row for this run (kind `scheduler`, owner
        // `daemon`, idempotency key = the scheduler run id, so a
        // re-execution of the same run never creates a second row).
        // Best-effort throughout: a ledger failure must never fail the
        // job itself, so every call below swallows its error and a
        // missing row just means no heartbeat / no finish bookkeeping.
        let ledger = hatch_tasks::TaskStore::new(self.db)
            .create("scheduler", &job.name, "daemon", "", 3, Some(&run_id), now)
            .and_then(|t| {
                hatch_tasks::TaskStore::new(self.db)
                    .start(&t.id, "daemon", hatch_tasks::DEFAULT_LEASE_TTL_SECS, now)
            })
            .ok()
            .map(|t| {
                let lease = t.lease().expect("start 总是签发一个租约");
                (t.id, lease)
            });
        let token = cancel.child();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = {
            let token = token.clone();
            let limit = std::time::Duration::from_secs(job.timeout_secs);
            std::thread::spawn(move || {
                if let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(limit) {
                    token.cancel(TIMEOUT_REASON);
                }
            })
        };
        // The heartbeat thread proves the run is alive to the D7
        // watchdog while the handler works (scoped, so it borrows the
        // Db and is joined before the ledger row is closed). It is a
        // different watchdog from the timeout thread above: that one
        // cancels a run that takes too long; this one proves a run
        // that is still working is not dead.
        let (hb_tx, hb_rx) = mpsc::channel::<()>();
        let result = std::thread::scope(|s| {
            if let Some((task_id, lease)) = &ledger {
                if !self.heartbeat_secs.is_zero() {
                    let db = self.db;
                    let interval = self.heartbeat_secs;
                    let task_id = task_id.clone();
                    let lease = lease.clone();
                    s.spawn(move || loop {
                        match hb_rx.recv_timeout(interval) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                let _ = hatch_tasks::TaskStore::new(db)
                                    .heartbeat(&task_id, &lease, &hatch_core::time::now());
                            }
                        }
                    });
                }
            }
            let result = match self.handlers.get(&job.kind) {
                None => Err(Error::Invalid(format!("没有 {} 类任务的处理器（已知：{}）", job.kind, self.kinds().join(", ")))),
                Some(h) => {
                    let ctx = JobContext { job, db: self.db, now: *now, tz: self.tz, cancel: token.clone(), trigger, run_id: run_id.clone() };
                    std::panic::catch_unwind(AssertUnwindSafe(|| h.run(&ctx)))
                        .unwrap_or_else(|_| Err(Error::Invalid("处理器内部错误（panic）".into())))
                }
            };
            drop(hb_tx);
            result
        });
        drop(done_tx);
        let _ = watchdog.join();
        let timed_out = token.reason().as_deref() == Some(TIMEOUT_REASON);
        let (status, summary, error) = match result {
            Ok(JobOutcome::Done { summary }) => (RunStatus::Ok, summary, String::new()),
            Ok(JobOutcome::Skipped { reason }) => (RunStatus::Skipped, reason, String::new()),
            Err(_) if timed_out => (RunStatus::Failed, String::new(), format!("运行超过 {} 秒上限，已停止", job.timeout_secs)),
            Err(e) if token.is_cancelled() || matches!(e, Error::Cancelled(_)) => {
                (RunStatus::Cancelled, String::new(), token.reason().unwrap_or_else(|| e.to_string()))
            }
            Err(e) => (RunStatus::Failed, String::new(), e.to_string()),
        };
        store.finish_run(&run_id, status, &summary, &error, now)?;
        // Close the ledger row to match the run outcome: done / skipped
        // finish it, a failure fails it, a cancellation leaves it
        // waiting — the firing stays pending and is re-run on the next
        // start, which creates a fresh row for the fresh run.
        if let Some((task_id, lease)) = &ledger {
            let tasks = hatch_tasks::TaskStore::new(self.db);
            let _ = match status {
                RunStatus::Ok | RunStatus::Skipped => tasks.finish(task_id, Some(lease), now),
                RunStatus::Cancelled => {
                    tasks.wait(task_id, Some(lease), "任务被中断（daemon 停止或取消），待下次启动重跑", now)
                }
                _ => tasks.fail(task_id, Some(lease), &error, now),
            };
        }
        self.db.audit("scheduler", &format!("job_{}", status.as_str()), &format!("{} [{}] {}{}", job.name, trigger.as_str(), summary, error))?;
        Ok(RunResult { job_id: job.id.clone(), job_name: job.name.clone(), run_id, trigger, status, summary, error })
    }
}

#[cfg(test)]
mod tests;
