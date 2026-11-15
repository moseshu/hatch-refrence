//! One task-bound execution service, shared by the three paths that
//! can run an approved action:
//!
//! * the CLI (`hatch approvals approve`),
//! * the HTTP API (`POST /v1/approvals/{id}/approve`),
//! * the worker recovery path (a resume picks up an approval that was
//!   decided but never executed — e.g. the process died between the
//!   decision and the execution — and runs it here).
//!
//! Every path goes through the same checks, in the same order:
//!
//! 1. the approval must still be pending (approve) or approved and
//!    unexecuted (recovery);
//! 2. a task-bound approval is only valid at the plan version it was
//!    bound to — a newer version invalidates it first, so no other
//!    path can execute it either (M1 §10.2);
//! 3. a cancelled task's approvals never execute;
//! 4. the executor is chosen by the bound task's source — a Gmail
//!    task sends with the Gmail sender, a fixture task lands in the
//!    outbox (M2), `--dry-run` always forces the outbox;
//! 5. the execution itself runs through [`execute_approved`], so the
//!    idempotency ledger, the 72h TTL and the approval snapshot keep
//!    their semantics: a second attempt reports `AlreadyDone`, a
//!    crashed mid-flight attempt reports `Ambiguous` and is never
//!    retried automatically.
//!
//! The service lives in `hatch-cli` on purpose: the checks need the
//! task ledger (`hatch-tasks`) and the executors (`hatch-tools`) at
//! the same time, and `hatch-tools` must not depend on `hatch-tasks`
//! (the M1 layering rule). No other crate grows a second approval
//! path — the chat auto-allow path (`route_action`) decides and
//! executes atomically inside one request, so the stale-version and
//! cancel windows do not apply to it the same way.

use crate::app::App;
use hatch_connectors::mail::gmail::{GmailConfig, GmailSender};
use hatch_connectors::MailSender;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tasks::{Task, TaskApprovals, TaskStatus, TaskStore};
use hatch_tools::{execute_approved, ExecOutcome};
use std::sync::Arc;

/// What the service did. `Refused` means nothing was decided and
/// nothing executed (the approval may have been invalidated as part
/// of the refusal); the other four mirror [`ExecOutcome`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    Executed(String),
    AlreadyDone(String),
    Ambiguous(String),
    Failed(String),
    Refused(String),
}

impl ServiceOutcome {
    /// The wire name used by the HTTP response (`outcome`).
    pub fn outcome_str(&self) -> &'static str {
        match self {
            ServiceOutcome::Executed(_) => "executed",
            ServiceOutcome::AlreadyDone(_) => "already_done",
            ServiceOutcome::Ambiguous(_) => "ambiguous",
            ServiceOutcome::Failed(_) => "failed",
            ServiceOutcome::Refused(_) => "refused",
        }
    }

    /// Human-readable detail (the executor result or the refusal).
    pub fn detail(&self) -> &str {
        match self {
            ServiceOutcome::Executed(r)
            | ServiceOutcome::AlreadyDone(r)
            | ServiceOutcome::Ambiguous(r)
            | ServiceOutcome::Failed(r)
            | ServiceOutcome::Refused(r) => r,
        }
    }
}

/// The guard's verdict: proceed with the bound task (or with no task
/// when the approval is not task-bound), or refuse with a reason.
enum Guard {
    Proceed(Option<Box<Task>>),
    Refused(String),
}

/// The unified approval execution service. Built on `&App` because
/// executor selection needs the app's sender configuration.
pub struct ApprovalService<'a> {
    app: &'a App,
    /// Test seam: a sender to use for Gmail tasks instead of the real
    /// Gmail sender. Production callers leave this unset; drills pass
    /// a recording stub so no real account is ever touched.
    sender_override: Option<Arc<dyn MailSender>>,
}

