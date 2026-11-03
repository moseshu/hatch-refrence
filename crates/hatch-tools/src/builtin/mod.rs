//! Built-in tools.

pub mod actions;
pub mod browser;
pub mod calendar;
pub mod fs_read;
pub mod fs_write;
pub mod mail;
pub mod memory;
pub mod shell;
pub mod skills;
pub mod web;

use crate::tool::Registry;
use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use std::sync::Arc;

/// The default tool set offered to the chat agent.
pub fn default_registry() -> Registry {
    Registry::new()
        .with(Arc::new(memory::MemorySearch))
        .with(Arc::new(memory::MemoryAdd))
        .with(Arc::new(memory::MemoryExplain))
        .with(Arc::new(calendar::CalendarList))
        .with(Arc::new(calendar::CalendarFreeSlots))
        .with(Arc::new(mail::MailSearch))
        .with(Arc::new(actions::ProposeEmail))
        .with(Arc::new(actions::ProposeEvent))
}

/// Options for the general-purpose tools.
#[derive(Clone, Debug)]
pub struct GeneralTools {
    /// File and shell tools are only offered when a workspace is configured.
    pub workspace: Option<crate::workspace::Workspace>,
    pub web_fetch: bool,
    /// Display name of the configured search backend, if any.
    pub search_backend: Option<String>,
    pub max_command_timeout_secs: u64,
}

/// Add file, shell and web tools to `reg` according to `opts`.
pub fn add_general_tools(mut reg: Registry, opts: &GeneralTools) -> Registry {
    if let Some(ws) = &opts.workspace {
        reg = reg
            .with(Arc::new(fs_read::FsRead { ws: ws.clone() }))
            .with(Arc::new(fs_read::FsList { ws: ws.clone() }))
            .with(Arc::new(fs_read::FsGlob { ws: ws.clone() }))
            .with(Arc::new(fs_read::FsGrep { ws: ws.clone() }))
            .with(Arc::new(fs_write::FsWrite { ws: ws.clone() }))
            .with(Arc::new(fs_write::FsEdit { ws: ws.clone() }))
            .with(Arc::new(shell::ShellRun { ws: ws.clone(), max_timeout_secs: opts.max_command_timeout_secs }));
    }
    if opts.web_fetch {
        reg = reg.with(Arc::new(web::WebFetch));
    }
    if let Some(name) = &opts.search_backend {
        reg = reg.with(Arc::new(web::WebSearch { backend_name: name.clone() }));
    }
    reg.with(Arc::new(web::ApprovalStatusTool))
}

/// Accept RFC3339 or local `YYYY-MM-DD HH:MM` / `YYYY-MM-DDTHH:MM`.
pub fn parse_user_time(s: &str, tz: &Tz) -> Result<Timestamp> {
    if let Ok(t) = parse_rfc3339(s) {
        return Ok(t);
    }
    for f in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s.trim(), f) {
            if let Some(t) = tz.from_local_datetime(&n).earliest() {
                return Ok(t.with_timezone(&chrono::Utc));
            }
        }
    }
    Err(Error::Invalid(format!("cannot parse time {s:?}; use RFC3339 or 'YYYY-MM-DD HH:MM'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let tz = chrono_tz::Asia::Shanghai;
        let a = parse_user_time("2026-11-04 15:00", &tz).unwrap();
        let b = parse_user_time("2026-11-04T07:00:00Z", &tz).unwrap();
        assert_eq!(a, b);
        assert!(parse_user_time("明天", &tz).is_err());
        assert_eq!(default_registry().names().len(), 8);
    }
}
