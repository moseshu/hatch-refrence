//! `hatch-policy` — who may do what, and when to bother the user.
//!
//! * [`action`]    — action classes, reversibility and autonomy levels.
//! * [`gate`]      — the gatekeeper: Allow / RequireApproval / Deny.
//! * [`grants`]    — standing orders the user issues ("always OK to reply to my team").
//! * [`approvals`] — the asynchronous approval queue (nothing ever blocks on stdin).
//! * [`ledger`]    — idempotency ledger for external side effects.
//! * [`attention`] — interruption budget and quiet hours.

pub mod action;
pub mod approvals;
pub mod attention;
pub mod gate;
pub mod grants;
pub mod ledger;

pub use action::{ActionClass, Autonomy, DataLabel, Reversibility};
pub use approvals::{Approval, ApprovalQueue, ApprovalStatus, NewApproval, SubmitOutcome};
pub use attention::{AttentionPolicy, Delivery, Notification, Notifier, Priority};
pub use gate::{ActionRequest, Decision, Gatekeeper, PolicyConfig};
pub use grants::{command_pattern_matches, host_pattern_matches, path_pattern_matches, Grant, GrantStore, NewGrant};
pub use ledger::{ActionLedger, LedgerState};
