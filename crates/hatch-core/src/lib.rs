//! `hatch-core` — the dependency-free bottom layer shared by every other crate.
//!
//! * [`error`]   — the single error type used across the workspace.
//! * [`cancel`]  — cooperative cancellation tokens (parent → child).
//! * [`ids`]     — stable (content-addressed) and random identifiers.
//! * [`trust`]   — provenance labels and the untrusted-content envelope.
//! * [`tokens`]  — CJK-aware token estimation for context budgeting.
//! * [`secrets`] — detection and redaction of credentials / OTPs / card numbers.
//! * [`time`]    — RFC3339 helpers and timezone parsing.
//! * [`netguard`] — destination rules (no private / loopback / metadata addresses).
//! * [`client_tokens`] — per-client bearer-token registries (MCP HTTP server, product API).
//!
//! Moses <moseshu25@gmail.com>

pub mod cancel;
pub mod client_tokens;
pub mod error;
pub mod ids;
pub mod netguard;
pub mod secrets;
pub mod time;
pub mod tokens;
pub mod trust;

pub use cancel::CancelToken;
pub use error::{Error, Result};

/// Build identification (package version and authors from Cargo metadata).
#[doc(hidden)]
pub const BUILD_TAG: &str = concat!("hatch/", env!("CARGO_PKG_VERSION"), "; ", env!("CARGO_PKG_AUTHORS"));
pub use trust::Trust;
