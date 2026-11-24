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
use hatch_browser::Broker;
use hatch_core::time::Timestamp;
use hatch_core::{CancelToken, Error, Result, Trust};
use hatch_llm::{ChatMessage, ToolCall};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tasks::{ExecStatus, Lease, UnitCallSpec, UnitContext, UnitStore};
use hatch_tools::{
    dispatch_call, BrowserClickExecutor, BrowserOpenExecutor, ExecutorRegistry, FetchUrlExecutor,
    IntentJudge, IntentReview, ToolContext,
};
use std::cell::Cell;
use std::sync::Arc;

/// The executors a ReAct turn dispatches through: `web_fetch` always,
/// plus the browser open/click executors whenever the turn has a
/// broker. One constructor shared by the normal path and the resume
/// path, so they can never drift apart again — the resume path used
/// to build an empty registry, and a pending browser call died as
/// "no executor" instead of running.
pub(crate) fn react_executors(broker: Option<Arc<Broker>>) -> ExecutorRegistry {
    let mut exec = ExecutorRegistry::new().with(Arc::new(FetchUrlExecutor {
        limits: hatch_tools::web::FetchLimits::default(),
    }));
    if let Some(b) = broker {
        exec = exec
            .with(Arc::new(BrowserOpenExecutor { broker: b.clone() }))
            .with(Arc::new(BrowserClickExecutor { broker: b }));
    }
    exec
}

