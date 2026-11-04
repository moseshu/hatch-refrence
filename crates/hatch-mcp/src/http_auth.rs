//! Per-request authentication for the MCP HTTP server.
//!
//! Split out of [`crate::http_server`] to keep that file under the
//! 400-line limit. Order: per-client registry token first (reloaded
//! from disk on every request, so revoke is immediate), then the
//! legacy shared token, then — only when neither is configured at
//! all — unauthenticated (loopback-only, enforced by the CLI).
//! A configured registry with no usable token, or an unreadable
//! registry file, fails closed.

use crate::client_tokens::{ClientIdentity, ClientRegistry};
use crate::http_proto::{bearer_ok, HttpRequest};
use crate::http_server::HttpServerConfig;
use crate::scope::Scope;
use crate::server::HTTP_ORIGIN;

/// What one request authenticated as: the client name (for logs and
/// for the approval origin), the origin to stamp on approvals, and
/// whether write tools may be called.
pub(crate) struct Auth {
    pub client: String,
    pub origin: String,
    pub allow_write: bool,
}

impl Auth {
    /// Identity of a per-client token: origin `mcp:http:<name>`, write
    /// tools only with the `write` scope.
    fn from_client(id: &ClientIdentity) -> Auth {
        Auth {
            client: id.name.clone(),
            origin: format!("{HTTP_ORIGIN}:{}", id.name),
            allow_write: id.scopes.contains(&Scope::Write),
        }
    }

    /// Identity of the legacy shared token: the historical origin
    /// `mcp:http`, full scope (backward compatible).
    fn legacy() -> Auth {
        Auth {
            client: "shared".to_string(),
            origin: HTTP_ORIGIN.to_string(),
            allow_write: true,
        }
    }

    /// Unauthenticated loopback access: same origin and scope as
    /// before per-client auth existed.
    fn anonymous() -> Auth {
        Auth {
            client: "-".to_string(),
            origin: HTTP_ORIGIN.to_string(),
            allow_write: true,
        }
    }
}

/// Authenticate one request against `config`.
/// `Err(status)` is the HTTP status to answer with (401/500).
pub(crate) fn authenticate(
    config: &HttpServerConfig,
    req: &HttpRequest,
) -> std::result::Result<Auth, u16> {
    let presented = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "));
    let mut registry_configured = false;
    if let Some(path) = &config.registry_path {
        match ClientRegistry::load_if_present(path) {
            Ok(Some(reg)) => {
                if let Some(id) = presented.and_then(|t| reg.authenticate(t)) {
                    return Ok(Auth::from_client(&id));
                }
                registry_configured = !reg.clients.is_empty();
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[mcp-http] token 注册表不可读，拒绝请求：{e}");
                return Err(500);
            }
        }
    }
    if let Some(token) = &config.token {
        if bearer_ok(req, token) {
            return Ok(Auth::legacy());
        }
        return Err(401);
    }
    if registry_configured {
        return Err(401);
    }
    Ok(Auth::anonymous())
}
