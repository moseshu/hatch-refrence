//! Six-class failure attribution (task 9).
//!
//! A failed step is not one thing, and "the model was wrong" is the
//! least useful guess to default to. Every regression is attributed
//! to exactly one of six classes at one pipeline stage, so the fix
//! lands where the fault is:
//!
//! * `Runtime` — the infrastructure flaked (timeout, rate limit, a
//!   lease lost mid-call). Fix by retrying or watching the infra.
//! * `Model` — the model's own output was unusable (bad JSON, an
//!   empty turn, a step that cannot succeed as written). Fix via
//!   prompt, skill or model.
//! * `Tool` — the tool or connector faulted, or the call was
//!   malformed at the tool layer. Fix the tool.
//! * `Context` — the task lost information it needed (a key fact
//!   vanished across compaction, an ambiguous external state the
//!   runtime cannot decide). Fix context handling.
//! * `Permission` — the approval or the policy gate said no (or the
//!   approval expired). Fix the authorization, or ask the user.
//! * `Delivery` — the work ran but the result does not match the
//!   acceptance. Fix the verification or the step's `expected`.
//!
//! The classifier is a deterministic rule list over the failure
//! evidence; the first matching rule wins, in the order below, so
//! the mapping stays auditable. It reuses task 6's `FailureKind`
//! (passed in as its `as_str` string, so this crate does not depend
//! on `hatch-plan`).

use serde::{Deserialize, Serialize};

/// One of the six attribution classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Infrastructure fault: timeout, rate limit, lease lost.
    Runtime,
    /// The model's output was unusable or the step as written
    /// cannot succeed.
    Model,
    /// The tool or connector faulted, or the call was malformed.
    Tool,
    /// Needed information was lost or the external state is
    /// unknowable.
    Context,
    /// Approval refused / expired, or the policy gate denied.
    Permission,
    /// The work ran but the result does not match acceptance.
    Delivery,
}

impl FailureClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureClass::Runtime => "runtime",
            FailureClass::Model => "model",
            FailureClass::Tool => "tool",
            FailureClass::Context => "context",
            FailureClass::Permission => "permission",
            FailureClass::Delivery => "delivery",
        }
    }

    pub fn parse(s: &str) -> Option<FailureClass> {
        match s {
            "runtime" => Some(FailureClass::Runtime),
            "model" => Some(FailureClass::Model),
            "tool" => Some(FailureClass::Tool),
            "context" => Some(FailureClass::Context),
            "permission" => Some(FailureClass::Permission),
            "delivery" => Some(FailureClass::Delivery),
            _ => None,
        }
    }

    /// Short Chinese label for reports and the eval sample list.
    pub fn label(&self) -> &'static str {
        match self {
            FailureClass::Runtime => "运行时",
            FailureClass::Model => "模型",
            FailureClass::Tool => "工具",
            FailureClass::Context => "上下文",
            FailureClass::Permission => "权限",
            FailureClass::Delivery => "交付",
        }
    }

    /// All six classes, in the order the plan lists them.
    pub fn all() -> [FailureClass; 6] {
        [
            FailureClass::Runtime,
            FailureClass::Model,
            FailureClass::Tool,
            FailureClass::Context,
            FailureClass::Permission,
            FailureClass::Delivery,
        ]
    }
}

/// Where in the pipeline the failure happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The plan step itself (wrong tool or arguments as written).
    Plan,
    /// Choosing and dispatching the next tool call.
    Dispatch,
    /// The tool call running.
    Execute,
    /// Checking the result against `expected` / the receipt.
    Verify,
    /// The approval gate.
    Approve,
    /// Writing / delivering the artifact.
    Deliver,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Plan => "plan",
            Stage::Dispatch => "dispatch",
            Stage::Execute => "execute",
            Stage::Verify => "verify",
            Stage::Approve => "approve",
            Stage::Deliver => "deliver",
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "plan" => Some(Stage::Plan),
            "dispatch" => Some(Stage::Dispatch),
            "execute" => Some(Stage::Execute),
            "verify" => Some(Stage::Verify),
            "approve" => Some(Stage::Approve),
            "deliver" => Some(Stage::Deliver),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Stage::Plan => "规划",
            Stage::Dispatch => "分发",
            Stage::Execute => "执行",
            Stage::Verify => "验收",
            Stage::Approve => "审批",
            Stage::Deliver => "交付",
        }
    }
}

/// What is known about one failed step. Every field is already
/// desensitized by the caller — the classifier never sees raw
/// mail, addresses or secrets, and `error` must be a short
/// already-scrubbed message, never a full trace.
#[derive(Clone, Debug, Default)]
pub struct FailureEvidence {
    /// Tool that was running, if the failure is tied to one.
    pub tool: Option<String>,
    /// Short desensitized error text.
    pub error: String,
    /// Task 6 `FailureKind` as its `as_str` string:
    /// "transient" / "permanent" / "needs_human".
    pub failure_kind: Option<String>,
    /// The step was waiting on an approval, or one was refused.
    pub approval_involved: bool,
    /// The policy gate denied the call.
    pub policy_denied: bool,
    /// The verifier found the result does not match `expected`.
    pub receipt_mismatch: bool,
    /// A key fact or constraint vanished (compaction, stale
    /// summary), or the external state is unknowable.
    pub context_lost: bool,
    /// The model call itself failed or produced unusable output.
    pub model_error: bool,
    /// The tool call itself errored (unknown tool, bad arguments
    /// at the tool layer, connector failure).
    pub tool_fault: bool,
}

