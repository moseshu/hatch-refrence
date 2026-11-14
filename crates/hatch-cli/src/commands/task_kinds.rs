//! The task kind registry (M1, task 4): a task's kind is data, not a
//! forked worker. A new kind adds one row here and one runner
//! function; the claim/lease/event/budget machinery stays shared
//! across kinds.

/// The kind a task gets when none is given.
pub const DEFAULT_KIND: &str = "meeting-brief";

/// Every kind the worker can dispatch on.
pub const KNOWN_KINDS: &[&str] = &["meeting-brief", "meeting-invite", "delegate-join"];

pub fn is_known_kind(kind: &str) -> bool {
    KNOWN_KINDS.contains(&kind)
}
