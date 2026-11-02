// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! `hatch-connectors` — the boundary to the outside world.
//!
//! * [`mail`]     — IMAP source, SMTP sender, MIME, parsing, fixtures, local mail store.
//! * [`calendar`] — ICS import, local calendar store, conflict and free-slot queries.
//! * [`tls`]      — rustls client with the Mozilla root set (ring provider).
//!
//! Connectors never decide anything. They fetch, parse and deliver; every
//! decision about *whether* to deliver lives in `hatch-policy`. Passwords and
//! app-specific codes are read from environment variables by the caller and are
//! only held in memory.

pub mod calendar;
pub mod mail;
pub mod tls;

pub use calendar::{CalendarRepo, Event};
pub use mail::{MailRepo, MailSender, MailSource, OutgoingMail, ParsedMail, StoredMail};
