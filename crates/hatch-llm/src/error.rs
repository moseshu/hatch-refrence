//! Model errors, classified so the router knows whether to retry.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmErrorKind {
    /// Network failure, timeout, HTTP 429 or 5xx — worth one retry.
    Transient,
    /// Bad request, auth failure, unparsable response — try the next model.
    Permanent,
    /// The configured call budget is exhausted — stop immediately.
    Budget,
    /// The endpoint refused the streaming form of the request before sending
    /// anything; the same request may succeed buffered.
    StreamRejected,
    /// The backend answered but the answer is unusable (empty: no text and
    /// no tool calls). The router stops — it does not fall through to the
    /// next model on a backend that produced nothing.
    Backend,
    /// Stopped on request (Ctrl-C, parent cancelled) — never retried.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmError {
    pub kind: LlmErrorKind,
    pub message: String,
}

impl LlmError {
    pub fn transient(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::Transient, message: m.into() }
    }
    pub fn permanent(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::Permanent, message: m.into() }
    }
    pub fn budget(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::Budget, message: m.into() }
    }
    pub fn stream_rejected(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::StreamRejected, message: m.into() }
    }
    pub fn backend(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::Backend, message: m.into() }
    }
    pub fn cancelled(m: impl Into<String>) -> Self {
        LlmError { kind: LlmErrorKind::Cancelled, message: m.into() }
    }
    pub fn is_cancelled(&self) -> bool {
        self.kind == LlmErrorKind::Cancelled
    }
    pub fn is_budget(&self) -> bool {
        self.kind == LlmErrorKind::Budget
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for LlmError {}

impl From<LlmError> for hatch_core::Error {
    fn from(e: LlmError) -> Self {
        if e.is_cancelled() {
            hatch_core::Error::Cancelled(e.message)
        } else {
            hatch_core::Error::Model(e.to_string())
        }
    }
}