/// Record one hook event into the unit store. On `Decision`, begin a
/// unit with the recorded batch — or reuse the step's latest unit
/// when it is the SAME batch re-decided (same turn, same in-turn
/// step, same call ids), e.g. after clarifications arrived. The
/// batch identity is the turn plus the step: call ids alone cannot
/// identify a batch, because a provider that synthesises ids per
/// response (Gemini's `gemini_call_0`) reuses the same id on every
/// step. A different batch never reuses, even with identical ids.
/// On `Result`, record the observation. On `Answer`, remember the
/// text (the unit is finished by the caller, which owns the turn's
/// counts).
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
            step,
            calls,
            turn_id,
            intent_digest,
            intent_text,
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
            // Reuse only for the same batch: same turn, same
            // in-turn step, same call ids. A new step with the
            // same ids is a new batch (a provider that
            // synthesises ids per response reuses them across
            // steps); reusing it would overwrite the old batch's
            // intent while keeping its calls, so resume would
            // re-dispatch the wrong action and drop the new
            // batch's results as duplicates. Pre-fix units carry
            // batch_step -1 and never match a real step.
            let reuse = units
                .latest_unit(task_id, step_id)
                .ok()
                .flatten()
                .and_then(|u| {
                    let same: Vec<&str> = u.calls.iter().map(|c| c.id.as_str()).collect();
                    if !turn_id.is_empty()
                        && u.turn_id == turn_id
                        && u.batch_step == step as i64
                        && same == ids
                    {
                        Some(u.unit_seq)
                    } else {
                        None
                    }
                });
            match reuse {
                Some(seq) => {
                    current_unit.set(Some(seq));
                    // Same batch, possibly a newer intent: the turn
                    // may have gained clarifications since the first
                    // decision, so the stored text and digest move
                    // with the latest decision.
                    let _ = units.update_intent(
                        task_id,
                        lease,
                        seq,
                        &intent_text,
                        &intent_digest,
                        now,
                    );
                }
                None => {
                    let ctx = UnitContext {
                        turn_id,
                        intent_digest,
                        intent_text,
                        batch_taint: tainted,
                        batch_step: step as i64,
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
/// through web_fetch, not fail as an unknown tool. `broker` rebuilds
/// the turn's executors (see [`react_executors`]): the resume path
/// gets exactly the executors the normal path had, including the
/// browser executors when the original turn had a broker. Returns
/// the number of calls re-dispatched.
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
    mode: hatch_tools::IntentGateMode,
    broker: Option<Arc<Broker>>,
) -> Result<usize> {
    let units = UnitStore::new(db);
    let exec = react_executors(broker);
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
    // Restore the intent the turn decided with: the recorded
    // composed text (user message plus `ask_user` clarifications),
    // checked against its recorded digest. A mismatch means the
    // stored record is corrupt or from another intent — refuse
    // rather than silently judging against the raw prompt, which
    // could allow what the user never authorised. Units recorded
    // before the text was stored carry none and keep judging
    // against the raw prompt, as before.
    let recorded_text = unit.as_ref().map(|u| u.intent_text.clone()).unwrap_or_default();
    let recorded_digest = unit.as_ref().map(|u| u.intent_digest.clone()).unwrap_or_default();
    let intent = if recorded_text.is_empty() {
        intent.to_string()
    } else {
        let digest = hatch_tools::intent::intent_digest(&recorded_text);
        if !recorded_digest.is_empty() && digest != recorded_digest {
            return Err(Error::Invalid(format!(
                "recorded intent for unit {unit_seq} fails its digest check; refusing to judge the resumed batch"
            )));
        }
        recorded_text
    };
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
            turn_id: turn_id.clone(),
            tainted,
            call_id: call.id.clone(),
            cancel: cancel.clone(),
            intent_review: judge.map(|j| IntentReview {
                judge: j,
                intent: intent.to_string(),
                turn_id: turn_id.clone(),
                context_missing,
                mode,
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
        seen_intent: Arc<Mutex<Vec<String>>>,
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
            self.seen_intent.lock().unwrap().push(input.intent.clone());
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

    fn capture_judge() -> CaptureJudge {
        CaptureJudge {
            seen_tainted: Arc::new(Mutex::new(Vec::new())),
            seen_intent: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The composed intent a turn decided with: the raw prompt plus
    /// the user's answer to the question asked in that turn.
    fn composed_intent() -> String {
        let base = "把其中一个评审会改到周五";
        let clarifications = vec![hatch_tools::intent::IntentClarification {
            question_id: "apr_1".into(),
            question: "改到周五几点？".into(),
            answer: "15:00".into(),
        }];
        hatch_tools::intent::compose_intent(base, &clarifications)
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
        resume_with_prompt(db, task_id, lease, seq, specs, judge, "建个周会").unwrap();
    }

    fn resume_with_prompt(
        db: &Db,
        task_id: &str,
        lease: &Lease,
        seq: i64,
        specs: &[UnitCallSpec],
        judge: &CaptureJudge,
        prompt: &str,
    ) -> Result<usize> {
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
            prompt,
            hatch_tools::IntentGateMode::Observe,
            None,
        )
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
            intent_text: String::new(),
            batch_taint: true,
            batch_step: 0,
        };
        let (db, task_id, lease, seq, specs) = setup(&ctx);
        let judge = capture_judge();
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
        let judge = capture_judge();
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

    /// RED (review round 2, P2): the user answered, then the worker
    /// died after the decision was recorded and before any call ran.
    /// Resume must judge the batch against the recorded composed
    /// intent — prompt plus the answer — not the raw prompt the
    /// caller passes as a fallback.
    #[test]
    fn resume_uses_recorded_intent_with_clarifications() {
        let composed = composed_intent();
        let ctx = UnitContext {
            turn_id: "turn-1".into(),
            intent_digest: hatch_tools::intent::intent_digest(&composed),
            intent_text: composed.clone(),
            batch_taint: false,
            batch_step: 0,
        };
        let (db, task_id, lease, seq, specs) = setup(&ctx);
        let judge = capture_judge();
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        assert_eq!(*judge.seen_intent.lock().unwrap(), vec![composed]);
    }

    /// A recorded intent whose digest does not match is refused, not
    /// silently replaced by the raw prompt: a mismatch means the
    /// stored record is corrupt or from another intent, and judging
    /// the resumed batch against the bare prompt could allow what
    /// the user never authorised.
    #[test]
    fn resume_refuses_intent_digest_mismatch() {
        let composed = composed_intent();
        let ctx = UnitContext {
            turn_id: "turn-1".into(),
            intent_digest: "bogus".into(),
            intent_text: composed,
            batch_taint: false,
            batch_step: 0,
        };
        let (db, task_id, lease, seq, specs) = setup(&ctx);
        let judge = capture_judge();
        let err = resume_with_prompt(&db, &task_id, &lease, seq, &specs, &judge, "建个周会")
            .expect_err("digest mismatch must refuse the resume");
        assert!(err.to_string().contains("digest"), "{err}");
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0, "nothing judged");
    }

    /// Pre-fix units carry no recorded intent: resume keeps judging
    /// against the raw prompt, as before.
    #[test]
    fn resume_without_recorded_intent_uses_raw_prompt() {
        let (db, task_id, lease, seq, specs) = setup(&UnitContext::default());
        let judge = capture_judge();
        resume(&db, &task_id, &lease, seq, &specs, &judge);
        assert_eq!(
            *judge.seen_intent.lock().unwrap(),
            vec!["建个周会".to_string()]
        );
    }

    /// The shared constructor: fetch always, browser executors only
    /// with a broker — the same registry the normal path runs with.
    /// Broker construction launches nothing, so this needs no
    /// Chromium.
    #[test]
    fn react_executors_register_fetch_and_browser_with_broker() {
        let without = react_executors(None);
        assert!(without.get("fetch_url").is_some());
        assert!(without.get("browser_open").is_none());
        assert!(without.get("browser_click").is_none());

        let dir = tempfile::tempdir().unwrap();
        let broker = Arc::new(hatch_browser::Broker::new(
            hatch_browser::BrowserConfig::new(dir.path()),
        ));
        let with = react_executors(Some(broker));
        assert!(with.get("fetch_url").is_some());
        assert!(with.get("browser_open").is_some());
        assert!(with.get("browser_click").is_some());
    }

    /// A browser call recorded before a crash must actually run on
    /// resume — through the browser executors, not die as
    /// "no executor". Skipped without Chromium, like the other
    /// browser tests.
    #[test]
    fn resume_redispatches_a_pending_browser_call() {
        use hatch_browser::testkit::MockSite;
        use hatch_browser::{find_chromium, Broker, BrowserConfig};
        if find_chromium(None).is_none() {
            eprintln!("SKIP: no Chromium found (set HATCH_CHROMIUM to run the browser tests)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = BrowserConfig::new(dir.path());
        cfg.allow_loopback = true;
        cfg.ca_bundle = None;
        let broker = Arc::new(Broker::new(cfg));
        let site = MockSite::start().unwrap();

        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let now = hatch_core::time::now();
        let task = store
            .create("worker", "调研", "daemon", "", 3, None, &now)
            .unwrap();
        let claimed = store
            .start(&task.id, "worker", DEFAULT_LEASE_TTL_SECS, &now)
            .unwrap();
        let lease = claimed.lease().unwrap();
        let specs = vec![UnitCallSpec {
            id: "c-web".into(),
            tool: "browser_open".into(),
            arguments: json!({"url": site.url("/article")}),
        }];
        let unit = UnitStore::new(&db)
            .begin_unit(
                &task.id,
                &lease,
                "gather",
                &specs,
                &UnitContext {
                    turn_id: "turn-1".into(),
                    intent_digest: "d".into(),
                    intent_text: String::new(),
                    batch_taint: false,
                    batch_step: 0,
                },
                &now,
            )
            .unwrap();
        // The normal path's registry: default + web_fetch + browser
        // tools backed by the same broker.
        let tools = super::super::task_react_turn::react_tools(Some(broker.clone()));
        resume_pending_calls(
            &db,
            &task.id,
            &tools,
            &specs,
            &lease,
            unit.unit_seq,
            &now,
            &chrono_tz::Asia::Shanghai,
            "me@corp.com",
            "Me",
            None,
            "打开文章看看",
            hatch_tools::IntentGateMode::Off,
            Some(broker.clone()),
        )
        .unwrap();
        let results = UnitStore::new(&db)
            .unit_results(&task.id, unit.unit_seq)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].content.contains("青鸟"),
            "resumed browser_open must return the page, not an executor error: {}",
            results[0].content
        );
    }

    /// A task + lease with no unit yet, for driving
    /// `record_turn_event` directly.
    fn setup_task() -> (Db, String, Lease) {
        let db = Db::open_in_memory().unwrap();
        let store = TaskStore::new(&db);
        let now = hatch_core::time::now();
        let task = store
            .create("worker", "简报", "daemon", "", 3, None, &now)
            .unwrap();
        let claimed = store
            .start(&task.id, "worker", DEFAULT_LEASE_TTL_SECS, &now)
            .unwrap();
        (db, task.id, claimed.lease().unwrap())
    }

    fn decision_event(step: usize, turn_id: &str, intent_text: &str, calls: Vec<ToolCall>) -> StepEvent {
        StepEvent::Decision {
            step,
            calls,
            turn_id: turn_id.into(),
            intent_digest: hatch_tools::intent::intent_digest(intent_text),
            intent_text: intent_text.into(),
            tainted: false,
        }
    }

    /// Drive one hook event through `record_turn_event` with the
    /// same `current_unit` cell across calls, as the real turn hook
    /// does — so a `Result` lands on the unit its `Decision` began.
    fn record_event(
        db: &Db,
        task_id: &str,
        lease: &Lease,
        current_unit: &Cell<Option<i64>>,
        answer: &Cell<Option<String>>,
        ev: StepEvent,
        now: &hatch_core::time::Timestamp,
    ) {
        record_turn_event(
            &UnitStore::new(db),
            task_id,
            "gather",
            lease,
            current_unit,
            answer,
            ev,
            now,
            false,
        );
    }

    fn ask_call() -> ToolCall {
        ToolCall {
            id: "gemini_call_0".into(),
            name: "ask_user".into(),
            arguments: json!({"question": "改到周五几点？", "options": [{"label": "15:00"}]}),
        }
    }

    fn propose_call() -> ToolCall {
        ToolCall {
            id: "gemini_call_0".into(),
            name: "propose_event".into(),
            arguments: json!({"title": "评审会", "start": "2026-11-27 15:00"}),
        }
    }

    /// RED (review round 3, P1): two consecutive steps whose first
    /// call shares the model call id (Gemini synthesises
    /// `gemini_call_0` per response when the response carries no
    /// native id) must NOT reuse one unit. The second decision is
    /// a new batch even though the id list is identical — reusing
    /// would leave the question batch in `calls_json` while the
    /// intent was already overwritten with the calendar step's.
    #[test]
    fn same_call_id_across_steps_starts_a_new_unit() {
        let (db, task_id, lease) = setup_task();
        let now = hatch_core::time::now();
        let current = Cell::new(None);
        let answer = Cell::new(None);
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(0, "turn-1", "把其中一个评审会改到周五", vec![ask_call()]),
            &now,
        );
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(1, "turn-1", &composed_intent(), vec![propose_call()]),
            &now,
        );
        let units = UnitStore::new(&db);
        let first = units.get_unit(&task_id, 1).unwrap().expect("step-0 unit");
        assert_eq!(
            first.calls[0].tool, "ask_user",
            "step 0 keeps its question batch"
        );
        let second = units
            .get_unit(&task_id, 2)
            .unwrap()
            .expect("step 1 must be a new batch, not a reuse of the question unit");
        assert_eq!(
            second.calls[0].tool, "propose_event",
            "step 1 must record the calendar batch: {second:?}"
        );
        assert_eq!(second.calls[0].id, "gemini_call_0");
    }

    /// Same batch re-decided (same turn, same step, same call ids)
    /// still reuses the unit and refreshes the stored intent: a
    /// turn that gains clarifications re-decides the same batch
    /// instead of starting a new one.
    #[test]
    fn same_batch_redecision_reuses_unit_and_refreshes_intent() {
        let (db, task_id, lease) = setup_task();
        let now = hatch_core::time::now();
        let current = Cell::new(None);
        let answer = Cell::new(None);
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(0, "turn-1", "把其中一个评审会改到周五", vec![ask_call()]),
            &now,
        );
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(0, "turn-1", &composed_intent(), vec![ask_call()]),
            &now,
        );
        let units = UnitStore::new(&db);
        let latest = units.latest_unit(&task_id, "gather").unwrap().unwrap();
        assert_eq!(latest.unit_seq, 1, "same batch reuses its unit");
        assert_eq!(
            latest.intent_text,
            composed_intent(),
            "stored intent moves with the latest decision"
        );
        assert!(
            units.get_unit(&task_id, 2).unwrap().is_none(),
            "no second unit for a re-decision"
        );
    }

    /// RED (review round 3, P1 — the user-specified core path):
    /// `ask_user` answered, the next step proposes the calendar
    /// action with the SAME model call id, and the worker dies
    /// before dispatch. Resume must re-dispatch the calendar
    /// action (not the question), and L0 must judge it against
    /// this turn's answer.
    #[test]
    fn resume_after_ask_user_answer_dispatches_calendar_action() {
        let (db, task_id, lease) = setup_task();
        let now = hatch_core::time::now();
        let current = Cell::new(None);
        let answer = Cell::new(None);
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(0, "turn-1", "把其中一个评审会改到周五", vec![ask_call()]),
            &now,
        );
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            StepEvent::Result {
                step: 0,
                call_id: "gemini_call_0".into(),
                tool: "ask_user".into(),
                content: "{\"answered\": true, \"choice\": \"15:00\"}".into(),
                trust: Trust::System,
            },
            &now,
        );
        record_event(
            &db,
            &task_id,
            &lease,
            &current,
            &answer,
            decision_event(1, "turn-1", &composed_intent(), vec![propose_call()]),
            &now,
        );
        // Crash before dispatch: nothing ran for the step-1 unit yet.
        let units = UnitStore::new(&db);
        let unit2 = units
            .get_unit(&task_id, 2)
            .unwrap()
            .expect("step-1 unit must exist as its own batch");
        assert_eq!(unit2.calls[0].tool, "propose_event");
        assert!(
            units.unit_results(&task_id, 2).unwrap().is_empty(),
            "nothing dispatched before the crash"
        );
        let judge = capture_judge();
        let specs = unit2.calls.clone();
        resume_pending_calls(
            &db,
            &task_id,
            &hatch_tools::builtin::default_registry(),
            &specs,
            &lease,
            2,
            &now,
            &chrono_tz::Asia::Shanghai,
            "me@corp.com",
            "Me",
            Some(&judge),
            "把其中一个评审会改到周五",
            hatch_tools::IntentGateMode::Observe,
            None,
        )
        .unwrap();
        assert_eq!(
            *judge.seen_intent.lock().unwrap(),
            vec![composed_intent()],
            "L0 judges the resumed batch against this turn's answer"
        );
        let results = units.unit_results(&task_id, 2).unwrap();
        assert_eq!(results.len(), 1, "the resumed batch records its own result");
        assert_eq!(
            results[0].tool, "propose_event",
            "resumed batch dispatches the calendar action, not the question"
        );
        assert_eq!(results[0].call_id, "gemini_call_0");
        let first_results = units.unit_results(&task_id, 1).unwrap();
        assert_eq!(first_results.len(), 1, "the question unit keeps its own result");
        assert_eq!(first_results[0].tool, "ask_user");
    }
}