/// Attribute one failure. The first matching rule wins, in this
/// order — read top to bottom to audit a result.
pub fn classify(ev: &FailureEvidence) -> (FailureClass, Stage) {
    // 1. The gate said no: nothing else about the step matters.
    if ev.policy_denied {
        return (FailureClass::Permission, Stage::Approve);
    }
    // 2. A human-gated approval refused or expired.
    if ev.approval_involved && ev.failure_kind.as_deref() == Some("needs_human") {
        return (FailureClass::Permission, Stage::Approve);
    }
    // 3. The work ran; the result does not match acceptance.
    if ev.receipt_mismatch {
        return (FailureClass::Delivery, Stage::Verify);
    }
    // 4. Needed information is gone, or the outside world cannot
    // be decided from here.
    if ev.context_lost {
        return (FailureClass::Context, Stage::Execute);
    }
    // 5. The model call itself failed or produced unusable output.
    if ev.model_error {
        return (FailureClass::Model, Stage::Dispatch);
    }
    // 6. A passing infrastructure fault.
    if ev.failure_kind.as_deref() == Some("transient") {
        return (FailureClass::Runtime, Stage::Execute);
    }
    // 7. A human is needed but no approval is involved: the state
    // is ambiguous, which is a context gap, not a permission one.
    if ev.failure_kind.as_deref() == Some("needs_human") {
        return (FailureClass::Context, Stage::Execute);
    }
    // 8. Permanent and the tool layer faulted: fix the tool.
    if ev.failure_kind.as_deref() == Some("permanent") && ev.tool_fault {
        return (FailureClass::Tool, Stage::Execute);
    }
    // 9. Permanent, tool fine, result fine: the step as written
    // cannot succeed — the plan (model-written) is wrong.
    if ev.failure_kind.as_deref() == Some("permanent") {
        return (FailureClass::Model, Stage::Plan);
    }
    // 10. No evidence at all: blame the runtime, not the model.
    // An unknown fault routes to investigation; a wrong `Model`
    // would trigger a prompt change for a fault that may not be
    // the model's.
    (FailureClass::Runtime, Stage::Execute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> FailureEvidence {
        FailureEvidence::default()
    }

    #[test]
    fn rule_order_covers_each_class() {
        let mut e = ev();
        e.policy_denied = true;
        assert_eq!(classify(&e), (FailureClass::Permission, Stage::Approve));

        let mut e = ev();
        e.approval_involved = true;
        e.failure_kind = Some("needs_human".into());
        assert_eq!(classify(&e), (FailureClass::Permission, Stage::Approve));

        let mut e = ev();
        e.receipt_mismatch = true;
        assert_eq!(classify(&e), (FailureClass::Delivery, Stage::Verify));

        let mut e = ev();
        e.context_lost = true;
        assert_eq!(classify(&e), (FailureClass::Context, Stage::Execute));

        let mut e = ev();
        e.model_error = true;
        assert_eq!(classify(&e), (FailureClass::Model, Stage::Dispatch));

        let mut e = ev();
        e.failure_kind = Some("transient".into());
        assert_eq!(classify(&e), (FailureClass::Runtime, Stage::Execute));

        let mut e = ev();
        e.failure_kind = Some("needs_human".into());
        assert_eq!(classify(&e), (FailureClass::Context, Stage::Execute));

        let mut e = ev();
        e.failure_kind = Some("permanent".into());
        e.tool_fault = true;
        assert_eq!(classify(&e), (FailureClass::Tool, Stage::Execute));

        let mut e = ev();
        e.failure_kind = Some("permanent".into());
        assert_eq!(classify(&e), (FailureClass::Model, Stage::Plan));

        assert_eq!(classify(&ev()), (FailureClass::Runtime, Stage::Execute));
    }

    #[test]
    fn permission_beats_receipt_mismatch() {
        // A denied call that also mismatches is a permission fault:
        // the delivery never legitimately happened.
        let mut e = ev();
        e.policy_denied = true;
        e.receipt_mismatch = true;
        assert_eq!(classify(&e), (FailureClass::Permission, Stage::Approve));
    }

    #[test]
    fn class_and_stage_round_trip() {
        for c in FailureClass::all() {
            assert_eq!(FailureClass::parse(c.as_str()), Some(c));
        }
        for s in [
            Stage::Plan,
            Stage::Dispatch,
            Stage::Execute,
            Stage::Verify,
            Stage::Approve,
            Stage::Deliver,
        ] {
            assert_eq!(Stage::parse(s.as_str()), Some(s));
        }
        assert_eq!(FailureClass::parse("nope"), None);
        assert_eq!(Stage::parse("nope"), None);
    }
}
