//! `hatch-connectors` — the boundary to the outside world.
//!
//! * [`mail`]     — IMAP source, SMTP sender, MIME, parsing, fixtures, local mail store.
//! * [`calendar`] — ICS import, local calendar store, conflict and free-slot queries.
//! * [`caldav`]   — CalDAV bidirectional sync (PROPFIND / REPORT / PUT /
//!   DELETE, sync-token state, server-wins conflicts), auth via the
//!   vault helper only.
//! * [`tls`]      — rustls client with the Mozilla root set (ring provider).
//! * [`framework`] — the connector framework: manifest, runner protocol,
//!   subprocess runner, registry. Reference connectors ship as binaries:
//!   `hatch-notes-connector` (local file notes) and
//!   `hatch-mockhttp-connector` (local mock HTTP service, tests only).
//! * [`payment`] — payment is explicitly deferred: no wallet, no
//!   checkout, no submit. `request_payment` always denies; the module
//!   documents the extension point for a future provider connector
//!   (framework + vault), which cannot execute a payment until the
//!   `Payment` autonomy ceiling is deliberately raised.
//!
//! Connectors never decide anything. They fetch, parse and deliver; every
//! decision about *whether* to deliver lives in `hatch-policy`. Passwords and
//! app-specific codes are read from environment variables by the caller and are
//! only held in memory.

pub mod calendar;
pub mod caldav;
pub mod framework;
pub mod mail;
pub mod payment;
pub mod tls;

pub use calendar::{CalendarRepo, Event};
pub use caldav::{CaldavClient, CaldavState, Conflict, Keep, SyncEngine, SyncReport, SyncSettings, SyncStatus, VaultHttp};
pub use mail::{MailRepo, MailSender, MailSource, OutgoingMail, ParsedMail, StoredMail};
