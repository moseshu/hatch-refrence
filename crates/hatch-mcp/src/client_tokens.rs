//! Per-client bearer tokens for the MCP HTTP server.
//!
//! Thin wrapper over the shared registry in [`hatch_core::client_tokens`]:
//! same load / save / create / revoke / authenticate implementation
//! the product API uses, instantiated with the MCP kind (registry
//! file `mcp-clients.json`, token prefix `hatch_mcp_`). Storage rules
//! live there: atomic write, mode `0600`, only SHA-256 hashes stored,
//! plaintext shown exactly once at creation, server reloads on every
//! request so `revoke` takes effect immediately.

use hatch_core::client_tokens as core_tokens;
use std::path::{Path, PathBuf};

/// Registry file name under the hatch home directory.
pub const REGISTRY_FILE: &str = "mcp-clients.json";

/// The MCP token kind: `hatch_mcp_` tokens, `mcp_client` record ids.
pub struct McpKind;
impl core_tokens::ClientKind for McpKind {
    const TOKEN_PREFIX: &'static str = "hatch_mcp_";
    const ID_PREFIX: &'static str = "mcp_client";
}

/// The MCP client registry (shared implementation, MCP kind).
pub type ClientRegistry = core_tokens::ClientRegistry<McpKind>;
/// One registered MCP client (shared shape).
pub type ClientToken = core_tokens::ClientToken;
/// What authentication establishes about an MCP caller.
pub type ClientIdentity = core_tokens::ClientIdentity;

/// The registry file for a home directory: `<home>/mcp-clients.json`.
pub fn registry_path(home: &Path) -> PathBuf {
    core_tokens::registry_path(home, REGISTRY_FILE)
}
