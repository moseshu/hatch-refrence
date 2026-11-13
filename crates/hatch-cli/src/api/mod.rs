//! Product API v1 (`/v1/...`) served by `hatch daemon run --api`.
//!
//! The backend API for external clients: std-only `TcpListener`, one
//! thread per connection — the same shape as the MCP streamable-HTTP
//! server. The frontend lives in a separate repo and consumes this
//! API; this crate serves no static frontend files. Plain JSON calls
//! plus SSE for chat turns (see [`chat`]). Errors are always
//! `{"error": {"code": "<stable>", "message": "<human-readable>"}}`.
//!
//! What v1 deliberately does NOT do: TLS (reverse proxy), multi-tenant
//! isolation (single-user dev token; isolation is separate work),
//! payments (still a stub), real multi-channel transports.

#[cfg(test)]
mod chat_tests;

pub mod agents;
pub mod approvals;
pub mod artifacts;
pub mod auth;
pub mod client_auth;
#[cfg(test)]
mod client_auth_tests;
pub mod chat;
pub mod http;
pub mod inference;
pub mod jobs;
pub mod memory;
pub mod resources;
pub mod routes;
pub mod server;
#[cfg(test)]
mod server_tests;

#[cfg(test)]
mod server_tests_resources;
pub mod sessions;
pub mod task_events;
pub mod tasks;

use crate::app::App;
use hatch_core::{CancelToken, Error as CoreError};
use hatch_llm::Router;
use hatch_mcp::http_proto::HttpRequest;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared state for API request handlers.
pub struct ApiCtx {
    pub app: Arc<App>,
    /// Builds a model router for a chat turn. Production builds from
    /// config; tests inject a scripted model.
    pub router_factory: Arc<dyn Fn() -> Result<Router, ApiError> + Send + Sync>,
    /// Turns currently running, by session id — for
    /// `POST /v1/sessions/{id}/cancel`.
    pub running: Mutex<HashMap<String, CancelToken>>,
}

impl ApiCtx {
    pub fn new(app: Arc<App>) -> Self {
        let a = app.clone();
        ApiCtx {
            app,
            router_factory: Arc::new(move || {
                if a.config.resolved_models().is_empty() {
                    return Err(ApiError::new(
                        503,
                        "model_not_configured",
                        "没有配置模型。请在 config.toml 里配置 [[models]]，或设置 HATCH_LLM_BASE_URL / HATCH_LLM_MODEL / HATCH_LLM_API_KEY。",
                    ));
                }
                a.router(a.default_limits()).map_err(ApiError::from)
            }),
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Test hook: replace the router factory (e.g. a scripted model).
    #[cfg(test)]
    pub fn with_router_factory(
        mut self,
        f: Arc<dyn Fn() -> Result<Router, ApiError> + Send + Sync>,
    ) -> Self {
        self.router_factory = f;
        self
    }
}

/// One API error: HTTP status + stable machine-readable code + a
/// human-readable message. Rendered as
/// `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError::new(400, "invalid_request", message)
    }

    pub fn not_found(what: &str) -> Self {
        ApiError::new(404, "not_found", format!("{what} 不存在"))
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        ApiError::new(409, "conflict", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::new(500, "internal_error", message)
    }

    pub fn to_json(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::NotFound(m) => ApiError::not_found(&m),
            CoreError::Invalid(m) => ApiError::bad_request(m),
            CoreError::SecretRejected(m) => ApiError::bad_request(m),
            CoreError::Denied(m) => ApiError::new(403, "forbidden", m),
            CoreError::QuotaExceeded(m) => ApiError::new(403, "quota_exceeded", m),
            CoreError::Cancelled(m) => ApiError::new(499, "cancelled", m),
            CoreError::Config(m) => ApiError::new(503, "model_not_configured", m),
            e => ApiError::internal(e.to_string()),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(format!("{e:#}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::new(400, "invalid_json", e.to_string())
    }
}

/// Parse a JSON request body. An empty body reads as `null`.
pub fn json_body(req: &HttpRequest) -> Result<Value, ApiError> {
    if req.body.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&req.body)?)
}

/// Required string field, or 400.
pub fn req_str(body: &Value, field: &str) -> Result<String, ApiError> {
    body.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ApiError::bad_request(format!("缺少字段 {field}（字符串）")))
}

/// Optional string field.
pub fn opt_str(body: &Value, field: &str) -> Option<String> {
    body.get(field).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Optional boolean field.
pub fn opt_bool(body: &Value, field: &str) -> Option<bool> {
    body.get(field).and_then(|v| v.as_bool())
}

/// Optional unsigned integer field.
pub fn opt_u64(body: &Value, field: &str) -> Option<u64> {
    body.get(field).and_then(|v| v.as_u64())
}

/// Optional signed integer field.
pub fn opt_i64(body: &Value, field: &str) -> Option<i64> {
    body.get(field).and_then(|v| v.as_i64())
}

/// Optional string list field.
pub fn opt_str_list(body: &Value, field: &str) -> Vec<String> {
    body.get(field)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
