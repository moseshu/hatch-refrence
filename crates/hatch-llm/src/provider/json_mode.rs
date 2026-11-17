//! `json_mode` input guarantee for the Responses provider.
//!
//! The Responses API moves system messages into `instructions`, outside
//! `input` — but a Responses gateway validates only the `input` messages
//! when `text.format` is `json_object` ("Response input messages must
//! contain the word 'json' in some form", HTTP 400, `rix_api_error`;
//! 2026-11-04 Colab live eval, zh-office 6/11 with every triage call
//! failing). Probe evidence the same day: instructions mentioning JSON
//! plus input without it → 400; the same request with the suffix below
//! in the input → 200.

use serde_json::{json, Value};

/// Suffix appended to the last user input message when `json_mode` is on
/// and no input message mentions JSON yet.
const JSON_MODE_SUFFIX: &str = "请以 JSON 对象回复。";

/// Whether any input item's message text mentions JSON (case-insensitive).
/// Content may be a plain string or a parts array (`input_text` parts
/// carry the text); image parts are skipped, never scanned — base64 data
/// could contain the letters "json" by chance.
fn input_mentions_json(input: &[Value]) -> bool {
    input.iter().any(|item| {
        let text = match item.get("content") {
            Some(Value::String(c)) => Some(c.as_str().to_string()),
            Some(Value::Array(parts)) => Some(
                parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        };
        text.is_some_and(|c| c.to_ascii_lowercase().contains("json"))
    })
}

/// Guarantee the word "json" appears in at least one `input` message.
///
/// When no input message mentions JSON yet, append the instruction to the
/// last user message; if there is no user message at all (e.g. a
/// tool-only turn), add one. Requests that already mention JSON, and
/// requests with `json_mode` off, are left untouched (the caller checks
/// the flag before calling this).
pub(crate) fn ensure_json_word_in_input(input: &mut Vec<Value>) {
    if input_mentions_json(input) {
        return;
    }
    for item in input.iter_mut().rev() {
        if item.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(Value::String(content)) = item.get_mut("content") {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(JSON_MODE_SUFFIX);
                return;
            }
        }
    }
    input.push(json!({"role": "user", "content": JSON_MODE_SUFFIX}));
}
