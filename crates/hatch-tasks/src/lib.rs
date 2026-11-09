//! `hatch-tasks` — P1 D6, the unified task ledger.
//!
//! One durable row per task of any kind (worker job, sub-agent run,
//! scheduled job run, one-off task), with a six-state machine
//! (`queued / running / waiting / blocked / done / failed`), owner,
//! parent task, retry count, last heartbeat, and a task-level
//! idempotency key. A process restart reconciles the ledger: tasks a
//! previous process left in `running` whose heartbeat is stale are put
//! back to `waiting` (retries left) or `failed` (retries exhausted),
//! so no task stays stuck in `running` forever.
//!
//! * [`task`]  — [`Task`] / [`TaskStatus`] and the transition table.
//! * [`store`] — [`TaskStore`], the SQLite persistence, transitions,
//!   heartbeat, and startup reconcile ([`TaskStore::reconcile`]).
//!
//! Deliberately not here: live heartbeat updates and liveness probing
//! (D7), snapshots (D11), pause/resume (D5). And no producer writes
//! task rows yet — worker / scheduler / sub-agent adoption is a
//! follow-up; this crate is the ledger they will share.

pub mod store;
pub mod task;

pub use store::{ReconcileReport, TaskStore, DEFAULT_STALE_AFTER_SECS};
pub use task::{Task, TaskStatus};
