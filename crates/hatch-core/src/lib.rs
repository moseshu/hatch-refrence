//! `hatch-core` — the dependency-free bottom layer shared by every other crate.
//!
//! * [`error`]   — the single error type used across the workspace.
//! * [`ids`]     — stable (content-addressed) and random identifiers.
//! * [`trust`]   — provenance labels and the untrusted-content envelope.
//! * [`tokens`]  — CJK-aware token estimation for context budgeting.
//! * [`secrets`] — detection and redaction of credentials / OTPs / card numbers.
//! * [`time`]    — RFC3339 helpers and timezone parsing.
//!
//! Moses <moseshu25@gmail.com>

pub mod error;
pub mod ids;
pub mod secrets;
pub mod time;
pub mod tokens;
pub mod trust;

pub use error::{Error, Result};

/// Build identification (package version and authors from Cargo metadata).
#[doc(hidden)]
pub const BUILD_TAG: &str = concat!("hatch/", env!("CARGO_PKG_VERSION"), "; ", env!("CARGO_PKG_AUTHORS"));
pub use trust::Trust;
