//! The M0 durable task worker (item 6): runs the meeting-brief world
//! as durable steps on a database, so a client can exit mid-run and
//! reconnect later to read the same task, steps, and events from the
//! cursor. A crash leaves the partial record behind; a new worker
//! instance over the same database resumes from `resume_position`.
//!
//! The task is the read-only cross-app brief — "明天开会前，把李婷最
//! 近邮件里的决定整理成简报" — as three steps:
//!
//! 1. `triage`: the triage workflow syncs the fixture mailbox into the
//!    mail repo and classifies it (script model: the eval oracle).
//! 2. `gather`: one Agent turn with a script model over the seeded
//!    repos (see `task_gather`); the `on_step_event` hook records the
//!    Decision before dispatch and each Result after it lands.
//! 3. `artifact`: the brief text is written to `brief-v<n>.md` in the
//!    output dir (version = 1 + prior `artifact_written` events).
//!
//! Nothing is ever sent: the world has no mail that needs a reply.

use super::task_gather::run_gather;
use super::task_steps::{run_triage, write_artifact};
use hatch_core::time::{parse_rfc3339, parse_tz, Timestamp};
use hatch_core::{Error, Result};
use hatch_eval::runner::seed_world;
use hatch_eval::World;
use hatch_store::Db;
use hatch_tasks::{
    EventLog, ExecStatus, StepSpec, TaskStatus, TaskStore, UnitCallSpec, UnitStore,
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

#[derive(Clone, Debug, Default)]
pub struct WorkerOpts {
    /// Resume this task instead of creating a new one.
    pub task_id: Option<String>,
    /// Lease TTL in seconds (default 300; drills use 1 to force expiry).
    pub lease_ttl_secs: u64,
    /// Clock for this run (default: the world's `persona.now`). Drills
    /// advance it so a 1-second lease is already stale on the second run.
    pub now_override: Option<Timestamp>,
    pub kill: KillPoint,
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
}

const STEPS: [(&str, &str, &str); 3] = [
    ("triage", "分诊收件箱", "triage.run"),
    ("gather", "查日程与邮件，组装简报", "agent.turn"),
    ("artifact", "写简报文件", "artifact.write"),
];

/// Run (or resume) the meeting-brief task for one eval world.
/// Returns the report on completion; returns `Err` when killed (drill)
/// or failed — the task stays `running` with its partial record, and a
/// later call with the same `task_id` (or a fresh claim after the lease
/// expires) resumes it.
pub fn run_worker(
    db: &Arc<Db>,
    world_dir: &Path,
    out_dir: &Path,
    opts: WorkerOpts,
) -> Result<WorkerReport> {
    let started = Instant::now();
    let db_ref: &Db = db;
    let world = World::load(world_dir)?;
    let now = match opts.now_override {
        Some(t) => t,
        None => parse_rfc3339(&world.persona.now)?,
    };
    let tz = parse_tz(&world.persona.timezone)?;
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);

    // Claim: a fresh task, or resume the named one. A stale lease can
    // be reclaimed (the previous worker is presumed dead); a live one
    // cannot be taken over.
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
                _ => {
                    let t = store.reclaim(id, "worker", ttl(&opts), &now)?;
                    (id.clone(), t.lease().unwrap())
                }
            }
        }
        None => {
            let title = format!(
                "简报：{}",
                world
                    .expectations
                    .brief
                    .as_ref()
                    .map(|b| b.event_title.as_str())
                    .unwrap_or("明天会议")
            );
            let task = store.create("meeting-brief", &title, "worker", "", 3, None, &now)?;
            let claimed = store.start(&task.id, "worker", ttl(&opts), &now)?;
            (task.id, claimed.lease().unwrap())
        }
    };
    let _ = log.append_event(&task_id, &lease, "task_started", "", "简报任务开始", &now);
    seed_world(db_ref, &world, &now)?;
    let steps: Vec<StepSpec> = STEPS
        .iter()
        .map(|(id, title, tool)| StepSpec {
            step_id: id.to_string(),
            title: title.to_string(),
            tool: tool.to_string(),
            arguments: json!({"world": world_dir.to_string_lossy()}),
        })
        .collect();
    units.ensure_steps(&task_id, &lease, &steps, &now)?;

    let mut model_calls = 0u64;
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    for step in units.list_steps(&task_id)? {
        if step.status == ExecStatus::Done {
            continue;
        }
        store.heartbeat(&task_id, &lease, &now)?;
        let _ = log.append_event(
            &task_id,
            &lease,
            "step_started",
            &step.step_id,
            &step.title,
            &now,
        );
        units.start_step(&task_id, &lease, &step.step_id, &now)?;
        match step.step_id.as_str() {
            "triage" => {
                let unit = units.begin_unit(
                    &task_id,
                    &lease,
                    "triage",
                    &[UnitCallSpec {
                        id: "triage.run".into(),
                        tool: "triage.run".into(),
                        arguments: json!({}),
                    }],
                    &now,
                )?;
                let r = run_triage(db_ref, &world, &now, &tz, out_dir)?;
                model_calls += r.model_calls;
                prompt_tokens += r.prompt_tokens;
                completion_tokens += r.completion_tokens;
                let summary = format!(
                    "分诊完成：处理 {} 封，模型调用 {}",
                    r.processed, r.model_calls
                );
                let _ = units.record_result(
                    &task_id,
                    &lease,
                    unit.unit_seq,
                    "triage.run",
                    "triage.run",
                    &summary,
                    "system",
                    &now,
                );
                let _ = units.finish_unit(
                    &task_id,
                    &lease,
                    unit.unit_seq,
                    ExecStatus::Done,
                    r.model_calls,
                    1,
                    &now,
                );
                units.complete_step(
                    &task_id,
                    &lease,
                    "triage",
                    ExecStatus::Done,
                    &summary,
                    &now,
                )?;
            }
            "gather" => {
                let (answer, calls, prompt, completion) =
                    run_gather(db, &world, &task_id, &lease, &now, &tz, opts.kill)?;
                model_calls += calls;
                prompt_tokens += prompt;
                completion_tokens += completion;
                if matches!(opts.kill, KillPoint::AfterTurn) {
                    return Err(Error::Invalid(
                        "drill: worker terminated after gather turn".into(),
                    ));
                }
                units.complete_step(&task_id, &lease, "gather", ExecStatus::Done, &answer, &now)?;
            }
            "artifact" => {
                let path = write_artifact(db_ref, &task_id, &lease, &units, out_dir, &now)?;
                units.complete_step(
                    &task_id,
                    &lease,
                    "artifact",
                    ExecStatus::Done,
                    &format!("简报已写入 {}", path.display()),
                    &now,
                )?;
            }
            _ => {}
        }
        if let KillPoint::AfterStep(name) = opts.kill {
            if name == step.step_id {
                return Err(Error::Invalid(format!(
                    "drill: worker terminated after step {name}"
                )));
            }
        }
    }
    store.finish(&task_id, Some(&lease), &now)?;
    let _ = log.append_event(&task_id, &lease, "task_done", "", "简报任务完成", &now);
    finished_report(db_ref, &task_id, started)
        .map(|r| r.with_usage(model_calls, prompt_tokens, completion_tokens))
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
    })
}
