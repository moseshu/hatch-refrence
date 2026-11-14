//! `hatch-policy` — who may do what, and when to bother the user.
//!
//! * [`action`]    — action classes, reversibility and autonomy levels.
//! * [`outbound`]  — name heuristic for sends, and the plan-class re-check.
//! * [`gate`]      — the gatekeeper: Allow / RequireApproval / Deny.
//! * [`grants`]    — standing orders the user issues ("always OK to reply to my team").
//! * [`safety_floor`] — six shell command classes that are never auto-approved.
//! * [`approvals`] — the asynchronous approval queue (nothing ever blocks on stdin).
//! * [`ledger`]    — idempotency ledger for external side effects.
//! * [`attention`] — interruption budget and quiet hours.

/// Execution policy version, pinned by a release (task 9).
/// Bump this when a gate rule changes — a release records the
/// version it was cut under, and a checkpoint records the version
/// it was written under, so a behaviour change is visible instead
/// of silent.
pub const POLICY_VERSION: &str = "1";

pub mod action;
pub mod approvals;
pub mod attention;
pub mod gate;
pub mod grants;
pub mod ledger;
pub mod outbound;
pub mod safety_floor;
mod command_parse;
mod shell_lex;
mod shell_lex_engine;

pub use action::{ActionClass, Autonomy, DataLabel, Reversibility};
pub use approvals::{Approval, ApprovalQueue, ApprovalStatus, NewApproval, SubmitOutcome};
pub use attention::{AttentionPolicy, Delivery, Notification, Notifier, Priority};
pub use gate::{ActionRequest, Decision, Gatekeeper, PolicyConfig};
pub use grants::{command_pattern_matches, host_pattern_matches, path_pattern_matches, Grant, GrantStore, NewGrant};
pub use ledger::{ActionLedger, LedgerState};
pub use outbound::{is_outbound_tool_name, recheck_class};
pub use safety_floor::{floor_reading, FloorReading, FloorRule};
