//! Agent configuration: per-agent settings and per-turn knobs.
//!
//! Split out of `agent.rs` so the turn-loop file stays under the
//! 400-line limit. Moved verbatim; behaviour is unchanged.

use crate::progress::Observer;
use crate::subagent::Delegation;
use chrono_tz::Tz;
use hatch_core::{CancelToken, Trust};
use hatch_llm::{StreamSink, ToolCall, Workload};

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub user_name: String,
    pub user_email: String,
    pub tz: Tz,
    pub max_steps: usize,
    /// §3.4: total context budget in (estimated) tokens — a quality /
    /// cost choice, not a window. Default [`crate::context_budget::DEFAULT_TOTAL_BUDGET`].
    pub total_budget: usize,
    /// §3.4: fraction of the declared context window that also caps
    /// the total (`effective = min(total_budget, window × ratio)`).
    /// Default [`crate::context_budget::DEFAULT_WINDOW_RATIO`].
    pub window_ratio: f64,
    /// §3.4: the running model's declared context window in tokens
    /// (static configuration, OMB #23). `None` = unknown: the absolute
    /// total is the only cap.
    pub context_window: Option<usize>,
    pub keep_tail_turns: usize,
    pub max_tokens: Option<u32>,
    /// Model workload for this agent's calls.
    pub workload: Workload,
    /// Extra system instructions appended to the behaviour spec (sub-agent role).
    pub system_note: Option<String>,
    /// Recorded on approvals; default `chat:<session>`.
    pub origin: Option<String>,
    /// Start in the tainted state (a sub-agent inherits its parent's).
    pub start_tainted: bool,
    /// Background updates shown in the context; their presence taints the turn.
    pub background: Vec<String>,
    /// Memory lines passed in by a parent process (already formatted like
    /// recalled claims). A sub-agent running in its own process has an
    /// empty local bank, so the parent hands it the relevant context.
    pub memory_context: Vec<String>,
    /// Loaded skill bodies, rendered as a stable context layer.
    pub skills: Vec<String>,
    /// When Some, the model may only call these tools (plus list_skills /
    /// load_skill, which are never filtered out). Set from the loaded
    /// skills' `tools` allowlists; None = no skill restriction.
    pub skill_tools: Option<Vec<String>>,
    /// Sub-agent settings; `None` = no `delegate` tools.
    pub delegation: Option<Delegation>,
    /// 0 for the main agent, 1 for its sub-agents, …
    pub depth: usize,
    /// `main` or the sub-agent run id (progress events).
    pub agent_id: String,
    /// P2: half-life in days for the recall decay (`[agent]
    /// salience_half_life_days`, default 90).
    pub salience_half_life_days: f64,
    /// D2: bind this agent's turns to a plan (re-anchoring + trajectory
    /// check). None = unbound; the turn loop never touches plans then.
    /// A sub-agent never inherits the parent's plan (its brief is its
    /// own task) — see `subagent::run`, which clears this.
    pub plan_id: Option<String>,
    /// D2: re-anchor every N steps while a plan is bound
    /// (`[agent] reanchor_every`, default [`crate::reanchor::DEFAULT_REANCHOR_EVERY`]).
    pub reanchor_every: usize,
}

impl AgentConfig {
    /// §3.4 effective budget for a turn's whole context:
    /// `min(total_budget, window × ratio)`; unknown window → the
    /// absolute total only.
    pub fn effective_budget(&self) -> usize {
        crate::context_budget::effective_budget(
            self.total_budget,
            self.context_window,
            self.window_ratio,
        )
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            user_name: "用户".into(),
            user_email: String::new(),
            tz: chrono_tz::Asia::Shanghai,
            max_steps: 24,
            total_budget: crate::context_budget::DEFAULT_TOTAL_BUDGET,
            window_ratio: crate::context_budget::DEFAULT_WINDOW_RATIO,
            context_window: None,
            keep_tail_turns: 3,
            max_tokens: None,
            workload: Workload::Main,
            system_note: None,
            origin: None,
            start_tainted: false,
            background: Vec::new(),
            delegation: None,
            depth: 0,
            agent_id: "main".into(),
            memory_context: Vec::new(),
            skills: Vec::new(),
            skill_tools: None,
            salience_half_life_days: 90.0,
            plan_id: None,
            reanchor_every: crate::reanchor::DEFAULT_REANCHOR_EVERY,
        }
    }
}

/// One observable moment of a turn's step loop (M0): the hook the
/// durable task runtime attaches to record a bounded execution unit.
/// `Decision` fires after the model returns a tool batch and BEFORE
/// any call is dispatched, so a killed worker can resume from the
/// recorded batch; `Result` fires after each call's observation is
/// written to the session; `Answer` fires when the model answers
/// instead of calling tools.
#[derive(Clone, Debug)]
pub enum StepEvent {
    Decision { step: usize, calls: Vec<ToolCall> },
    Result { step: usize, call_id: String, tool: String, content: String, trust: Trust },
    Answer { step: usize, text: String },
}

/// Per-turn knobs: streaming, cancellation and progress.
#[derive(Default)]
pub struct RunOptions<'o> {
    pub on_delta: Option<&'o mut StreamSink<'o>>,
    pub cancel: CancelToken,
    pub observer: Option<&'o Observer<'o>>,
    /// Durable-unit hook (M0): called for every step event above.
    /// `None` = this turn is not recorded (all existing callers).
    pub on_step_event: Option<&'o mut dyn FnMut(StepEvent)>,
}
