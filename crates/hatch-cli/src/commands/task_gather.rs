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

use super::task_turn::{
    drill_kill_error, finish_turn_unit, is_drill_panic, record_turn_event,
    replay_unit_into_session, resume_pending_calls, task_turn_state, TaskTurnState,
};
use super::task_worker::KillPoint;
use chrono::TimeZone;
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore, StepEvent};
use hatch_core::time::Timestamp;
use hatch_core::Result;
use hatch_eval::meeting_brief::assemble_brief;
use hatch_eval::World;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatResponse, LlmError, Role, Router, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{Lease, UnitStore};
use hatch_tools::builtin::default_registry;
use hatch_tools::ExecutorRegistry;
use serde_json::json;
use std::cell::Cell;
use std::sync::Arc;

/// One Agent turn for the `gather` step. Returns
/// `(answer, model_calls, prompt_tokens, completion_tokens)`.
/// `live_router` (`--live`) runs the turn on the real model; `None`
/// keeps the scripted model, so CI makes zero real calls.
/// `world_now` is the fixture clock: the script model's calendar
/// window ("tomorrow's" meeting) is anchored to the world's own date,
/// not to the runtime clock.
/// `prompt` overrides the gather instruction (Gmail runs ask about
/// recent mail in general); `None` keeps the fixture instruction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_gather(
    db: &Arc<Db>,
    world: &World,
    task_id: &str,
    lease: &Lease,
    now: &Timestamp,
    world_now: &Timestamp,
    tz: &chrono_tz::Tz,
    kill: KillPoint,
    live_router: Option<&Router>,
    prompt: Option<&str>,
) -> Result<(String, u64, u64, u64)> {
    let db_ref: &Db = db;
    let units = UnitStore::new(db_ref);
    let sessions = SessionStore::new(db_ref);
    let TaskTurnState {
        session,
        origin,
        resume,
        continuation,
    } = task_turn_state(db_ref, task_id, now)?;
    let current_unit: Cell<Option<i64>> = Cell::new(None);
    let answer: Cell<Option<String>> = Cell::new(None);
    // L0 intent gate (Observe): scripted allow-judge (this turn is
    // scripted-only); off unless HATCH_INTENT_GATE says observe.
    let intent_mode = hatch_tools::IntentGateMode::configured().unwrap_or_default();
    let judge_router = Router::single(std::sync::Arc::new(FnModel::new("intent-script", |_| {
        Ok(ChatResponse::text(
            "{\"verdict\":\"allow\",\"reason\":\"scripted\",\"cited_intent\":\"\",\"cited_field\":\"\"}",
        ))
    })));
    let intent_judge = (intent_mode != hatch_tools::IntentGateMode::Off)
        .then(|| hatch_agent::intent_judge::RouterIntentJudge::new(&judge_router));
    // Resume: a unit with pending calls is re-dispatched directly (the
    // model batch is already recorded), then replayed into the session
    // so the turn only has to answer. A unit that is already complete
    // still becomes the current unit, so its counts are kept.
    if let Some((pos, seq)) = resume
        .as_ref()
        .and_then(|pos| pos.unit_seq.map(|seq| (pos, seq)))
    {
        current_unit.set(Some(seq));
        if !pos.pending_calls.is_empty() {
            // The recorded batch re-dispatches through the
            // turn's own registry (see task_turn).
            let tools = default_registry();
            resume_pending_calls(
                db_ref,
                task_id,
                &tools,
                &pos.pending_calls,
                lease,
                seq,
                now,
                tz,
                &world.persona.email,
                &world.persona.name,
                intent_judge
                    .as_ref()
                    .map(|j| j as &dyn hatch_tools::IntentJudge),
                prompt.unwrap_or_default(),
                intent_mode,
                // The gather turn has no broker: its pending
                // calls are calendar/mail reads, and the
                // resume registry still carries web_fetch.
                None,
                &origin,
            )?;
        }
        replay_unit_into_session(db_ref, &sessions, &session, task_id, "gather", seq, now)?;
    }
    let db_arc = Arc::clone(db);
    let world_arc = Arc::new(world.clone());
    let now_copy = *world_now;
    let tz_copy = *tz;
    // Desensitize: the mapping is built once from this task's mailbox
    // (the same canonical set the triage workflow and the artifact
    // step rebuild), the agent is addressed as its code identity, and
    // the scripted brief is forwarded through the mapping so the
    // recorded gather answer is in code space, exactly as a real
    // model's answer would be. The user email stays real here: it is
    // only used for tool contexts (the mail tool needs the real owner
    // address to build the same mapping) and never enters the prompt.
    let mapper = Arc::new(hatch_triage::desensitize::mapper_for(
        db_ref,
        &world.persona.email,
        &world.persona.name,
    )?);
    // M1: `--live` runs the gather turn on the real model; the default
    // stays the scripted model (CI makes zero real calls).
    let script_router;
    let router: &Router = match live_router {
        Some(r) => r,
        None => {
            let mapper = Arc::clone(&mapper);
            let model = FnModel::new("brief-script", move |req| {
                let seen_tool = req.messages.iter().any(|m| m.role == Role::Tool);
                if seen_tool {
                    let brief = assemble_brief(&db_arc, &world_arc)
                        .map_err(|e| LlmError::permanent(e.to_string()))?;
                    Ok(ChatResponse::text(mapper.forward(&brief.text)))
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
            script_router = Router::single(Arc::new(model));
            &script_router
        }
    };
    let tools = default_registry();
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let agent = Agent {
        db: db_ref,
        router,
        tools: &tools,
        executors: &exec,
        gate: &gate,
        config: AgentConfig {
            user_name: mapper.owner_code_name().to_string(),
            user_email: world.persona.email.clone(),
            tz: *tz,
            // The scripted model always finishes in exactly 2 turns
            // (read batch, then answer), but a live model on real mail
            // may spend a turn on a dead-end query before it reads and
            // answers — 2 steps left it with no step to write the
            // brief (M2 pilot). 6 still bounds the turn; the task
            // budget caps total calls.
            max_steps: 6,
            origin: Some(origin.clone()),
            ..Default::default()
        },
    };
    let kill_after_decision = matches!(kill, KillPoint::AfterDecision);
    let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        agent.run_with_continuation(
            &session,
            prompt.unwrap_or("明天开会前，把李婷最近邮件里的决定整理成简报"),
            now,
            RunOptions {
                on_step_event: Some(&mut |ev: StepEvent| {
                    record_turn_event(
                        &units,
                        task_id,
                        "gather",
                        lease,
                        &current_unit,
                        &answer,
                        ev,
                        &origin,
                        now,
                        kill_after_decision,
                    )
                }),
                ..Default::default()
            },
            continuation.as_ref(),
        )
    }));
    let report = match (kill_after_decision, turn) {
        (true, Err(p)) if is_drill_panic(&p) => {
            return Err(drill_kill_error());
        }
        (_, Err(p)) => std::panic::resume_unwind(p),
        (_, Ok(r)) => r?,
    };
    let answer = super::task_react_turn::turn_conclusion(
        &report,
        answer.take(),
        "gather 回合被验证阻塞，未产出可信简报",
        "gather 回合没有产出简报",
    )?;
    // The answer is in code space (the model only ever saw codes,
    // and the scripted brief is forwarded the same way); map it back
    // before it is stored as the step output, so the plan card shows
    // real names. The artifact step reverses again — a no-op on real
    // text — so the written brief is unchanged.
    let answer = mapper.reverse(&answer);
    if let Some(seq) = current_unit.get() {
        finish_turn_unit(
            db_ref,
            task_id,
            "gather",
            lease,
            seq,
            report.model_calls,
            report.tool_calls.len() as u64,
            now,
        )?;
    }
    Ok((
        answer,
        report.model_calls,
        report.usage.prompt_tokens,
        report.usage.completion_tokens,
    ))
}
