//! Browser tools.
//!
//! | tool | class |
//! |---|---|
//! | `browser_open` | `fetch_web` (a new destination; approval when the turn is tainted) |
//! | `browser_read` / `find` / `scroll` / `screenshot` / `back` / `close` | `browse_local` |
//! | `browser_type` / `browser_select` | `browse_local` (nothing is submitted; passwords refused) |
//! | `browser_click` | decided per element by code: link → `fetch_web`, page widget → `browse_local`, anything that may submit → `browser_submit` (approval), payment → `payment` (suggest only) |
//! | `browser_download_enable` / `browser_downloads` | `browse_local` (configures where this profile's downloads land; files stay inside the home) |
//!
//! Page content is untrusted. Sessions belong to the conversation that
//! opened them.

pub mod act;
pub mod look;

use crate::tool::{arg_str, Registry, ToolContext};
use hatch_browser::{Broker, BrowserError, PageView, SessionStore};
use hatch_core::{Error, Result};
use hatch_policy::{AttentionPolicy, Notifier, Priority};
use serde_json::Value;
use std::sync::Arc;

pub use act::{BrowserClick, BrowserSelect, BrowserType};
pub use look::{BrowserBack, BrowserClose, BrowserDownloadEnable, BrowserDownloads, BrowserFind, BrowserOpen, BrowserRead, BrowserScreenshot, BrowserScroll};

/// Read-only browser tools (for sub-agent types that may browse but not act).
pub const BROWSER_READ_TOOLS: &[&str] =
    &["browser_open", "browser_read", "browser_find", "browser_scroll", "browser_screenshot", "browser_back", "browser_close", "browser_download_enable", "browser_downloads"];

/// Every browser tool name.
pub const BROWSER_TOOLS: &[&str] = &[
    "browser_open",
    "browser_read",
    "browser_find",
    "browser_click",
    "browser_type",
    "browser_select",
    "browser_scroll",
    "browser_screenshot",
    "browser_back",
    "browser_close",
    "browser_download_enable",
    "browser_downloads",
];

/// Add the browser tools backed by `broker`.
pub fn add_browser_tools(reg: Registry, broker: Arc<Broker>) -> Registry {
    let b = || broker.clone();
    reg.with(Arc::new(BrowserOpen { broker: b() }))
        .with(Arc::new(BrowserRead { broker: b() }))
        .with(Arc::new(BrowserFind { broker: b() }))
        .with(Arc::new(BrowserClick { broker: b() }))
        .with(Arc::new(BrowserType { broker: b() }))
        .with(Arc::new(BrowserSelect { broker: b() }))
        .with(Arc::new(BrowserScroll { broker: b() }))
        .with(Arc::new(BrowserScreenshot { broker: b() }))
        .with(Arc::new(BrowserBack { broker: b() }))
        .with(Arc::new(BrowserClose { broker: b() }))
        .with(Arc::new(BrowserDownloadEnable { broker: b() }))
        .with(Arc::new(BrowserDownloads { broker: b() }))
}

pub(crate) fn err(e: BrowserError) -> Error {
    Error::from(e)
}

/// The `session` argument, checked to belong to this conversation.
pub(crate) fn own_session<'v>(args: &'v Value, ctx: &ToolContext) -> Result<&'v str> {
    let id = arg_str(args, "session")?;
    check_owner(id, ctx.db, &ctx.origin)?;
    Ok(id)
}

pub(crate) fn check_owner(id: &str, db: &hatch_store::Db, origin: &str) -> Result<()> {
    let rec = SessionStore::new(db).get(id).map_err(err)?;
    if rec.origin != origin {
        return Err(Error::Denied(format!("浏览器会话 {id} 属于另一个对话，不能在这里使用")));
    }
    Ok(())
}

pub(crate) fn ref_arg(args: &Value) -> Result<u32> {
    args.get("ref")
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('[').trim_end_matches(']').parse().ok())))
        .map(|n| n as u32)
        .ok_or_else(|| Error::Invalid("missing element number `ref` (from browser_read)".into()))
}

/// Tell the user when a session needs them (login / captcha).
pub(crate) fn notify_if_needed(db: &hatch_store::Db, tz: chrono_tz::Tz, view: &PageView, now: &hatch_core::time::Timestamp) {
    if view.needs_user() {
        let text = format!("浏览器会话 {} 需要你接管（{}）：运行 `hatch browser takeover {}`", view.session, view.snapshot.url, view.session);
        let _ = Notifier::new(db, AttentionPolicy { tz, ..Default::default() }).notify(Priority::High, &text, &format!("browser-needs-user:{}:{}", view.session, view.snapshot.url), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refs_and_tool_lists() {
        assert_eq!(ref_arg(&json!({"ref": 3})).unwrap(), 3);
        assert_eq!(ref_arg(&json!({"ref": "[7]"})).unwrap(), 7);
        assert!(ref_arg(&json!({})).is_err());
        assert!(BROWSER_READ_TOOLS.iter().all(|t| BROWSER_TOOLS.contains(t)));
        assert!(!BROWSER_READ_TOOLS.contains(&"browser_click") && !BROWSER_READ_TOOLS.contains(&"browser_type"));
    }
}
