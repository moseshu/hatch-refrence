//! Helpers shared by the HTTP providers.

use crate::LlmError;
use serde_json::{json, Value};

/// Tool-call arguments that arrive as a JSON *string* (Chat Completions,
/// Responses). Empty → `{}`; malformed → `{"_unparsed": raw}` so the tool
/// layer can report a clean validation error instead of crashing.
pub(crate) fn parse_arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({"_unparsed": s})),
        Some(Value::Null) | None => json!({}),
        Some(other) => other.clone(),
    }
}

/// The model hit its output limit before producing any answer — typical of
/// reasoning models whose hidden thinking used up the whole budget.
pub(crate) fn truncation_error(detail: &str, completion_tokens: u64, reasoning_tokens: u64) -> LlmError {
    LlmError::permanent(format!(
        "output truncated before any answer ({detail}, completion_tokens={completion_tokens}, reasoning_tokens={reasoning_tokens}); raise max_tokens or lower reasoning_effort"
    ))
}

/// Read a `u64` at a JSON pointer below `v`, defaulting to 0.
pub(crate) fn u64_at(v: Option<&Value>, pointer: &str) -> u64 {
    v.and_then(|v| v.pointer(pointer)).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse a JSON SSE payload into a value, with a provider-neutral error.
pub(crate) fn sse_json(data: &str) -> Result<Value, LlmError> {
    serde_json::from_str(data).map_err(|e| LlmError::permanent(format!("bad stream chunk: {e}")))
}
