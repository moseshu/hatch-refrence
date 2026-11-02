// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch-core` — the dependency-free bottom layer shared by every other crate.
//!
//! * [`error`]   — the single error type used across the workspace.
//! * [`ids`]     — stable (content-addressed) and random identifiers.
//! * [`trust`]   — provenance labels and the untrusted-content envelope.
//! * [`tokens`]  — CJK-aware token estimation for context budgeting.
//! * [`secrets`] — detection and redaction of credentials / OTPs / card numbers.
//! * [`time`]    — RFC3339 helpers and timezone parsing.

pub mod error;
pub mod ids;
pub mod secrets;
pub mod time;
pub mod tokens;
pub mod trust;

pub use error::{Error, Result};
pub use trust::Trust;
