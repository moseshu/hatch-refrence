//! `hatch-agent` — the conversational loop.
//!
//! * [`prompt`]     — the behaviour spec (versioned by content hash).
//! * [`session`]    — persisted transcripts with trust labels and compaction marks.
//! * [`assemble`]   — layered context: spec → profile → recalled memory → summary → history.
//! * [`system_prompt`] — system-prompt rendering (stable prefix + volatile tail) and per-layer estimated token counts.
//! * [`compaction`] — summarise old turns at user-message boundaries, keep the tail verbatim.
//! * [`worker`]     — the same compaction as a scheduler job: the turn loop
//!   only enqueues it, a background worker does the summarising.
//! * [`agent`]      — the turn loop. Tool calls go through the gate; actions that
//!   need consent are queued and the loop continues — it never blocks on the user.
//! * [`config`]     — per-agent settings ([`AgentConfig`]) and per-turn knobs ([`RunOptions`]).
//! * [`report`]     — stop reasons (answered / step limit / budget / error / cancelled) and receipts.
//! * [`progress`]   — short live progress lines for the CLI and the run store.
//! * [`subagent`]   — delegation: types, runs, parallel execution, handoffs.

pub mod agent;
pub mod assemble;
pub mod bridge;
pub mod bridge_call;
pub mod compaction;
pub mod config;
pub mod progress;
pub mod prompt;
pub mod report;
pub mod session;
pub mod subagent;
pub mod system_prompt;
pub mod unanswered;
pub mod worker;

pub use agent::{Agent, AgentConfig, RunOptions};
pub use progress::{Observer, Progress, ProgressKind};
pub use report::{StopReason, TurnReport};
pub use subagent::{
    AgentType, ChildRegistry, Delegation, PendingHandoff, RegistryEntry, RunRecord, RunStatus, RunStore, SpawnMode,
    TypeRegistry,
};
pub use session::{RewindReport, SessionInfo, SessionStore, StoredMessage, TranscriptRow};