impl<'a> ApprovalService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self {
            app,
            sender_override: None,
        }
    }

    /// Test seam: build the service with a sender override (see the
    /// `sender_override` field). Test-only: production never
    /// overrides the sender.
    #[cfg(test)]
    pub fn with_sender(app: &'a App, sender: Arc<dyn MailSender>) -> Self {
        Self {
            app,
            sender_override: Some(sender),
        }
    }

    /// Approve (decide) and execute: the CLI and HTTP entry point.
    pub fn approve_and_execute(
        &self,
        id: &str,
        note: &str,
        dry_run: bool,
        now: &Timestamp,
    ) -> Result<ServiceOutcome> {
        let q = ApprovalQueue::new(&self.app.db);
        let current = q.get(id)?.status;
        if current != ApprovalStatus::Pending {
            return Ok(ServiceOutcome::Refused(format!(
                "审批 {id} 当前状态为 {}，不会再次执行",
                current.as_str()
            )));
        }
        let task = match self.guard(id, now)? {
            Guard::Refused(reason) => return Ok(ServiceOutcome::Refused(reason)),
            Guard::Proceed(task) => task,
        };
        let status = q.decide(id, true, note, now)?;
        if status != ApprovalStatus::Approved {
            return Ok(ServiceOutcome::Refused(format!(
                "审批 {id} 状态为 {}（过期的审批永远不会执行）",
                status.as_str()
            )));
        }
        self.execute_decided_with(id, task.as_deref(), dry_run, now)
    }

    /// Execute an already-decided approval: the worker recovery path.
    /// Only an approval in `Approved` state can be recovered this
    /// way; anything else is refused without executing.
    pub fn execute_decided(
        &self,
        id: &str,
        dry_run: bool,
        now: &Timestamp,
    ) -> Result<ServiceOutcome> {
        let q = ApprovalQueue::new(&self.app.db);
        let current = q.get(id)?.status;
        if current != ApprovalStatus::Approved {
            return Ok(ServiceOutcome::Refused(format!(
                "审批 {id} 当前状态为 {}，不能执行（只有已批准未执行的审批可以恢复执行）",
                current.as_str()
            )));
        }
        let task = match self.guard(id, now)? {
            Guard::Refused(reason) => return Ok(ServiceOutcome::Refused(reason)),
            Guard::Proceed(task) => task,
        };
        self.execute_decided_with(id, task.as_deref(), dry_run, now)
    }

    /// Settle every decided-but-unexecuted approval linked to a task,
    /// oldest first. Called by the worker right after it claims a
    /// task and before it dispatches any step, so a decision that was
    /// persisted before a crash is executed exactly once, through the
    /// same checks, instead of leaving the task waiting forever.
    /// Returns one outcome per settled approval; approvals in any
    /// other state are left alone.
    pub fn settle_task_approved(
        &self,
        task_id: &str,
        now: &Timestamp,
    ) -> Result<Vec<(String, ServiceOutcome)>> {
        let mut settled = Vec::new();
        for link in TaskApprovals::new(&self.app.db).for_task(task_id)? {
            let status = ApprovalQueue::new(&self.app.db)
                .get(&link.approval_id)?
                .status;
            if status == ApprovalStatus::Approved {
                let outcome = self.execute_decided(&link.approval_id, false, now)?;
                settled.push((link.approval_id, outcome));
            }
        }
        Ok(settled)
    }

    /// The task-bound guards, shared by the approve and recovery
    /// paths: a task-bound approval is only valid at the plan version
    /// it was bound to, and a cancelled task's approvals never run.
    /// A violation invalidates the approval first, so no other path
    /// can execute it either.
    fn guard(&self, id: &str, now: &Timestamp) -> Result<Guard> {
        let linked = TaskApprovals::new(&self.app.db).linked_version(id)?;
        let Some((task_id, version)) = linked else {
            return Ok(Guard::Proceed(None));
        };
        let task = TaskStore::new(&self.app.db)
            .get(&task_id)?
            .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
        let q = ApprovalQueue::new(&self.app.db);
        if task.plan_version > version {
            let _ = q.invalidate(
                id,
                &format!("计划已变更为 v{}，此审批已失效", task.plan_version),
                now,
            );
            return Ok(Guard::Refused(format!(
                "{id} 已随计划版本失效（绑定于 v{version}，当前 v{}），不会执行。",
                task.plan_version
            )));
        }
        if task.status == TaskStatus::Cancelled {
            let _ = q.invalidate(id, "任务已取消，此审批已失效", now);
            return Ok(Guard::Refused(format!(
                "{id} 所属任务已取消，审批已失效，不会执行。"
            )));
        }
        Ok(Guard::Proceed(Some(Box::new(task))))
    }

    /// Choose the executor registry by the bound task's source (M2):
    /// a Gmail task sends with the Gmail sender, everything else uses
    /// the app's configured sender. `--dry-run` always forces the
    /// outbox, so a dry run can never send for real.
    fn executors_for(
        &self,
        task: Option<&Task>,
        dry_run: bool,
    ) -> Result<hatch_tools::ExecutorRegistry> {
        let app_exec = |dry_run: bool| {
            self.app
                .executors(dry_run)
                .map_err(|e| Error::Config(format!("执行器配置失败：{e}")))
        };
        let Some(task) = task else {
            return app_exec(dry_run);
        };
        if task.source == "gmail" && !dry_run {
            let sender: Arc<dyn MailSender> = match &self.sender_override {
                Some(s) => Arc::clone(s),
                None => Arc::new(GmailSender::new(GmailConfig::cli_path())),
            };
            return self
                .app
                .executors_with_sender(dry_run, Some(sender))
                .map_err(|e| Error::Config(format!("执行器配置失败：{e}")));
        }
        app_exec(dry_run)
    }

    fn execute_decided_with(
        &self,
        id: &str,
        task: Option<&Task>,
        dry_run: bool,
        now: &Timestamp,
    ) -> Result<ServiceOutcome> {
        let exec = self.executors_for(task, dry_run)?;
        match execute_approved(&self.app.db, &exec, id, now)? {
            ExecOutcome::Executed(r) => Ok(ServiceOutcome::Executed(r)),
            ExecOutcome::AlreadyDone(r) => Ok(ServiceOutcome::AlreadyDone(r)),
            ExecOutcome::Ambiguous(r) => Ok(ServiceOutcome::Ambiguous(r)),
            ExecOutcome::Failed(r) => Ok(ServiceOutcome::Failed(r)),
        }
    }
}
