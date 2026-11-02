// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Shared blocking HTTP POST with error classification.

use crate::LlmError;
use serde_json::Value;
use std::time::Duration;

pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: Duration,
) -> Result<Value, LlmError> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let mut req = agent.post(url).set("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_json(body.clone()) {
        Ok(resp) => resp
            .into_json::<Value>()
            .map_err(|e| LlmError::permanent(format!("response is not JSON: {e}"))),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            let snippet: String = text.chars().take(400).collect();
            let msg = format!("HTTP {code}: {snippet}");
            if code == 429 || code >= 500 {
                Err(LlmError::transient(msg))
            } else {
                Err(LlmError::permanent(msg))
            }
        }
        Err(e) => Err(LlmError::transient(format!("transport: {e}"))),
    }
}
