//! `hatch-browser` — a browser the assistant can use, under the same rules as
//! everything else it does.
//!
//! * [`broker`]    — profiles (one Chromium each), sessions (one tab each),
//!   attach / reopen, idle cleanup.
//! * [`page`]      — actions on a tab over CDP; destination checks for every
//!   request while attached.
//! * [`snapshot`]  — readable page text plus numbered elements (fixed internal
//!   script; the model never runs JavaScript).
//! * [`detect`]    — login walls, captchas, bot blocks, checkout pages, and what
//!   a click would do.
//! * [`urlpolicy`] — which destinations may be loaded.
//! * [`session`]   — the persisted session state machine.
//! * [`launch`]    — starting Chromium and finding a running one.
//! * [`cdp`], [`http`] — DevTools transport.
//! * [`testkit`]   — a local mock website for tests and evaluation.

pub mod broker;
pub mod cdp;
pub mod config;
pub mod detect;
pub mod error;
pub mod http;
pub mod launch;
pub mod page;
pub mod session;
pub mod snapshot;
pub mod testkit;
pub mod urlpolicy;
pub mod view;

pub use broker::{Broker, ClickExpect, ProfileStatus};
pub use config::{find_chromium, profile_for_origin, BrowserConfig, PROFILES};
pub use detect::{classify_click, ClickKind, PageCondition};
pub use error::{BResult, BrowserError};
pub use session::{SessionRecord, SessionState, SessionStore};
pub use snapshot::{Element, FormField, Snapshot};
pub use view::PageView;
