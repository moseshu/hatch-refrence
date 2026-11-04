//! `hatch-mcp` — MCP client (stdio JSON-RPC 2.0 and HTTP/SSE) and server
//! (stdio), plus the hatch tool/executor adapter for MCP servers.
//!
//! * Client: `stdio` spawns a child process and speaks newline-delimited
//!   JSON; `http` POSTs JSON-RPC to a URL and accepts a plain JSON reply
//!   or an SSE stream. Server configs hold env-var / header *names* only;
//!   the caller resolves them (and any token) and passes the values in.
//! * Server: [`Server`] exposes a hatch tool registry over stdio. Every
//!   `tools/call` goes through the same gatekeeper/approval path as a
//!   local tool call; a call that needs approval returns
//!   `approval_required` + an approval id instead of executing. The
//!   same server also backs the streamable-HTTP transport
//!   ([`HttpServer`], MCP spec 2025-03-26), with the same approval
//!   path and a per-transport origin (`mcp:http`).
//! * Adapter: remote tools are registered as `mcp_<server>_<tool>` and
//!   their calls are deferred to the approval queue (`mcp_call`); server
//!   output is always `Trust::Untrusted`.

pub mod adapter;
pub mod client;
pub mod http;
mod http_proto;
pub mod http_routes;
pub mod http_server;
pub mod server;
pub mod transport;
pub mod types;

pub use adapter::{mcp_tool, McpCallExecutor, McpTool};
pub use client::{Client, PROTOCOL_VERSION};
pub use http::HttpClient;
pub use http_server::{HttpServer, HttpServerConfig};
pub use server::{
    Server, HTTP_ORIGIN as MCP_HTTP_ORIGIN, HTTP_PROTOCOL_VERSION as SERVER_HTTP_PROTOCOL_VERSION,
    ORIGIN as MCP_SERVER_ORIGIN, PROTOCOL_VERSION as SERVER_PROTOCOL_VERSION,
};
pub use transport::AnyClient;
pub use types::{McpServerConfig, PromptInfo, ResourceInfo, ToolInfo};
