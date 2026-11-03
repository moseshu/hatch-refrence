//! Time helpers. All timestamps are stored as RFC3339 UTC strings.

use crate::{Error, Result};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;

pub type Timestamp = DateTime<Utc>;

pub fn now() -> Timestamp {
    Utc::now()
}

pub fn to_rfc3339(ts: &Timestamp) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn parse_rfc3339(s: &str) -> Result<Timestamp> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| Error::Invalid(format!("bad timestamp {s:?}: {e}")))
}

pub fn parse_tz(name: &str) -> Result<Tz> {
    name.parse::<Tz>()
        .map_err(|_| Error::Config(format!("unknown timezone {name:?}")))
}

/// A wall-clock time in `tz` as an instant (earlier instance when ambiguous;
/// read as UTC in the rare case it falls in a DST gap).
pub fn local_to_utc(tz: &Tz, naive: chrono::NaiveDateTime) -> Timestamp {
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

/// Human-readable local time, e.g. `2026-11-03 14:00 (周二)`.
pub fn fmt_local(ts: &Timestamp, tz: &Tz) -> String {
    let local = tz.from_utc_datetime(&ts.naive_utc());
    let wd = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
        [chrono::Datelike::weekday(&local).num_days_from_monday() as usize];
    format!("{} ({wd})", local.format("%Y-%m-%d %H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_roundtrip() {
        let t = parse_rfc3339("2026-11-03T06:00:00Z").unwrap();
        assert_eq!(to_rfc3339(&t), "2026-11-03T06:00:00Z");
        let t2 = parse_rfc3339("2026-11-03T14:00:00+08:00").unwrap();
        assert_eq!(t, t2);
        assert!(parse_rfc3339("yesterday").is_err());
    }

    #[test]
    fn local_formatting() {
        let tz = parse_tz("Asia/Shanghai").unwrap();
        let t = parse_rfc3339("2026-11-03T06:00:00Z").unwrap();
        assert_eq!(fmt_local(&t, &tz), "2026-11-03 14:00 (周二)");
        assert!(parse_tz("Mars/Base").is_err());
    }
}
