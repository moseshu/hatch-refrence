//! Standard five-field cron expressions: `minute hour day-of-month month day-of-week`.
//!
//! Supported per field: `*`, `N`, `A-B`, `*/S`, `A-B/S` and comma lists.
//! Day-of-week is `0-7` (`0` and `7` are Sunday). As in classic cron, when
//! both day-of-month and day-of-week are restricted a day matches if *either*
//! does. Times are evaluated as wall-clock times in the user's zone and mapped
//! to instants with the DST rules of [`crate::localtime`].

use crate::localtime::{local, resolve};
use chrono::{Datelike, Duration, NaiveDate, Timelike};
use chrono_tz::Tz;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};

/// Five years of days: long enough for `0 0 29 2 *`.
const MAX_DAYS: i64 = 366 * 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64,
    hours: u64,
    dom: u64,
    months: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
    source: String,
}

fn parse_field(field: &str, lo: u32, hi: u32, name: &str) -> Result<(u64, bool)> {
    let bad = |why: &str| Error::Invalid(format!("cron {name} field {field:?}: {why}"));
    let mut bits = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| bad("bad step"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(bad("step must be positive"));
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| bad("bad range"))?, b.parse().map_err(|_| bad("bad range"))?)
        } else {
            let v: u32 = range.parse().map_err(|_| bad("not a number"))?;
            // `N/S` means "from N to the end, every S".
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(bad(&format!("out of range {lo}-{hi}")));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, field == "*"))
}

impl CronExpr {
    pub fn parse(s: &str) -> Result<CronExpr> {
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return Err(Error::Invalid(format!("cron needs 5 fields, got {}: {s:?}", f.len())));
        }
        let (minutes, _) = parse_field(f[0], 0, 59, "minute")?;
        let (hours, _) = parse_field(f[1], 0, 23, "hour")?;
        let (dom, dom_any) = parse_field(f[2], 1, 31, "day-of-month")?;
        let (months, _) = parse_field(f[3], 1, 12, "month")?;
        let (mut dow, dow_any) = parse_field(f[4], 0, 7, "day-of-week")?;
        if dow & (1 << 7) != 0 {
            dow |= 1; // 7 is Sunday too
        }
        Ok(CronExpr { minutes, hours, dom, months, dow, dom_any, dow_any, source: f.join(" ") })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        if self.months & (1 << d.month()) == 0 {
            return false;
        }
        let dom = self.dom & (1 << d.day()) != 0;
        let dow = self.dow & (1 << d.weekday().num_days_from_sunday()) != 0;
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// First firing strictly after `after`.
    pub fn next_after(&self, after: &Timestamp, tz: &Tz) -> Option<Timestamp> {
        let start = local(tz, after).with_second(0)?.with_nanosecond(0)?;
        for day in 0..MAX_DAYS {
            let date = start.date() + Duration::days(day);
            if !self.day_matches(date) {
                continue;
            }
            for h in (0..24).filter(|h| self.hours & (1 << h) != 0) {
                for m in (0..60).filter(|m| self.minutes & (1 << m) != 0) {
                    let naive = date.and_hms_opt(h, m, 0)?;
                    if naive < start {
                        continue;
                    }
                    let t = resolve(tz, naive);
                    if t > *after {
                        return Some(t);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::parse_rfc3339;

    fn next(expr: &str, after: &str, tz: Tz) -> String {
        CronExpr::parse(expr).unwrap().next_after(&parse_rfc3339(after).unwrap(), &tz).unwrap().to_rfc3339()
    }

    #[test]
    fn fields_and_lists() {
        let sh = chrono_tz::Asia::Shanghai;
        // 08:30 Shanghai = 00:30 UTC.
        assert_eq!(next("30 8 * * *", "2026-11-03T00:00:00Z", sh), "2026-11-03T00:30:00+00:00");
        assert_eq!(next("30 8 * * *", "2026-11-03T00:30:00Z", sh), "2026-11-04T00:30:00+00:00");
        // Every 15 minutes.
        assert_eq!(next("*/15 * * * *", "2026-11-03T00:16:00Z", sh), "2026-11-03T00:30:00+00:00");
        // Weekdays at 09:00 — 2026-11-07 is a Saturday, so the next is Monday 11-09.
        assert_eq!(next("0 9 * * 1-5", "2026-11-06T02:00:00Z", sh), "2026-11-09T01:00:00+00:00");
        // Sunday as 7.
        assert_eq!(next("0 9 * * 7", "2026-11-03T00:00:00Z", sh), "2026-11-08T01:00:00+00:00");
        // Leap day.
        assert_eq!(next("0 0 29 2 *", "2026-03-01T00:00:00Z", chrono_tz::UTC), "2028-02-29T00:00:00+00:00");
    }

    #[test]
    fn dom_or_dow_when_both_restricted() {
        // The 13th, or any Friday: after Tue 2026-11-03 the next is Fri 11-06.
        assert_eq!(next("0 12 13 * 5", "2026-11-03T00:00:00Z", chrono_tz::UTC), "2026-11-06T12:00:00+00:00");
    }

    #[test]
    fn dst_fall_back_fires_once() {
        let ny = chrono_tz::America::New_York;
        // 01:30 on 2026-11-01 happens twice; fire only at the first (05:30 UTC).
        assert_eq!(next("30 1 * * *", "2026-11-01T00:00:00Z", ny), "2026-11-01T05:30:00+00:00");
        // From inside the repeated hour the next firing is the following day.
        assert_eq!(next("30 1 * * *", "2026-11-01T05:30:00Z", ny), "2026-11-02T06:30:00+00:00");
        assert_eq!(next("30 1 * * *", "2026-11-01T06:15:00Z", ny), "2026-11-02T06:30:00+00:00");
    }

    #[test]
    fn rejects_bad_expressions() {
        for bad in ["* * * *", "60 * * * *", "* 24 * * *", "*/0 * * * *", "5-1 * * * *", "a * * * *"] {
            assert!(CronExpr::parse(bad).is_err(), "{bad}");
        }
    }
}
