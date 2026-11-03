//! `hatch-workers` — work the scheduler runs while nobody is chatting.
//!
//! | Kind | What it does | Model |
//! |---|---|---|
//! | [`brief::KIND`] `daily_brief` | today's and tomorrow's mail, calendar, conflicts, approvals, reminders | Brief (phrasing only; every item cites a fact) |
//! | [`triage_sync::KIND`] `triage_sync` | runs mail triage on a schedule | Triage / Draft |
//! | [`memory_flush::KIND`] `memory_flush` | merges near-duplicate claims, reports expired ones, proposes the rest | MemoryFlush (proposals only) |
//!
//! Workers never widen permissions: external actions go through a scoped
//! gatekeeper and the approval queue. Their output is stored for the next
//! chat ([`outputs`]) and announced according to the job's delivery setting.

pub mod brief;
pub mod memory_flush;
pub mod outputs;
pub mod seed;
pub mod triage_sync;

pub use brief::{BriefStore, BriefWorker, StoredBrief};
pub use memory_flush::MemoryFlushWorker;
pub use outputs::{deliver, handoff_lines, WorkerOutput, WorkerOutputs};
pub use triage_sync::TriageSyncWorker;

/// All built-in job kinds.
pub const KINDS: [&str; 3] = [brief::KIND, triage_sync::KIND, memory_flush::KIND];
