//! Runner protocol: the JSON in/out contract between hatch and a connector
//! subprocess.
//!
//! One request per process invocation, newline-delimited JSON:
//!
//! * request (written to the child's stdin, one line):
//!   `{"op": "<op name>", "args": {...}, "request_id": "<id>"}`
//! * response (one line on the child's stdout):
//!   `{"ok": true, "result": ...}` or
//!   `{"ok": false, "error": {"code": "<stable code>", "message": "<text>"}}`
//!
//! Error codes are stable and provider-neutral. A connector must never put
//! secret material (tokens, passwords, full credentials) or provider
//! internals (stack traces, raw upstream bodies) into `error.message` —
//! messages are shown to the user and recorded in the audit log.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single connector invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorRequest {
    pub op: String,
    #[serde(default)]
    pub args: Value,
    pub request_id: String,
}

impl ConnectorRequest {
    pub fn new(op: &str, args: Value, request_id: &str) -> Self {
        ConnectorRequest {
            op: op.to_string(),
            args,
            request_id: request_id.to_string(),
        }
    }

    /// The single line written to the connector child's stdin.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// A connector's reply: either a result or a coded error.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ConnectorError>,
}

/// A stable, provider-neutral connector error.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorError {
    /// One of the `ERR_*` codes below.
    pub code: String,
    /// Human-readable message. Must not contain secrets or raw upstream
    /// bodies.
    pub message: String,
}

impl ConnectorError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        ConnectorError { code: code.to_string(), message: message.into() }
    }
}

/// The connector does not offer this op.
pub const ERR_UNKNOWN_OP: &str = "unknown_op";
/// The arguments are missing, wrongly typed, or fail validation.
pub const ERR_INVALID_ARGS: &str = "invalid_args";
/// The connector binary is not configured (missing env var, directory,
/// base URL, or vault token reference).
pub const ERR_NOT_CONFIGURED: &str = "not_configured";
/// A credential is missing or was rejected. The message must not say which
/// credential value was used.
pub const ERR_AUTH: &str = "auth_missing";
/// The requested item does not exist.
pub const ERR_NOT_FOUND: &str = "not_found";
/// The upstream (HTTP service, file system, ...) failed. The message
/// carries the failure class only, never a raw upstream body.
pub const ERR_UPSTREAM: &str = "upstream_error";
/// The connector did not answer in time.
pub const ERR_TIMEOUT: &str = "timeout";
/// The connector's reply was not a valid protocol response.
pub const ERR_BAD_RESPONSE: &str = "bad_response";
/// Anything else inside the connector.
pub const ERR_INTERNAL: &str = "internal";

/// Parse one stdout line into a [`ConnectorResponse`].
///
/// Anything that is not valid protocol JSON, or valid JSON with the wrong
/// shape, becomes [`ERR_BAD_RESPONSE`]. The offending line is never
/// included in the error (it could contain anything the child printed).
pub fn parse_response_line(line: &str) -> Result<ConnectorResponse, ConnectorError> {
    let value: Value = serde_json::from_str(line).map_err(|_| {
        ConnectorError::new(ERR_BAD_RESPONSE, "connector returned malformed JSON")
    })?;
    let ok = value.get("ok").and_then(Value::as_bool).ok_or_else(|| {
        ConnectorError::new(ERR_BAD_RESPONSE, "connector response has no boolean `ok`")
    })?;
    if ok {
        Ok(ConnectorResponse {
            ok: true,
            result: value.get("result").cloned().or(Some(Value::Null)),
            error: None,
        })
    } else {
        let err = value.get("error").cloned().unwrap_or(Value::Null);
        let code = err
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or(ERR_BAD_RESPONSE)
            .to_string();
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("connector reported failure without a message")
            .to_string();
        Ok(ConnectorResponse {
            ok: false,
            result: None,
            error: Some(ConnectorError { code, message }),
        })
    }
}

/// Render a response line (what a connector binary prints).
pub fn response_line(resp: &ConnectorResponse) -> String {
    serde_json::to_string(resp).unwrap_or_else(|_| {
        "{\"ok\":false,\"error\":{\"code\":\"internal\",\"message\":\"failed to serialize response\"}}"
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trip() {
        let req = ConnectorRequest::new("read_note", json!({"name": "x"}), "r-1");
        let line = req.to_line().unwrap();
        let back: ConnectorRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back.op, "read_note");
        assert_eq!(back.request_id, "r-1");
    }

    #[test]
    fn parse_ok_response() {
        let r = parse_response_line(r#"{"ok":true,"result":{"a":1}}"#).unwrap();
        assert!(r.ok);
        assert_eq!(r.result, Some(json!({"a": 1})));
    }

    #[test]
    fn parse_error_response() {
        let r = parse_response_line(
            r#"{"ok":false,"error":{"code":"not_found","message":"no such note"}}"#,
        )
        .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
    }

    #[test]
    fn parse_garbage_is_bad_response_without_echo() {
        let err = parse_response_line("not json at all {secret-leak}").unwrap_err();
        assert_eq!(err.code, ERR_BAD_RESPONSE);
        assert!(!err.message.contains("secret-leak"));
    }

    #[test]
    fn parse_missing_ok_is_bad_response() {
        assert!(parse_response_line(r#"{"result":1}"#).is_err());
    }
}
