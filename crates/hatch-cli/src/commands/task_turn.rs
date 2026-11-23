//! One bounded agent turn inside a durable step (P1, §9 task 3 /
//! §6.1 "有界 ReAct"): the unit bookkeeping a turn needs so a kill
//! mid-turn resumes from the recorded decision instead of
//! re-deciding — extracted from `task_gather.rs` (M0), shared by
//! the brief's gather turn and the generic ReAct runner.
//!
//! Shape: the caller builds the [`Agent`] (its own scripted model,
//! registry and prompt) and passes an `on_step_event` hook built
//! from [`record_turn_event`]; before starting it replays any unit
//! with pending calls ([`resume_pending_calls`] +
//! [`replay_unit_into_session`]) and finishes the unit from the
//! turn report ([`finish_turn_unit`]).

use hatch_agent::{SessionStore, StepEvent};
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_llm::{ChatMessage, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{ExecStatus, Lease, UnitCallSpec, UnitContext, UnitStore};
use hatch_tools::{dispatch_call, ExecutorRegistry, IntentJudge, IntentReview, ToolContext};
use std::cell::Cell;

/// Record one hook event into the unit store. On `Decision`, begin a
/// unit with the recorded batch — or reuse the step's latest unit
/// when it already carries the same call ids (a re-decision after
/// resume). On `Result`, record the observation. On `Answer`,
/// remember the text (the unit is finished by the caller, which owns
/// the turn's counts).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_turn_event(
    units: &UnitStore,
    task_id: &str,
    step_id: &str,
    lease: &Lease,
    current_unit: &Cell<Option<i64>>,
    answer: &Cell<Option<String>>,
    ev: StepEvent,
    now: &Timestamp,
    kill_after_decision: bool,
) {
    match ev {
        StepEvent::Decision {
            calls,
            turn_id,
            intent_digest,
            tainted,
            ..
        } => {
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
                .latest_unit(task_id, step_id)
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
                    let ctx = UnitContext {
                        turn_id,
                        intent_digest,
                        batch_taint: tainted,
                    };
                    if let Ok(unit) = units.begin_unit(task_id, lease, step_id, &specs, &ctx, now) {
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

/// Re-dispatch the pending calls of a recorded unit directly (no
/// model call): the batch is already durable, only the observations
/// are missing. The calls run through the turn's own registry — a
/// web_fetch pending from a killed ReAct turn must re-dispatch
/// through web_fetch, not fail as an unknown tool. Returns the
/// number of calls re-dispatched.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_pending_calls(
    db: &Db,
    task_id: &str,
    tools: &hatch_tools::Registry,
    pending: &[UnitCallSpec],
    lease: &Lease,
    unit_seq: i64,
    now: &Timestamp,
    tz: &chrono_tz::Tz,
    user_email: &str,
    user_name: &str,
    judge: Option<&dyn IntentJudge>,
    intent: &str,
) -> Result<usize> {
    let units = UnitStore::new(db);
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let cancel = CancelToken::new();
    // Rebuild the original decision's trust state: the batch-start
    // taint persisted with the unit, merged in call order with the
    // recorded results. Deriving taint from results alone loses it
    // when the worker died after the batch but before any result.
    let unit = units.get_unit(task_id, unit_seq)?;
    let context_missing = unit.as_ref().map(|u| u.turn_id.is_empty()).unwrap_or(true);
    let turn_id = unit
        .as_ref()
        .map(|u| u.turn_id.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("task:{task_id}:{unit_seq}"));
    let mut tainted = unit.as_ref().map(|u| u.batch_taint).unwrap_or(false)
        || units
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
            user_email: user_email.to_string(),
            user_name: user_name.to_string(),
            origin: format!("task:{unit_seq}"),
            tainted,
            call_id: call.id.clone(),
            cancel: cancel.clone(),
            intent_review: judge.map(|j| IntentReview {
                judge: j,
                intent: intent.to_string(),
                turn_id: turn_id.clone(),
                context_missing,
            }),
        };
        let out = dispatch_call(tools, &exec, &gate, &call, &ctx);
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
pub(crate) fn replay_unit_into_session(
    db: &Db,
    sessions: &SessionStore,
    session_id: &str,
    task_id: &str,
    step_id: &str,
    unit_seq: i64,
    now: &Timestamp,
) -> Result<()> {
    let units = UnitStore::new(db);
    let Some(unit) = units.latest_unit(task_id, step_id)? else {
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

/// Finish the turn's unit from the turn report: the unit's model
/// calls plus the report's, and the tool-call count as the larger of
/// the recorded batch and the report's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_turn_unit(
    db: &Db,
    task_id: &str,
    step_id: &str,
    lease: &Lease,
    unit_seq: i64,
    model_calls: u64,
    tool_calls: u64,
    now: &Timestamp,
) -> Result<()> {
    let units = UnitStore::new(db);
    let (mc, tc) = match units.latest_unit(task_id, step_id)? {
        Some(u) if u.unit_seq == unit_seq => (
            u.model_calls + model_calls,
            (u.calls.len() as u64).max(tool_calls),
        ),
        _ => (model_calls, tool_calls),
    };
    let _ = units.finish_unit(task_id, lease, unit_seq, ExecStatus::Done, mc, tc, now);
    Ok(())
}

/// Whether a caught panic is the drill's kill point.
pub(crate) fn is_drill_panic(p: &Box<dyn std::any::Any + Send>) -> bool {
    p.downcast_ref::<&str>()
        .is_some_and(|s| s.contains("drill: terminate"))
        || p.downcast_ref::<String>()
            .is_some_and(|s| s.contains("drill: terminate"))
}

/// The error a drill kill point returns (so the worker dies exactly
/// where the drill asked it to).
pub(crate) fn drill_kill_error() -> Error {
    Error::Invalid("drill: worker terminated after decision".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_tasks::{TaskStore, DEFAULT_LEASE_TTL_SECS};
    use hatch_tools::intent::{IntentJudgeInput, IntentJudgement, IntentVerdict};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // The resumed batch calls propose_event (deferred WriteCalendar)
    // through the default registry: under the resume path's default
    // policy it queues, which is a dispatch L0 judges.

    struct CaptureJudge {
        seen_tainted: Arc<Mutex<Vec<bool>>>,
        calls: Arc<AtomicUsize>,
    }

    impl IntentJudge for CaptureJudge {
        fn judge(
            &self,
            input: &IntentJudgeInput,
            _cancel: &CancelToken,
        ) -> Result<IntentJudgement> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_tainted.lock().unwrap().push(input.tainted);
            Ok(IntentJudgement {
                verdict: IntentVerdict::Allow,
                reason: "stub".into(),
                failure: None,
            })
        }
        fn version(&self) -> &str {
            "stub-v1"
        }
    }

    fn setup(unit_ctx: &UnitContext) -> (Db, String, Lease, i64, Vec<UnitCallSpec>) {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let now = hatch_core::time::now();
        let task = store
            .create("worker", "简报", "daemon", "", 3, None, &now)
            .unwrap();
        let claimed = store
            .start(&task.id, "worker", DEFAULT_LEASE_TTL_SECS, &now)
            .unwrap();
        let lease = claimed.lease().unwrap();
        let specs = vec![UnitCallSpec {
            id: "c1".into(),
            tool: "propose_event".into(),
            arguments: json!({"title": "周会", "start": "2026-11-24 10:00"}),
        }];
        let unit = UnitStore::new(&db)
            .begin_unit(&task.id, &lease, "gather", &specs, unit_ctx, &now)
            .unwrap();
        (db, task.id, lease, unit.unit_seq, specs)
    }

    fn resume(
        db: &Db,
        task_id: &str,
        lease: &Lease,
        seq: i64,
        specs: &[UnitCallSpec],
        judge: &CaptureJudge,
    ) {
        let tools = hatch_tools::builtin::default_registry();
        let now = hatch_core::time::now();
        resume_pending_calls(
            db,
            task_id,
            &tools,
            specs,
            lease,
            seq,
            &now,
            &chrono_tz::Asia::Shanghai,
            "me@corp.com",
            "Me",
            Some(judge),
            "建个周会",
        )
        .unwrap();
    }

    fn verdict_rows(db: &Db) -> Vec<(String, i64, i64, i64)> {
        let conn = db.conn();
        let mut st = conn
            .prepare("SELECT turn_id, tainted, cache_hit, context_missing FROM intent_verdicts ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<hatch_store::rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// The v2.1 crash case: untrusted content was read earlier in
    /// the turn, the decision batch was persisted, and the worker
    /// died before any call of the batch ran. Resume must keep the
    /// original taint — deriving it from recorded results alone
    /// would read `tainted=false` here.
    #[test]
    fn resume_keeps_batch_start_taint_without_results() {
        let ctx = UnitContext {
            turn_id: "turn-1".into(),
            intent_digest: "d".into(),
            batch_taint: true,
        };
        let (db, task_id, lease, seq, specs) = setup(&ctx);
        let judge = CaptureJudge {
            seen_tainted: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        assert_eq!(*judge.seen_tainted.lock().unwrap(), vec![true]);
        let rows = verdict_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "turn-1");
        assert_eq!(rows[0].1, 1, "recorded tainted");
        assert_eq!(rows[0].3, 0, "context present");
        // Second replay with the queued approval removed (as if the
        // first process died between judging and submitting): the
        // verdict is reused from the per-turn cache.
        db.conn().execute("DELETE FROM approvals", []).unwrap();
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
        let rows = verdict_rows(&db);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].2, 1, "cache_hit");
    }

    /// Pre-v24 units carry no turn context: marked missing, and
    /// verdicts are never reused for them.
    #[test]
    fn resume_without_context_never_reuses_verdict() {
        let (db, task_id, lease, seq, specs) = setup(&UnitContext::default());
        let judge = CaptureJudge {
            seen_tainted: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        db.conn().execute("DELETE FROM approvals", []).unwrap();
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            2,
            "no reuse without context"
        );
        let rows = verdict_rows(&db);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.3 == 1), "context_missing: {rows:?}");
        assert!(rows.iter().all(|r| r.2 == 0), "no cache hits: {rows:?}");
    }
}
