//! Built-in tools.

pub mod actions;
pub mod calendar;
pub mod mail;
pub mod memory;

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
