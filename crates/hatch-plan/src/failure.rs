//! Step failure classification (task 6): a failed step is not one
//! thing. The class decides the recovery path, and the classes are
//! deliberately few so the routing stays auditable:
//!
//! * `Transient` — a passing fault (timeout, rate limit, a service
//!   briefly unavailable). The step may be retried; the task stays
//!   resumable and the next run re-runs exactly that step.
//! * `Permanent` — the call cannot succeed as written (bad arguments,
//!   unknown tool, a delivery that was checked and does not match).
//!   Retrying the same call would fail the same way; the task fails
//!   visibly and the fix is a plan change, not a retry.
//! * `NeedsHuman` — the runtime cannot decide safely: the external
//!   state is unknown (ambiguous), the user rejected the action, or
//!   the approval expired / was invalidated by a version bump. The
//!   task blocks with a clear reason; nothing retries silently.
//!
//! The classifier is a deterministic heuristic over the failure
//! evidence (the error text and whether the verifier flagged the
//! external state as unknown). It is conservative on purpose: only a
//! clear transient signal earns a retry. Anything else that is not
//! recognisably human-gated is `Permanent` — a wrong `Permanent`
//! costs a visible failure the user can fix, while a wrong
//! `Transient` would burn retries on a call that can never succeed.

use serde::{Deserialize, Serialize};

/// How a step failed, and therefore how it recovers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Retryable: a passing fault.
    Transient,
    /// Not retryable as written: the fix is a plan change.
    Permanent,
    /// Needs a human decision: ambiguous state, rejected or
    /// invalidated approval.
    NeedsHuman,
}

impl FailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureKind::Transient => "transient",
            FailureKind::Permanent => "permanent",
            FailureKind::NeedsHuman => "needs_human",
        }
    }

    pub fn parse(s: &str) -> Option<FailureKind> {
        match s {
            "transient" => Some(FailureKind::Transient),
            "permanent" => Some(FailureKind::Permanent),
            "needs_human" => Some(FailureKind::NeedsHuman),
            _ => None,
        }
    }

    /// Short Chinese label for the plan card.
    pub fn label(&self) -> &'static str {
        match self {
            FailureKind::Transient => "暂时失败（可重试）",
            FailureKind::Permanent => "永久失败（需改计划）",
            FailureKind::NeedsHuman => "需人工介入",
        }
    }
}

/// Where a classified failure goes. The worker routes on this, not
/// on the raw error text, so the three paths cannot drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Leave the task resumable; the next run re-runs the failed step.
    Retry,
    /// Fail the task visibly with the reason.
    FailTask,
    /// Block the task with a clear reason; only a human unblocks it.
    BlockForHuman,
}

/// The recovery path for a failure kind.
pub fn recovery_for(kind: FailureKind) -> Recovery {
    match kind {
        FailureKind::Transient => Recovery::Retry,
        FailureKind::Permanent => Recovery::FailTask,
        FailureKind::NeedsHuman => Recovery::BlockForHuman,
    }
}

/// Classify a step failure from its evidence.
///
/// `ambiguous` is true when the verifier could not tell whether the
/// side effect happened (M1 `StepVerify::Ambiguous`): the external
/// state is unknown, so no recovery may retry or assume — a human
/// must look first.
///
/// The error text is matched on markers, in both languages, because
/// tool errors surface in either. Only a clear transient signal
/// earns `Transient`; an approval outcome earns `NeedsHuman`;
/// everything else is `Permanent`.
pub fn classify_failure(error: &str, ambiguous: bool) -> FailureKind {
    if ambiguous {
        return FailureKind::NeedsHuman;
    }
    let lower = error.to_lowercase();
    // Human-gated outcomes first: a rejected, expired or
    // version-invalidated approval is a decision the runtime cannot
    // remake on its own.
    for marker in [
        "已被拒绝",
        "已过期",
        "已随计划版本失效",
        "rejected",
        "expired",
        "invalidated",
    ] {
        if lower.contains(marker) {
            return FailureKind::NeedsHuman;
        }
    }
    // Clear transient signals: the fault is in the world, not the call.
    for marker in [
        "超时",
        "timeout",
        "timed out",
        "暂时",
        "temporary",
        "不可用",
        "unavailable",
        "限流",
        "rate limit",
        "too many requests",
        "请稍后重试",
        "try again",
        "503",
        "502",
        "504",
        "connection reset",
        "connection refused",
    ] {
        if lower.contains(marker) {
            return FailureKind::Transient;
        }
    }
    FailureKind::Permanent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_is_always_needs_human() {
        assert_eq!(
            classify_failure("anything at all", true),
            FailureKind::NeedsHuman
        );
        assert_eq!(
            classify_failure("timeout after 30s", true),
            FailureKind::NeedsHuman,
            "ambiguity beats even a transient-looking error"
        );
    }

    #[test]
    fn approval_outcomes_need_a_human() {
        assert_eq!(
            classify_failure("审批 apr_1 已被拒绝", false),
            FailureKind::NeedsHuman
        );
        assert_eq!(
            classify_failure("审批 apr_1 已过期", false),
            FailureKind::NeedsHuman
        );
        assert_eq!(
            classify_failure("审批 apr_1 已随计划版本失效", false),
            FailureKind::NeedsHuman
        );
    }

    #[test]
    fn transient_signals_retry() {
        for error in [
            "read timed out after 30s",
            "503 Service Unavailable",
            "rate limit exceeded, try again later",
            "服务暂时不可用",
            "连接超时",
        ] {
            assert_eq!(
                classify_failure(error, false),
                FailureKind::Transient,
                "error: {error}"
            );
        }
    }

    #[test]
    fn everything_else_is_permanent() {
        for error in [
            "file not found: /no/such/path",
            "unknown tool `nope`",
            "invalid arguments: missing `to`",
            "delivery does not match expected",
            "审批 apr_1 执行失败",
        ] {
            assert_eq!(
                classify_failure(error, false),
                FailureKind::Permanent,
                "error: {error}"
            );
        }
    }

    #[test]
    fn recovery_routing() {
        assert_eq!(recovery_for(FailureKind::Transient), Recovery::Retry);
        assert_eq!(recovery_for(FailureKind::Permanent), Recovery::FailTask);
        assert_eq!(
            recovery_for(FailureKind::NeedsHuman),
            Recovery::BlockForHuman
        );
        assert_eq!(
            FailureKind::parse("transient"),
            Some(FailureKind::Transient)
        );
        assert_eq!(FailureKind::parse("bogus"), None);
    }
}
