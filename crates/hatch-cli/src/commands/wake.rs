//! Wake turns: when a sub-agent handoff lands and its session is
//! idle, run one system turn so the result is processed — and
//! reported when worth it — without waiting for the user's next
//! message. The contract (steering plan §6.1): one queue per
//! session (the handoff table), arrival order, no preemption of an
//! in-flight turn, exactly-once delivery (the turn's own
//! `collect_handoffs`), and the agent decides whether to report —
//! replying with the silent sentinel ends the turn with nothing
//! surfaced. Wake turns are audited separately (`wake_turn`) and
//! capped per hour; over the cap, handoffs stay queued for the next
//! user turn.
//!
//! Two triggers, both in-process owners of their sessions:
//! * the chat REPL, while it sits at the prompt (and once at
//!   startup, for handoffs that arrived while it was closed);
//! * the product API server (inside `daemon run --api`), claiming
//!   the session in its running map so a user turn and a wake turn
//!   can never overlap there.
//!
//! A daemon without the API wakes nothing: no process owns chat
//! sessions there. Cross-process races (REPL vs API on one session)
//! predate this feature and are unchanged.

use super::chat::setup_turn;
use crate::api::ApiCtx;
use crate::app::App;
use crate::config::AgentSection;
use anyhow::Result;
use hatch_agent::subagent::RunStore;
use hatch_agent::{Agent, AgentConfig, RunOptions};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::CancelToken;
use hatch_llm::Router;
use hatch_store::{params, Db, DbResultExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The system turn's input. The handoffs themselves ride into the
/// turn's context through the normal exactly-once collection; this
/// text only tells the model why it was woken and how to stay
/// silent.
pub const WAKE_TEXT: &str = "【系统交接】你委派的子代理已有结果返回，交接内容在本轮上下文中。请处理：若有值得向用户汇报的结果、失败或需要跟进的事项，简要汇报（不要复述全部过程）；若没有，只回复「无事可报」四个字，不要调用工具，不要做其他动作。";

/// The exact reply that means "nothing to report".
pub const SILENT: &str = "无事可报";

/// Audit event for wake turns (separate billing trail).
pub const WAKE_EVENT: &str = "wake_turn";

/// How often the API wake thread scans for idle sessions with
/// waiting handoffs.
const API_SCAN_SECS: u64 = 2;

pub struct WakeOutcome {
    /// The model chose silence (or produced no answer).
    pub silent: bool,
    /// The report text, when not silent.
    pub answer: Option<String>,
}

/// Wake turns recorded in the last hour (all sessions combined).
pub fn wake_count_last_hour(db: &Db, now: &Timestamp) -> usize {
    let cutoff = to_rfc3339(&(*now - chrono::Duration::hours(1)));
    db.transaction(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM audit WHERE event = ?1 AND ts >= ?2",
            params![WAKE_EVENT, cutoff],
            |r| r.get::<_, i64>(0),
        )
        .db()
    })
    .map(|n| n.max(0) as usize)
    .unwrap_or(0)
}

/// Whether `session` should be woken now: feature on, a handoff
/// waiting, and the hourly cap not reached. A DB error means "no" —
/// never wake on a guess.
pub fn should_wake(db: &Db, session: &str, cfg: &AgentSection, now: &Timestamp) -> bool {
    if !cfg.wake_on_handoff {
        return false;
    }
    let parent = format!("session:{session}");
    if !RunStore::new(db).has_pending_handoffs(&parent).unwrap_or(false) {
        return false;
    }
    wake_count_last_hour(db, now) < cfg.wake_max_per_hour as usize
}

/// Run one wake turn for `session`. The caller owns the "session is
/// idle" claim (REPL prompt, API running map); this function only
/// runs the turn and audits it. `router` is caller-built so tests
/// can inject a scripted model.
pub fn run_wake_turn(
    app: &App,
    session: &str,
    router: &Router,
    cancel: CancelToken,
    workspace: Option<&Path>,
) -> Result<WakeOutcome> {
    let setup = setup_turn(app, workspace)?;
    let agent = Agent {
        db: &app.db,
        router,
        tools: &setup.tools,
        executors: &setup.executors,
        gate: &setup.gate,
        config: AgentConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            max_tokens: app.config.limits.max_tokens,
            max_steps: app.config.agent.max_steps,
            salience_half_life_days: app.config.agent.salience_half_life_days,
            total_budget: app.config.agent.total_budget,
            window_ratio: app.config.agent.window_ratio,
            context_window: router.context_window(hatch_llm::Workload::Main),
            delegation: setup.delegation,
            background: Vec::new(),
            skills: setup.skill_bodies,
            skill_tools: setup.skill_tools,
            origin: Some(format!("wake:{session}")),
            completeness_check: app.config.agent.completeness_check,
            todo_tools: app.config.agent.todo_tools,
            ..Default::default()
        },
    };
    let now = hatch_core::time::now();
    let result = agent.run(
        session,
        WAKE_TEXT,
        &now,
        RunOptions {
            on_delta: None,
            cancel,
            observer: None,
            on_step_event: None,
            images: Vec::new(),
        },
    );
    let _ = app.record_usage(router, &now);
    let report = result?;
    super::task_delegate_reconcile::reconcile_turn_subagents(&app.db, &report.subagents, &now);
    let answer = report.answer.clone().unwrap_or_default();
    let silent = answer.trim() == SILENT || answer.trim().is_empty();
    let _ = app.db.audit(
        &format!("wake:{session}"),
        WAKE_EVENT,
        &format!(
            "{session}: {} 次调用，{}+{} tokens，{}",
            report.model_calls,
            report.usage.prompt_tokens,
            report.usage.completion_tokens,
            if silent { "静默" } else { "已汇报" }
        ),
    );
    Ok(WakeOutcome {
        silent,
        answer: if silent { None } else { Some(answer) },
    })
}

/// The API server's wake thread: scan for sessions with waiting
/// handoffs, claim each idle one in the running map, and run its
/// wake turn. Errors are logged and the scan continues; the thread
/// exits when `shutdown` is set.
pub fn spawn_api_wake_thread(
    ctx: Arc<ApiCtx>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(API_SCAN_SECS));
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let sessions = RunStore::new(&ctx.app.db)
                .sessions_with_pending_handoffs()
                .unwrap_or_default();
            for session in sessions {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let now = hatch_core::time::now();
                if !should_wake(&ctx.app.db, &session, &ctx.app.config.agent, &now) {
                    continue;
                }
                let cancel = CancelToken::new();
                {
                    let mut running = ctx.running.lock().unwrap();
                    if running.contains_key(&session) {
                        continue;
                    }
                    running.insert(session.clone(), cancel.clone());
                }
                let outcome = (ctx.router_factory)()
                    .map_err(|e| anyhow::anyhow!("唤醒回合无可用模型：{}", e.message))
                    .and_then(|router| run_wake_turn(&ctx.app, &session, &router, cancel, None));
                ctx.running.lock().unwrap().remove(&session);
                if let Err(e) = outcome {
                    eprintln!("[wake] 会话 {session} 唤醒回合失败：{e:#}");
                }
            }
        }
    })
}
