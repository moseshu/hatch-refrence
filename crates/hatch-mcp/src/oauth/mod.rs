//! OAuth 2.0 for remote MCP servers (client side): PKCE S256
//! authorization-code flow, discovery, and deduplicated refresh.
//!
//! A remote MCP server that needs a login answers 401 with a
//! `WWW-Authenticate` header pointing at its OAuth metadata
//! ([`discovery`]); the flow ([`flow`]) runs the authorization code +
//! PKCE exchange over a loopback redirect; tokens and config live in
//! separate `0600` files under `<home>/mcp-oauth/<server>/`
//! ([`store`]); [`OAuthSession`] is the single place refreshes happen,
//! so concurrent callers cause exactly one refresh request.
//!
//! What this is not: DCR (dynamic client registration — later; the
//! client id is configured statically for now), and the server side
//! (`client_tokens.rs` per-client tokens are the reverse direction:
//! hatch as the MCP server authenticating its own clients).

pub mod discovery;
pub mod flow;
pub mod loopback;
pub mod pkce;
pub mod session;
pub mod store;

pub use discovery::{discover, ServerMetadata};
pub use flow::{begin_login, exchange_code, refresh};
pub use loopback::PendingLogin;
pub use session::OAuthSession;
pub use store::{oauth_dir, OAuthClientConfig, OAuthConfig, OAuthStore, OAuthTokens};
