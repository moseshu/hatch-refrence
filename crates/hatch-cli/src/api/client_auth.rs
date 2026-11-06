//! Per-client bearer tokens for the product API (`/v1/...`).
//!
//! Mirrors the MCP HTTP server's per-client auth ([`hatch_mcp::http_auth`]):
//! a registry token first (reloaded from disk on every request, so
//! revoke is immediate), then the legacy shared token, then — only
//! when neither is configured at all — unauthenticated (loopback-only,
//! enforced by the CLI). A configured registry with no usable token,
//! or an unreadable registry file, fails closed.
//!
//! The registry is the shared implementation in
//! [`hatch_core::client_tokens`], instantiated with the daemon kind
//! (file `daemon-clients.json`, token prefix `hatch_daemon_`) —
//! separate from `mcp-clients.json`, so the two client sets never mix.
//!
//! ## Scope: read vs write, by method
//!
//! The API has no read-only POST: every `GET` is a read, every other
//! method mutates something, so the rule is one line:
//!
//! | Method | Scope needed | Examples |
//! |---|---|---|
//! | `GET` | `read` | sessions/memory/approvals/jobs/artifacts/skills/goals/feed/ideas/calendar/mail lists & details, `/v1/status`, inference usage |
//! | `POST` / `PATCH` / `DELETE` | `write` | create session, send message (JSON or SSE), cancel, fork, rewind, add claim, approve/deny, create/patch/delete/run job, create/delete/share/revoke artifact, create goal |
//!
//! A read token calling a write endpoint gets 403 `insufficient_scope`
//! and nothing executes — the request never reaches the handler or the
//! approval queue. The legacy shared token and unauthenticated
//! loopback access keep full scope, exactly as before per-client auth.

use hatch_core::client_tokens as core_tokens;
use hatch_core::client_tokens::{ClientIdentity, ClientRegistry, Scope};
use hatch_mcp::http_proto::{bearer_ok, HttpRequest};
use std::path::{Path, PathBuf};

/// Registry file name under the hatch home directory.
pub const REGISTRY_FILE: &str = "daemon-clients.json";

/// The daemon token kind: `hatch_daemon_` tokens, `daemon_client` ids.
pub struct DaemonKind;
impl core_tokens::ClientKind for DaemonKind {
    const TOKEN_PREFIX: &'static str = "hatch_daemon_";
    const ID_PREFIX: &'static str = "daemon_client";
}

/// The daemon client registry (shared implementation, daemon kind).
pub type DaemonRegistry = ClientRegistry<DaemonKind>;

/// The registry file for a home directory: `<home>/daemon-clients.json`.
pub fn registry_path(home: &Path) -> PathBuf {
    core_tokens::registry_path(home, REGISTRY_FILE)
}

/// What one request authenticated as: the client name (for logs) and
/// whether write endpoints may be called.
pub struct ApiAuth {
    pub client: String,
    pub allow_write: bool,
}

impl ApiAuth {
    /// Identity of a per-client token: write endpoints only with the
    /// `write` scope.
    fn from_client(id: &ClientIdentity) -> ApiAuth {
        ApiAuth {
            client: id.name.clone(),
            allow_write: id.scopes.contains(&Scope::Write),
        }
    }

    /// Identity of the legacy shared token: full scope (backward
    /// compatible).
    fn legacy() -> ApiAuth {
        ApiAuth {
            client: "shared".to_string(),
            allow_write: true,
        }
    }

    /// Unauthenticated loopback access: same scope as before
    /// per-client auth existed.
    fn anonymous() -> ApiAuth {
        ApiAuth {
            client: "-".to_string(),
            allow_write: true,
        }
    }
}

/// Whether an endpoint needs the `write` scope. The line is the HTTP
/// method: `GET` only reads, anything else mutates state (see the
/// module docs for the endpoint table).
pub fn requires_write(method: &str) -> bool {
    method != "GET"
}

/// Authenticate one request. `Err(status)` is the HTTP status to
/// answer with (401/500). `legacy_token` is the shared token
/// (`daemon-api` vault / `HATCH_DAEMON_API_TOKEN`), if configured.
pub fn authenticate(
    legacy_token: Option<&str>,
    registry_path: Option<&Path>,
    req: &HttpRequest,
) -> Result<ApiAuth, u16> {
    let presented = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "));
    let mut registry_configured = false;
    if let Some(path) = registry_path {
        match DaemonRegistry::load_if_present(path) {
            Ok(Some(reg)) => {
                if let Some(id) = presented.and_then(|t| reg.authenticate(t)) {
                    return Ok(ApiAuth::from_client(&id));
                }
                registry_configured = !reg.clients.is_empty();
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[api] token 注册表不可读，拒绝请求：{e}");
                return Err(500);
            }
        }
    }
    if let Some(token) = legacy_token {
        if bearer_ok(req, token) {
            return Ok(ApiAuth::legacy());
        }
        return Err(401);
    }
    if registry_configured {
        return Err(401);
    }
    Ok(ApiAuth::anonymous())
}
