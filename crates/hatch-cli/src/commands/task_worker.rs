//! The M1 durable task worker (M0 item 6, extended): runs a task's
//! steps on a database, so a client can exit mid-run and reconnect
//! later to read the same task, steps, and events from the cursor. A
//! crash leaves the partial record behind; a new worker instance over
//! the same database resumes from `resume_position`.
//!
//! M1 adds: an approval-gated step blocks the task (`Waiting` is for
//! budgets; `Blocked` is for approvals) instead of running ahead;
//! a model-call budget pauses the task with its completed steps still
//! readable; a cancel stops the worker at the next step boundary;
//! the worker dispatches on the task's kind (see `task_kinds`); and
//! `--live` runs the steps on a real model router.
//!
//! The meeting-brief task is the read-only cross-app brief — "明天开
//! 会前，把李婷最近邮件里的决定整理成简报" — as three steps:
//!
//! 1. `triage`: the triage workflow syncs the fixture mailbox into the
//!    mail repo and classifies it (script model: the eval oracle).
//! 2. `gather`: one Agent turn with a script model over the seeded
//!    repos (see `task_gather`); the `on_step_event` hook records the
//!    Decision before dispatch and each Result after it lands.
//! 3. `artifact`: the brief is written to `brief-v<n>.md` through the
//!    `fs_write` approval gate (see `task_steps::run_artifact`).
//!
//! Nothing is ever sent: the world has no mail that needs a reply.

