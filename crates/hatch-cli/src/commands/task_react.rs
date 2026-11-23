//! The bounded-ReAct runner (P1, §6.1): one observation-driven
//! problem inside a single step of a durable task.
//!
//! The task has two steps, mirroring the brief's shape: `gather`
//! runs one bounded agent turn (the model reads a page, finds the
//! fact, reports it with evidence), `artifact` writes the findings
//! through the same approval-gated `fs_write` path as the brief
//! (`run_artifact`, file stem `research-v`). The turn's caps come
//! from the task's execution parameters: model calls (the turn's
//! `max_steps`), tool calls and wall-clock time — exceeding any of
//! them cancels the turn, and the partial record stays readable.
//!
//! The turn itself reuses the durable-turn bookkeeping
//! ([`task_turn`]): a kill mid-turn resumes from the recorded
//! decision instead of re-deciding. The scripted model is the
//! stand-in for a real model: it opens the first URL in the goal
//! and answers from the fetched page — it never knows the fact in
//! advance, so the acceptance page is genuinely read, not baked in.

use super::task_plan_mirror::fail_task;
use super::task_react_turn::run_react_turn;
use super::task_run_params::RunParams;
use super::task_steps::{run_artifact, ArtifactResult};
use super::task_worker::{is_cancelled, is_paused, KillPoint, RunnerOutcome, WorkerOpts};
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_eval::World;
use hatch_store::Db;
use hatch_tasks::{EventLog, ExecStatus, Lease, StepSpec, TaskStore, UnitStore};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// The ReAct task's durable steps: research, then the artifact.
const REACT_STEPS: [(&str, &str, &str, &str); 2] = [
    (
        "gather",
        "网上调研：打开页面、找出要点",
        "agent.turn",
        "调研要点已找出，注明来源",
    ),
    (
        "artifact",
        "写出调研文件",
        "fs_write",
        "调研文件已写入，可供交付",
    ),
];

/// The caps for one ReAct turn (P1, §6.1): 0 = the runner default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReactCaps {
    pub max_steps: usize,
    pub max_tool_calls: usize,
    pub timeout_secs: u64,
}

impl Default for ReactCaps {
    fn default() -> Self {
        ReactCaps { max_steps: 6, max_tool_calls: 20, timeout_secs: 300 }
    }
}

impl ReactCaps {
    pub fn from_params(params: &RunParams) -> ReactCaps {
        ReactCaps {
            max_steps: if params.react_max_steps == 0 {
                6
            } else {
                params.react_max_steps as usize
            },
            max_tool_calls: if params.react_max_tool_calls == 0 {
                20
            } else {
                params.react_max_tool_calls as usize
            },
            timeout_secs: if params.react_timeout_secs == 0 {
                300
            } else {
                params.react_timeout_secs
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_react(
    db: &Arc<Db>,
    world: &World,
    out_dir: &Path,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    opts: &WorkerOpts,
    budget: u64,
    params: Option<&RunParams>,
) -> Result<RunnerOutcome> {
    let db_ref: &Db = db;
    let store = TaskStore::new(db_ref);
    let units = UnitStore::new(db_ref);
    let log = EventLog::new(db_ref);
    let steps: Vec<StepSpec> = REACT_STEPS
        .iter()
        .map(|(id, title, tool, expected)| StepSpec {
            step_id: id.to_string(),
            title: title.to_string(),
            tool: tool.to_string(),
            arguments: json!({}),
            expected: expected.to_string(),
        })
        .collect();
    units.ensure_steps(task_id, lease, &steps, now)?;

    let mut outcome = RunnerOutcome::default();
    for step in units.list_steps(task_id)? {
        if step.status == ExecStatus::Done {
            continue;
        }
        if is_cancelled(&store, task_id)? {
            outcome.cancelled = true;
            return Ok(outcome);
        }
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
            "gather" => {
                let prompt = params.and_then(|p| p.prompt.clone()).unwrap_or_else(|| {
                    format!("调研：{}", store.get(task_id).ok().flatten().map(|t| t.title).unwrap_or_default())
                });
                let caps = params.map(ReactCaps::from_params).unwrap_or_default();
                let (answer, calls, prompt_tokens, completion_tokens) = run_react_turn(
                    db,
                    task_id,
                    lease,
                    now,
                    tz,
                    opts.kill,
                    opts.router.as_deref(),
                    // Browser tools join the turn when the app has a
                    // broker (`[browser]` enabled); drills without an
                    // app keep the fetch-only registry.
                    opts.app.as_ref().and_then(|a| a.browser()),
                    &prompt,
                    &world.persona.email,
                    &world.persona.name,
                    caps,
                )?;
                outcome.model_calls += calls;
                outcome.prompt_tokens += prompt_tokens;
                outcome.completion_tokens += completion_tokens;
                if matches!(opts.kill, KillPoint::AfterTurn) {
                    return Err(Error::Invalid(
                        "drill: worker terminated after gather turn".into(),
                    ));
                }
                units.complete_step(task_id, lease, "gather", ExecStatus::Done, &answer, now)?;
            }
            "artifact" => match run_artifact(
                db_ref,
                task_id,
                lease,
                &units,
                out_dir,
                now,
                tz,
                &world.persona.email,
                &world.persona.name,
                "research-v",
            )? {
                ArtifactResult::Done(path) => {
                    units.complete_step(
                        task_id,
                        lease,
                        "artifact",
                        ExecStatus::Done,
                        &format!("调研已写入 {}", path.display()),
                        now,
                    )?;
                }
                ArtifactResult::AwaitingApproval(id) => {
                    outcome.blocked = Some(format!(
                        "等待审批 {id}：调研写入需人工批准，批准后重新运行任务继续"
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
                    fail_task(db_ref, &store, task_id, lease, &reason, tz, now)?;
                    return Err(Error::Invalid(reason));
                }
            },
            _ => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_default_when_params_leave_them_zero() {
        let params = RunParams {
            version: 1,
            kind: "react".into(),
            source: "fixture".into(),
            world: None,
            out_dir: None,
            title: "调研".into(),
            prompt: None,
            timezone: "Asia/Shanghai".into(),
            gmail_query: "is:unread".into(),
            gmail_max: 10,
            gmail_user: None,
            gmail_name: None,
            invite: None,
            max_model_calls: 0,
            lease_ttl_secs: 0,
            mode: "bounded-react".into(),
            react_max_steps: 0,
            react_max_tool_calls: 0,
            react_timeout_secs: 0,
        };
        let caps = ReactCaps::from_params(&params);
        assert_eq!((caps.max_steps, caps.max_tool_calls, caps.timeout_secs), (6, 20, 300));
    }
}
