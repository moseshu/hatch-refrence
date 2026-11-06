//! Agent configuration: per-agent settings and per-turn knobs.
//!
//! Split out of `agent.rs` so the turn-loop file stays under the
//! 400-line limit. Moved verbatim; behaviour is unchanged.

use crate::progress::Observer;
use crate::subagent::Delegation;
use chrono_tz::Tz;
use hatch_core::CancelToken;
use hatch_llm::{StreamSink, Workload};

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub user_name: String,
    pub user_email: String,
    pub tz: Tz,
    pub max_steps: usize,
    /// Total context budget in (estimated) tokens.
    pub context_budget: usize,
    /// Compact when live history exceeds this many tokens.
    pub compaction_threshold: usize,
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            user_name: "用户".into(),
            user_email: String::new(),
            tz: chrono_tz::Asia::Shanghai,
            max_steps: 24,
            context_budget: 24_000,
            compaction_threshold: 12_000,
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
        }
    }
}

/// Per-turn knobs: streaming, cancellation and progress.
#[derive(Default)]
pub struct RunOptions<'o> {
    pub on_delta: Option<&'o mut StreamSink<'o>>,
    pub cancel: CancelToken,
    pub observer: Option<&'o Observer<'o>>,
}
