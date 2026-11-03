//! `hatch-agent` — the conversational loop.
//!
//! * [`prompt`]     — the behaviour spec (versioned by content hash).
//! * [`session`]    — persisted transcripts with trust labels and compaction marks.
//! * [`assemble`]   — layered context: spec → profile → recalled memory → summary → history.
//! * [`compaction`] — summarise old turns at user-message boundaries, keep the tail verbatim.
//! * [`agent`]      — the turn loop. Tool calls go through the gate; actions that
//!   need consent are queued and the loop continues — it never blocks on the user.

pub mod agent;
pub mod assemble;
pub mod compaction;
pub mod prompt;
pub mod session;

pub use agent::{Agent, AgentConfig, StopReason, TurnReport};
pub use session::{SessionStore, StoredMessage};
