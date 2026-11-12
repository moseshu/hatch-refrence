//! The gather step of the M0 durable task worker: one Agent turn with
//! a script model over the meeting-brief world.
//!
//! The model decides the read batch first (`calendar_list` +
//! `mail_search`, real tools against the seeded repos), then answers
//! with the brief text assembled deterministically from the same
//! database the tools read (`assemble_brief`). The `on_step_event`
//! hook records the Decision before dispatch and each Result after it
//! lands, so the unit's calls and observations stay paired.
//!
//! Resume: when a unit has pending calls, the worker re-dispatches
//! them directly (no model call — the batch is already recorded),
//! replays the batch + observations into the session, and lets the
//! model answer from the replayed history. A model that re-decides a
//! batch the store already has reuses that unit instead of starting a
//! duplicate.

use super::task_worker::KillPoint;
use chrono::TimeZone;
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore, StepEvent};
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_eval::meeting_brief::assemble_brief;
use hatch_eval::World;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatMessage, ChatResponse, LlmError, Role, Router, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{ExecStatus, Lease, UnitCallSpec, UnitStore};
use hatch_tools::builtin::default_registry;
use hatch_tools::{dispatch_call, ExecutorRegistry, ToolContext};
use serde_json::json;
use std::cell::Cell;
use std::sync::Arc;

/// One Agent turn for the `gather` step. Returns
/// `(answer, model_calls, prompt_tokens, completion_tokens)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_gather(
    db: &Arc<Db>,
    world: &World,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    kill: KillPoint,
) -> Result<(String, u64, u64, u64)> {
    let db_ref: &Db = db;
    let units = UnitStore::new(db_ref);
    let sessions = SessionStore::new(db_ref);
    let session = sessions
        .list(false, 100)?
        .into_iter()
        .find(|s| s.title == format!("task:{task_id}"))
        .map(|s| s.id)
        .unwrap_or_else(|| {
            sessions
                .create(&format!("task:{task_id}"), now)
                .unwrap_or_default()
        });
    let current_unit: Cell<Option<i64>> = Cell::new(None);
    let answer: Cell<Option<String>> = Cell::new(None);
    // Resume: a unit with pending calls is re-dispatched directly (the
    // model batch is already recorded), then replayed into the session
    // so the turn only has to answer. A unit that is already complete
    // still becomes the current unit, so its counts are kept.
    if let Some(pos) = units.resume_position(task_id)? {
        if pos.step_id == "gather" {
            if let Some(seq) = pos.unit_seq {
                current_unit.set(Some(seq));
                if !pos.pending_calls.is_empty() {
                    resume_dispatch(
                        db_ref,
                        task_id,
                        world,
                        &pos.pending_calls,
                        lease,
                        seq,
                        now,
                        tz,
                    )?;
                    replay_into_session(db_ref, &sessions, &session, task_id, seq, now)?;
                }
            }
        }
    }
    let db_arc = Arc::clone(db);
    let world_arc = Arc::new(world.clone());
    let now_copy = *now;
    let tz_copy = *tz;
    let model = FnModel::new("brief-script", move |req| {
        let seen_tool = req.messages.iter().any(|m| m.role == Role::Tool);
        if seen_tool {
            let brief = assemble_brief(&db_arc, &world_arc)
                .map_err(|e| LlmError::permanent(e.to_string()))?;
            Ok(ChatResponse::text(brief.text))
        } else {
            let from = now_copy + chrono::Duration::days(1);
            let from_local = tz_copy.from_utc_datetime(&from.naive_utc());
            let from_str = from_local.format("%Y-%m-%d %H:%M").to_string();
            Ok(ChatResponse::calls(vec![
                ToolCall {
                    id: "c-cal".into(),
                    name: "calendar_list".into(),
                    arguments: json!({"from": from_str, "days": 1}),
                },
                ToolCall {
                    id: "c-mail".into(),
                    name: "mail_search".into(),
                    arguments: json!({"query": "李婷", "limit": 10}),
                },
            ]))
        }
    });
    let router = Router::single(Arc::new(model));
    let tools = default_registry();
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let agent = Agent {
        db: db_ref,
        router: &router,
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config: AgentConfig {
            user_name: world.persona.name.clone(),
            user_email: world.persona.email.clone(),
            tz: *tz,
            max_steps: 2,
            ..Default::default()
        },
    };
    let kill_after_decision = matches!(kill, KillPoint::AfterDecision);
    let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        agent.run(
            &session,
            "明天开会前，把李婷最近邮件里的决定整理成简报",
            now,
            RunOptions {
                on_step_event: Some(&mut |ev: StepEvent| {
                    record_event(
                        &units,
                        task_id,
                        lease,
                        &current_unit,
                        &answer,
                        ev,
                        now,
                        kill_after_decision,
                    )
                }),
                ..Default::default()
            },
        )
    }));
    let report = match (kill_after_decision, turn) {
        (true, Err(p)) if is_drill_panic(&p) => {
            return Err(Error::Invalid(
                "drill: worker terminated after decision".into(),
            ));
        }
        (_, Err(p)) => std::panic::resume_unwind(p),
        (_, Ok(r)) => r?,
    };
    let answer = answer
        .take()
        .or(report.answer)
        .ok_or_else(|| Error::Invalid("gather 回合没有产出简报".into()))?;
    if let Some(seq) = current_unit.get() {
        let (mc, tc) = match units.latest_unit(task_id, "gather")? {
            Some(u) if u.unit_seq == seq => (
                u.model_calls + report.model_calls,
                (u.calls.len() as u64).max(report.tool_calls.len() as u64),
            ),
            _ => (report.model_calls, report.tool_calls.len() as u64),
        };
        let _ = units.finish_unit(task_id, lease, seq, ExecStatus::Done, mc, tc, now);
    }
    Ok((
        answer,
        report.model_calls,
        report.usage.prompt_tokens,
        report.usage.completion_tokens,
    ))
}

