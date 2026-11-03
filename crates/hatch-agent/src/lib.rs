//! `hatch-agent` — the conversational loop.
//!
//! * [`prompt`]     — the behaviour spec (versioned by content hash).
//! * [`session`]    — persisted transcripts with trust labels and compaction marks.
//! * [`assemble`]   — layered context: spec → profile → recalled memory → summary → history.
//! * [`compaction`] — summarise old turns at user-message boundaries, keep the tail verbatim.
//! * [`agent`]      — the turn loop. Tool calls go through the gate; actions that
//!   need consent are queued and the loop continues — it never blocks on the user.
//! * [`report`]     — stop reasons (answered / step limit / budget / error / cancelled) and receipts.
//! * [`progress`]   — short live progress lines for the CLI and the run store.
//! * [`subagent`]   — delegation: types, runs, parallel execution, handoffs.

pub mod agent;
pub mod assemble;
pub mod compaction;
pub mod progress;
pub mod prompt;
pub mod report;
pub mod session;
pub mod subagent;

pub use agent::{Agent, AgentConfig, RunOptions};
pub use progress::{Observer, Progress, ProgressKind};
pub use report::{StopReason, TurnReport};
pub use subagent::{AgentType, Delegation, RunRecord, RunStatus, RunStore, TypeRegistry};
pub use session::{RewindReport, SessionInfo, SessionStore, StoredMessage, TranscriptRow};
