//! `hatch-triage` — the first end-to-end scenario: mail + calendar triage.
//!
//! ```text
//! sync ─► classify (quarantined) ─► enrich (people/VIP, injection heuristics)
//!      ─► calendar (conflicts / free slots / propose event)
//!      ─► draft reply (recipient fixed by code) ─► gate ─► approval queue
//!      ─► notify (attention budget) ─► receipt
//! ```
//!
//! Every step is recorded in `workflow_steps`, so a re-run (or a run after a
//! crash) resumes where it stopped and never queues the same reply twice.

pub mod boundary;
pub mod classify;
pub mod desensitize;
pub mod draft;
pub mod injection;
pub mod model;
pub mod receipt;
pub mod repo;
pub mod workflow;

pub use model::{Category, ExtractedEvent, TriageResult};
pub use receipt::{ItemReceipt, Receipt};
pub use workflow::{TriageConfig, TriageWorkflow};