/// Record one hook event into the unit store. On `Decision`, begin a
/// unit with the recorded batch — or reuse the step's latest unit when
/// it already carries the same call ids (a re-decision after resume).
/// On `Result`, record the observation. On `Answer`, remember the text
/// (the unit is finished by the caller, which owns the turn's counts).
#[allow(clippy::too_many_arguments)]
fn record_event(
    units: &UnitStore,
    task_id: &str,
    lease: &Lease,
    current_unit: &Cell<Option<i64>>,
    answer: &Cell<Option<String>>,
    ev: StepEvent,
    now: &Timestamp,
    kill_after_decision: bool,
) {
    match ev {
        StepEvent::Decision { calls, .. } => {
            let specs: Vec<UnitCallSpec> = calls
                .into_iter()
                .map(|c| UnitCallSpec {
                    id: c.id,
                    tool: c.name,
                    arguments: c.arguments,
                })
                .collect();
            let ids: Vec<&str> = specs.iter().map(|c| c.id.as_str()).collect();
            let reuse = units
                .latest_unit(task_id, "gather")
                .ok()
                .flatten()
                .and_then(|u| {
                    let same: Vec<&str> = u.calls.iter().map(|c| c.id.as_str()).collect();
                    if same == ids {
                        Some(u.unit_seq)
                    } else {
                        None
                    }
                });
            match reuse {
                Some(seq) => current_unit.set(Some(seq)),
                None => {
                    if let Ok(unit) = units.begin_unit(task_id, lease, "gather", &specs, now) {
                        current_unit.set(Some(unit.unit_seq));
                        if kill_after_decision {
                            panic!("drill: terminate after decision");
                        }
                    }
                }
            }
        }
        StepEvent::Result {
            call_id,
            tool,
            content,
            trust,
            ..
        } => {
            if let Some(seq) = current_unit.get() {
                let _ = units.record_result(
                    task_id,
                    lease,
                    seq,
                    &call_id,
                    &tool,
                    &content,
                    trust.as_str(),
                    now,
                );
            }
        }
        StepEvent::Answer { text, .. } => {
            answer.set(Some(text));
        }
    }
}

fn is_drill_panic(p: &Box<dyn std::any::Any + Send>) -> bool {
    p.downcast_ref::<&str>()
        .map_or(false, |s| s.contains("drill: terminate"))
        || p.downcast_ref::<String>()
            .map_or(false, |s| s.contains("drill: terminate"))
}

/// Re-dispatch the pending calls of a recorded unit directly (no model
/// call): the batch is already durable, only the observations are
/// missing. Returns the number of calls re-dispatched.
#[allow(clippy::too_many_arguments)]
fn resume_dispatch(
    db: &Db,
    task_id: &str,
    world: &World,
    pending: &[UnitCallSpec],
    lease: &Lease,
    unit_seq: i64,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
) -> Result<usize> {
    let units = UnitStore::new(db);
    let tools = default_registry();
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let cancel = CancelToken::new();
    let mut tainted = units
        .unit_results(task_id, unit_seq)?
        .iter()
        .any(|r| r.trust == "untrusted");
    for spec in pending {
        let call = ToolCall {
            id: spec.id.clone(),
            name: spec.tool.clone(),
            arguments: spec.arguments.clone(),
        };
        let ctx = ToolContext {
            db,
            now: *now,
            tz: *tz,
            user_email: world.persona.email.clone(),
            origin: format!("task:{unit_seq}"),
            tainted,
            call_id: call.id.clone(),
            cancel: cancel.clone(),
        };
        let out = dispatch_call(&tools, &exec, &gate, &call, &ctx);
        if out.trust.is_untrusted() {
            tainted = true;
        }
        let _ = units.record_result(
            task_id,
            lease,
            unit_seq,
            &call.id,
            &call.name,
            &out.content,
            out.trust.as_str(),
            now,
        );
    }
    Ok(pending.len())
}

/// Replay a recorded unit's batch + observations into the session, so
/// the resumed turn answers from history instead of re-deciding.
fn replay_into_session(
    db: &Db,
    sessions: &SessionStore,
    session_id: &str,
    task_id: &str,
    unit_seq: i64,
    now: &Timestamp,
) -> Result<()> {
    let units = UnitStore::new(db);
    let Some(unit) = units.latest_unit(task_id, "gather")? else {
        return Ok(());
    };
    if unit.unit_seq != unit_seq {
        return Ok(());
    }
    let results = units.unit_results(task_id, unit_seq)?;
    let calls: Vec<ToolCall> = unit
        .calls
        .iter()
        .map(|c| ToolCall {
            id: c.id.clone(),
            name: c.tool.clone(),
            arguments: c.arguments.clone(),
        })
        .collect();
    sessions.append(
        session_id,
        &ChatMessage::assistant_tools("", calls),
        Trust::System,
        now,
    )?;
    for spec in &unit.calls {
        if let Some(r) = results.iter().find(|r| r.call_id == spec.id) {
            sessions.append(
                session_id,
                &ChatMessage::tool(&r.call_id, &r.content),
                Trust::parse(&r.trust).unwrap_or(Trust::System),
                now,
            )?;
        }
    }
    Ok(())
}
