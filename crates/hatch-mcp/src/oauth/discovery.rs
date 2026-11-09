//! OAuth discovery for a remote MCP server (MCP spec 2025-03-26,
//! RFC 9728 protected-resource metadata, RFC 8414 authorization-server
//! metadata).
//!
//! Order: first try an unauthenticated GET of the MCP URL itself — a
//! server that requires OAuth answers 401 with
//! `WWW-Authenticate: Bearer resource_metadata="<url>"`. If that yields
//! nothing, fall back to the well-known resource-metadata URL at the
//! MCP URL's origin. Either path ends at the authorization server's
//! metadata document, which gives the authorization and token endpoints.
//!
//! Only the endpoint URLs are returned; no token material passes
//! through here.

use hatch_core::{Error, Result};
use serde::Deserialize;
use std::time::Duration;

/// What discovery found: the two endpoints the authorization-code flow
/// needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

/// Protected-resource metadata (RFC 9728): the authorization servers
/// that can issue tokens for this resource.
#[derive(Deserialize)]
struct ResourceMetadata {
    authorization_servers: Vec<String>,
}

/// Authorization-server metadata (RFC 8414): only the two endpoints
/// this flow needs are read; the rest is ignored.
#[derive(Deserialize)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
}

/// The `resource_metadata` URL from a `WWW-Authenticate` header, if the
/// header carries one (RFC 9728: `Bearer resource_metadata="<url>"`).
/// The `Bearer` scheme prefix (when present) is skipped; quoted-pair
/// escapes inside the value are not expected from servers.
pub fn resource_metadata_url(header: &str) -> Option<String> {
    for part in header.split(',') {
        let part = part.trim().strip_prefix("Bearer ").unwrap_or(part.trim());
        if let Some(rest) = part.strip_prefix("resource_metadata=") {
            let rest = rest.trim();
            let url = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(rest);
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// `scheme://host[:port]` of a URL (no `url` crate in the workspace, so
/// this is string surgery; enough for building well-known URLs).
pub fn origin_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    let scheme = url.split("://").next()?;
    if host.is_empty() || scheme.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// GET a JSON document with `ureq` (timeout 10s). The body is never
/// logged.
fn get_json(url: &str) -> Result<serde_json::Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => Error::Connector(format!(
            "oauth discovery: GET {url} returned HTTP {code}"
        )),
        ureq::Error::Transport(_) => Error::Connector(format!(
            "oauth discovery: could not reach {url}"
        )),
    })?;
    resp.into_json().map_err(|_| {
        Error::Connector(format!("oauth discovery: {url} returned non-JSON"))
    })
}

/// Fetch an authorization server's metadata document and read the two
/// endpoints out of it.
fn fetch_auth_server_metadata(url: &str) -> Result<ServerMetadata> {
    let body = get_json(url)?;
    let meta: AuthServerMetadata = serde_json::from_value(body).map_err(|_| {
        Error::Connector(format!(
            "oauth discovery: {url} has no authorization_endpoint/token_endpoint"
        ))
    })?;
    Ok(ServerMetadata {
        authorization_endpoint: meta.authorization_endpoint,
        token_endpoint: meta.token_endpoint,
    })
}

/// Discover the authorization + token endpoints for an MCP server URL.
/// Tries the `WWW-Authenticate` route first (GET the MCP URL without a
/// token), then the well-known resource-metadata URL at the origin.
pub fn discover(server_url: &str) -> Result<ServerMetadata> {
    // Route 1: the MCP URL itself answers 401 with resource_metadata.
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    if let Err(ureq::Error::Status(401, resp)) = agent.get(server_url).call() {
        if let Some(h) = resp.header("www-authenticate") {
            if let Some(meta_url) = resource_metadata_url(h) {
                let body = get_json(&meta_url)?;
                let meta: ResourceMetadata = serde_json::from_value(body).map_err(|_| {
                    Error::Connector(format!(
                        "oauth discovery: {meta_url} has no authorization_servers"
                    ))
                })?;
                if let Some(as_url) = meta.authorization_servers.first() {
                    let as_meta = format!(
                        "{}/.well-known/oauth-authorization-server",
                        as_url.trim_end_matches('/')
                    );
                    return fetch_auth_server_metadata(&as_meta);
                }
            }
        }
    }
    // Route 2: well-known resource metadata at the MCP URL's origin.
    let origin = origin_of(server_url).ok_or_else(|| {
        Error::Config(format!("oauth discovery: cannot parse origin of {server_url}"))
    })?;
    let meta_url = format!("{origin}/.well-known/oauth-protected-resource");
    let body = get_json(&meta_url)?;
    let meta: ResourceMetadata = serde_json::from_value(body).map_err(|_| {
        Error::Connector(format!(
            "oauth discovery: {meta_url} has no authorization_servers"
        ))
    })?;
    let as_url = meta.authorization_servers.first().ok_or_else(|| {
        Error::Connector(format!(
            "oauth discovery: {meta_url} lists no authorization server"
        ))
    })?;
    let as_meta = format!(
        "{}/.well-known/oauth-authorization-server",
        as_url.trim_end_matches('/')
    );
    fetch_auth_server_metadata(&as_meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn www_authenticate_resource_metadata() {
        let h = r#"Bearer resource_metadata="https://example.com/.well-known/oauth-protected-resource", realm="mcp""#;
        assert_eq!(
            resource_metadata_url(h).as_deref(),
            Some("https://example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(resource_metadata_url("Bearer realm=\"x\""), None);
        assert_eq!(resource_metadata_url(""), None);
    }

    #[test]
    fn origin_parsing() {
        assert_eq!(
            origin_of("https://example.com:8443/mcp").as_deref(),
            Some("https://example.com:8443")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8080/").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(origin_of("not-a-url"), None);
        assert_eq!(origin_of("https://"), None);
    }
}
