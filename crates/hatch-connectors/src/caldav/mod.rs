//! CalDAV bidirectional sync (Phase 6).
//!
//! Why a module of `hatch-connectors` and not a new crate: the sync
//! engine is a thin layer over the existing calendar store
//! ([`crate::calendar`], ICS parsing, `calendar_events`) and the vault
//! helper for auth — a separate crate would only re-export the same
//! dependencies. The pieces:
//!
//! * [`client`] — PROPFIND / REPORT / PUT / DELETE over the vault
//!   helper (the token never enters this process).
//! * [`response`] — parsing DAV multistatus bodies into server events.
//! * [`xml`] — hand-rolled DAV XML reader (no XML crate in the
//!   workspace; only the elements CalDAV needs are parsed).
//! * [`state`] — sync-token / etag / conflict state in SQLite, plus
//!   the offline `resolve` and `status` operations.
//! * [`sync`] — the engine: pull → apply → push, and the conflict rule
//!   (server wins unless the user explicitly approves the local version).

pub mod client;
pub mod response;
pub mod state;
pub mod sync;
pub mod xml;

pub use client::{CaldavClient, CalendarInfo, ServerEvent, VaultHttp};
pub use state::{CaldavState, Conflict, Keep};
pub use sync::{SyncEngine, SyncReport, SyncSettings, SyncStatus};