use super::task_gather::run_gather;
use super::task_invite::run_invite;
use super::task_kinds::{is_known_kind, DEFAULT_KIND};
use super::task_steps::{run_artifact, run_triage, ArtifactResult};
use hatch_core::time::{now as now_now, parse_rfc3339, parse_tz, Timestamp};
use hatch_core::{Error, Result};
use hatch_eval::runner::seed_world;
use hatch_eval::World;
use hatch_llm::Router;
use hatch_store::Db;
use hatch_tasks::{
    BlockedKind, EventLog, ExecStatus, StepSpec, TaskStatus, TaskStore, UnitCallSpec, UnitStore,
    DEFAULT_LEASE_TTL_SECS,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Where a drill run pretends the worker died. `None` = run to completion.
/// The kill variants are constructed only by the recovery drills in
/// `task_drill` (`#[cfg(test)]`); production runs always use `None`.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KillPoint {
    #[default]
    None,
    /// Die right after the first model decision batch is recorded
    /// (mid-turn: the unit exists, its results do not yet).
    AfterDecision,
    /// Die after the gather turn returned, before its step is completed.
    AfterTurn,
    /// Die after the named step is completed (step + event committed).
    AfterStep(&'static str),
}

#[derive(Clone, Default)]
pub struct WorkerOpts {
    /// Resume this task instead of creating a new one.
    pub task_id: Option<String>,
    /// The kind of a task created by this run (default:
    /// `meeting-brief`); a resumed task always keeps its stored kind.
    pub kind: Option<String>,
    /// Lease TTL in seconds (default 300; drills use 1 to force expiry).
    pub lease_ttl_secs: u64,
    /// Clock for this run (default: the real wall clock — leases,
    /// events and approval TTLs are real-time). Drills override it
    /// with the world's `persona.now` so a 1-second lease is already
    /// stale on the second run.
    pub now_override: Option<Timestamp>,
    pub kill: KillPoint,
    /// A real model router (`--live`): `None` keeps the scripted
    /// model, so CI makes zero real calls.
    pub router: Option<Arc<Router>>,
    /// Model-call budget for this run (`None` = the task's stored
    /// budget; 0 = unlimited).
    pub max_model_calls: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct WorkerReport {
    pub task_id: String,
    pub steps_done: usize,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub artifact: Option<PathBuf>,
    pub duration_ms: u64,
    pub steps_total: usize,
    /// The worker paused on an approval: the approval id it waits on.
    pub blocked: Option<String>,
    /// The worker paused because the model-call budget ran out.
    pub budget_stopped: bool,
    /// The task was cancelled while the worker was running.
    pub cancelled: bool,
    /// The task was paused by the user (task 5): the worker stopped
    /// at a step boundary and the task is already Blocked/paused.
    pub paused: bool,
}

/// The brief's durable steps: id, title, worker tool, and what the
/// step must produce (task 5 `expected`, shown in the planning list).
const STEPS: [(&str, &str, &str, &str); 3] = [
    (
        "triage",
        "分诊收件箱",
        "triage.run",
        "收件箱已分诊，待决策的邮件已列出",
    ),
    (
        "gather",
        "查日程与邮件，组装简报",
        "agent.turn",
        "简报内容已组装，引用来源完整",
    ),
    (
        "artifact",
        "写简报文件",
        "artifact.write",
        "简报文件已写入，可供交付",
    ),
];

/// What one kind's runner produced. The caller (`run_worker`) owns
/// the task-level transition (finish / block / wait), so every kind
/// reports its outcome in the same shape.
#[derive(Clone, Debug, Default)]
pub struct RunnerOutcome {
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Paused on an approval: the approval id it waits on.
    pub blocked: Option<String>,
    /// The step the worker is waiting on (the plan, or the step that
    /// queued the approval); recorded on the event.
    pub blocked_step: Option<String>,
    /// Paused because the model-call budget ran out.
    pub budget_stopped: bool,
    /// The task was cancelled while the runner was running.
    pub cancelled: bool,
    /// The task was paused by the user (task 5): the runner stopped
    /// at a step boundary and claimed nothing after the pause.
    pub paused: bool,
}

/// Run (or resume) the task for one eval world, dispatching on the
/// task's kind. Returns the report on completion, on a pause (blocked
/// / budget / cancelled — the task already holds the pause), and on a
/// step failure that the worker itself recorded as a task failure;
/// returns `Err` when killed (drill) or when the task cannot be
/// claimed — the task keeps its partial record, and a later call with
/// the same `task_id` (or a fresh claim after the lease expires)
/// resumes it.
pub fn run_worker(
    db: &Arc<Db>,
    world_dir: &Path,
    out_dir: &Path,
    opts: WorkerOpts,
) -> Result<WorkerReport> {
    let started = Instant::now();
    let db_ref: &Db = db;
    let world = World::load(world_dir)?;
    // Two clocks (M1): the runtime clock is real time — leases,
    // events and approval TTLs are only meaningful in it — while the
    // fixture clock (`persona.now`) anchors the world's own date
    // arithmetic (tomorrow's meeting). A worker that stamped approvals
    // with the fixture's frozen 2026-11-03 would hand the human
    // approvals that are already expired in real time.
    // Drills pass `now_override` = the world's now, so the two clocks
    // coincide there and nothing changes for them.
    let world_now = parse_rfc3339(&world.persona.now)?;
    let now = match opts.now_override {
        Some(t) => t,
        None => now_now(),
    };
    let tz = parse_tz(&world.persona.timezone)?;
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);

    // Claim — one transition per status, never the M0 catch-all
    // reclaim (a Blocked task is not running, so `reclaim` refused
    // it; a Failed/Cancelled task is never resumed).
    let (task_id, lease) = match &opts.task_id {
        Some(id) => {
            let task = store
                .get(id)?
                .ok_or_else(|| Error::NotFound(format!("task {id} 不存在")))?;
            match task.status {
                TaskStatus::Done => return finished_report(db_ref, id, started),
                TaskStatus::Queued => {
                    let t = store.start(id, "worker", ttl(&opts), &now)?;
                    (id.clone(), t.lease().unwrap())
                }
                TaskStatus::Blocked => {
                    let t = store.unblock(id, "worker", ttl(&opts), &now)?;
                    (id.clone(), t.lease().unwrap())
                }
                TaskStatus::Waiting => {
                    let t = store.retry(id, "worker", ttl(&opts), &now)?;
                    (id.clone(), t.lease().unwrap())
                }
                TaskStatus::Running => {
                    let t = store.reclaim(id, "worker", ttl(&opts), &now)?;
                    (id.clone(), t.lease().unwrap())
                }
                TaskStatus::Failed | TaskStatus::Cancelled => {
                    return Err(Error::Invalid(format!(
                        "task {} 已{}，不能继续执行",
                        id,
                        task.status.as_str()
                    )));
                }
            }
        }
        None => {
            let kind = opts.kind.clone().unwrap_or_else(|| DEFAULT_KIND.into());
            if !is_known_kind(&kind) {
                return Err(Error::Invalid(format!(
                    "未知任务 kind {kind}（已知 kind：{}）",
                    super::task_kinds::KNOWN_KINDS.join("、")
                )));
            }
            let event_title = world
                .expectations
                .brief
                .as_ref()
                .map(|b| b.event_title.as_str())
                .unwrap_or("明天会议");
            let title = match kind.as_str() {
                "meeting-invite" => format!("邀请：{event_title}"),
                _ => format!("简报：{event_title}"),
            };
            let task = store.create(&kind, &title, "worker", "", 3, None, &now)?;
            let claimed = store.start(&task.id, "worker", ttl(&opts), &now)?;
            (task.id, claimed.lease().unwrap())
        }
    };
    // This run's budget override (the claiming worker owns the lease,
    // so it may raise the budget it was resumed with).
    if let Some(b) = opts.max_model_calls {
        store.set_budget_leased(&task_id, &lease, b, &now)?;
    }
    let task = store
        .get(&task_id)?
        .ok_or_else(|| Error::NotFound(format!("task {task_id} 不存在")))?;
    let _ = log.append_event(&task_id, &lease, "task_started", "", "任务开始", &now);
    seed_world(db_ref, &world, &now)?;

    let outcome = match task.kind.as_str() {
        "meeting-brief" => run_brief(
            db,
            world_dir,
            &world,
            out_dir,
            &task_id,
            &lease,
            &now,
            &world_now,
            &tz,
            &opts,
            task.max_model_calls,
        )?,
        "meeting-invite" => {
            run_invite(db, &world, out_dir, &task_id, &lease, &now, &world_now, &tz)?
        }
        _ => {
            return Err(Error::Invalid(format!(
                "任务 kind {} 尚未接入 worker（已知 kind：{}）",
                task.kind,
                super::task_kinds::KNOWN_KINDS.join("、")
            )));
        }
    };
    let mut report = finished_report(db_ref, &task_id, started)?.with_usage(
        outcome.model_calls,
        outcome.prompt_tokens,
        outcome.completion_tokens,
    );
    report.blocked = outcome.blocked.clone();
    report.budget_stopped = outcome.budget_stopped;
    report.cancelled = outcome.cancelled;
    report.paused = outcome.paused;
    if outcome.cancelled {
        // The task is already Cancelled; nothing left to transition.
        return Ok(report);
    }
    if outcome.paused || is_paused(&store, &task_id)? {
        // The user paused the task — either the runner stopped at a
        // boundary, or the pause landed after its last check (e.g.
        // mid-`run_plan`). The task is already Blocked/paused, so
        // nothing is left to transition — and `finish` must not run
        // on it. A resume re-reads the ledger from the breakpoint.
        report.paused = true;
        return Ok(report);
    }
    if let Some(reason) = &outcome.blocked {
        // The event goes first: once the task is Blocked the
        // lease-guarded event append refuses.
        let _ = log.append_event(
            &task_id,
            &lease,
            "approval_requested",
            outcome.blocked_step.as_deref().unwrap_or(""),
            reason,
            &now,
        );
        store.block(&task_id, Some(&lease), BlockedKind::Approval, reason, &now)?;
        return Ok(report);
    }
    if outcome.budget_stopped {
        let used = units.total_model_calls(&task_id)?;
        let note = format!("已用 {used} 次模型调用，超出预算 {}", task.max_model_calls);
        let _ = log.append_event(&task_id, &lease, "budget_exceeded", "", &note, &now);
        store.wait(&task_id, Some(&lease), &note, &now)?;
        return Ok(report);
    }
    store.finish(&task_id, Some(&lease), &now)?;
    let _ = log.append_event(&task_id, &lease, "task_done", "", "任务完成", &now);
    Ok(report)
}

/// The meeting-brief runner: the three M0 steps over the world.
/// `world_now` is the fixture clock (the world's `persona.now`),
/// used only for the fixture's own date arithmetic (the gather
/// script model's calendar window); everything else runs on `now`.
#[allow(clippy::too_many_arguments)]
fn run_brief(
    db: &Arc<Db>,
    world_dir: &Path,
    world: &World,
    out_dir: &Path,
    task_id: &str,
    lease: &hatch_tasks::Lease,
    now: &Timestamp,
    world_now: &Timestamp,
    tz: &chrono_tz::Tz,
    opts: &WorkerOpts,
    budget: u64,
) -> Result<RunnerOutcome> {
    let db_ref: &Db = db;
    let db_arc = Arc::clone(db);
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);
    let steps: Vec<StepSpec> = STEPS
        .iter()
        .map(|(id, title, tool, expected)| StepSpec {
            step_id: id.to_string(),
            title: title.to_string(),
            tool: tool.to_string(),
            arguments: json!({"world": world_dir.to_string_lossy()}),
            expected: expected.to_string(),
        })
        .collect();
    units.ensure_steps(task_id, lease, &steps, now)?;

    let mut outcome = RunnerOutcome::default();
    for step in units.list_steps(task_id)? {
        if step.status == ExecStatus::Done {
            continue;
        }
        // Step boundary: cancel stops the run; a spent budget pauses
        // it (the completed steps stay readable).
        if is_cancelled(&store, task_id)? {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        // A user pause stops the run the same way, before the next
        // step is claimed (task 5); the task is already Blocked, so
        // no transition is needed here.
        if is_paused(&store, task_id)? {
            outcome.paused = true;
            return Ok(outcome);
        }
        if budget > 0 && units.total_model_calls(task_id)? >= budget {
            outcome.budget_stopped = true;
            return Ok(outcome);
        }
        store.heartbeat(task_id, lease, now)?;
        let _ = log.append_event(
            task_id,
            lease,
            "step_started",
            &step.step_id,
            &step.title,
            now,
        );
        units.start_step(task_id, lease, &step.step_id, now)?;
        match step.step_id.as_str() {
            "triage" => {
                let unit = units.begin_unit(
                    task_id,
                    lease,
                    "triage",
                    &[UnitCallSpec {
                        id: "triage.run".into(),
                        tool: "triage.run".into(),
                        arguments: json!({}),
                    }],
                    now,
                )?;
                let r = run_triage(db_ref, world, now, tz, out_dir, opts.router.as_deref())?;
                outcome.model_calls += r.model_calls;
                outcome.prompt_tokens += r.prompt_tokens;
                outcome.completion_tokens += r.completion_tokens;
                let summary = format!(
                    "分诊完成：处理 {} 封，模型调用 {}",
                    r.processed, r.model_calls
                );
                let _ = units.record_result(
                    task_id,
                    lease,
                    unit.unit_seq,
                    "triage.run",
                    "triage.run",
                    &summary,
                    "system",
                    now,
                );
                let _ = units.finish_unit(
                    task_id,
                    lease,
                    unit.unit_seq,
                    ExecStatus::Done,
                    r.model_calls,
                    1,
                    now,
                );
                units.complete_step(task_id, lease, "triage", ExecStatus::Done, &summary, now)?;
            }
            "gather" => {
                let (answer, calls, prompt, completion) = run_gather(
                    &db_arc,
                    world,
                    task_id,
                    lease,
                    now,
                    world_now,
                    tz,
                    opts.kill,
                    opts.router.as_deref(),
                )?;
                outcome.model_calls += calls;
                outcome.prompt_tokens += prompt;
                outcome.completion_tokens += completion;
                if matches!(opts.kill, KillPoint::AfterTurn) {
                    return Err(Error::Invalid(
                        "drill: worker terminated after gather turn".into(),
                    ));
                }
                units.complete_step(task_id, lease, "gather", ExecStatus::Done, &answer, now)?;
            }
            "artifact" => {
                match run_artifact(
                    db_ref,
                    task_id,
                    lease,
                    &units,
                    out_dir,
                    now,
                    tz,
                    &world.persona.email,
                    &world.persona.name,
                )? {
                    ArtifactResult::Done(path) => {
                        units.complete_step(
                            task_id,
                            lease,
                            "artifact",
                            ExecStatus::Done,
                            &format!("简报已写入 {}", path.display()),
                            now,
                        )?;
                    }
                    ArtifactResult::AwaitingApproval(id) => {
                        outcome.blocked = Some(format!(
                            "等待审批 {id}：简报写入需人工批准，批准后重新运行任务继续"
                        ));
                        outcome.blocked_step = Some("artifact".into());
                        return Ok(outcome);
                    }
                    ArtifactResult::Dead(reason) => {
                        units.complete_step(
                            task_id,
                            lease,
                            "artifact",
                            ExecStatus::Failed,
                            &reason,
                            now,
                        )?;
                        // The step failed terminally; fail the task
                        // with the same note instead of leaving it
                        // Running — and surface the failure.
                        let _ = store.fail(task_id, Some(lease), &reason, now);
                        return Err(Error::Invalid(reason));
                    }
                }
            }
            _ => {}
        }
        // A cancel that landed mid-step stops the run before the
        // next step starts (the step that just ran stays recorded).
        if is_cancelled(&store, task_id)? {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        if let KillPoint::AfterStep(name) = opts.kill {
            if name == step.step_id {
                return Err(Error::Invalid(format!(
                    "drill: worker terminated after step {name}"
                )));
            }
        }
    }
    Ok(outcome)
}

/// Whether the task was cancelled (a control-plane action the worker
/// must respect at the next step boundary).
pub(crate) fn is_cancelled(store: &TaskStore, task_id: &str) -> Result<bool> {
    Ok(store
        .get(task_id)?
        .is_some_and(|t| t.status == TaskStatus::Cancelled))
}

/// Whether the user paused the task (task 5): blocked with kind
/// `paused`. The worker must respect it at the next step boundary,
/// like a cancel — but unlike a cancel it is resumable.
pub(crate) fn is_paused(store: &TaskStore, task_id: &str) -> Result<bool> {
    Ok(store
        .get(task_id)?
        .is_some_and(|t| t.status == TaskStatus::Blocked && t.blocked_kind == BlockedKind::Paused))
}

fn ttl(opts: &WorkerOpts) -> u64 {
    if opts.lease_ttl_secs == 0 {
        DEFAULT_LEASE_TTL_SECS
    } else {
        opts.lease_ttl_secs
    }
}

impl WorkerReport {
    fn with_usage(mut self, calls: u64, prompt: u64, completion: u64) -> Self {
        self.model_calls = calls;
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self
    }
}

fn finished_report(db: &Db, task_id: &str, started: Instant) -> Result<WorkerReport> {
    let units = UnitStore::new(db);
    let steps = units.list_steps(task_id)?;
    let done = steps
        .iter()
        .filter(|s| s.status == ExecStatus::Done)
        .count();
    let total = steps.len();
    let artifact = steps
        .iter()
        .find(|s| s.step_id == "artifact")
        .map(|s| s.output.clone())
        .filter(|o| !o.is_empty())
        .and_then(|o| o.strip_prefix("简报已写入 ").map(PathBuf::from));
    Ok(WorkerReport {
        task_id: task_id.to_string(),
        steps_done: done,
        model_calls: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        artifact,
        duration_ms: started.elapsed().as_millis() as u64,
        steps_total: total,
        blocked: None,
        budget_stopped: false,
        cancelled: false,
        paused: false,
    })
}
