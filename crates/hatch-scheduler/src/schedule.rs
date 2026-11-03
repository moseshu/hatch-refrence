//! Schedule expressions.
//!
//! | Form | Meaning |
//! |---|---|
//! | `daily HH:MM` | every day at a wall-clock time |
//! | `weekly MON,WED HH:MM` | on the listed weekdays |
//! | `every 30m` / `every 2h` | fixed interval from the job's anchor (≥ 5 minutes) |
//! | `once 2026-11-05T09:00:00+08:00` | a single run |
//! | `cron M H DOM MON DOW` (or five bare fields) | classic cron |
//!
//! Wall-clock forms are evaluated in the user's zone with the DST rules of
//! [`crate::localtime`]; intervals are measured in real elapsed time.

use crate::cron::CronExpr;
use crate::localtime::{local, resolve};
use chrono::{Datelike, Duration, NaiveTime, Weekday};
use chrono_tz::Tz;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use std::fmt;

/// Shortest allowed interval for `every`.
pub const MIN_INTERVAL_MINUTES: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    Daily { at: NaiveTime },
    Weekly { days: Vec<Weekday>, at: NaiveTime },
    Every { minutes: u32 },
    Once { at: Timestamp },
    Cron(CronExpr),
}

const DAY_NAMES: [(&str, Weekday); 7] = [
    ("mon", Weekday::Mon),
    ("tue", Weekday::Tue),
    ("wed", Weekday::Wed),
    ("thu", Weekday::Thu),
    ("fri", Weekday::Fri),
    ("sat", Weekday::Sat),
    ("sun", Weekday::Sun),
];

fn parse_time(s: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| Error::Invalid(format!("time must be HH:MM, got {s:?}")))
}

fn parse_days(s: &str) -> Result<Vec<Weekday>> {
    let mut days = Vec::new();
    for d in s.split(',').map(|d| d.trim().to_ascii_lowercase()) {
        let w = DAY_NAMES
            .iter()
            .find(|(n, _)| d.starts_with(n))
            .map(|(_, w)| *w)
            .ok_or_else(|| Error::Invalid(format!("unknown weekday {d:?} (use MON..SUN)")))?;
        if !days.contains(&w) {
            days.push(w);
        }
    }
    days.sort_by_key(|w| w.num_days_from_monday());
    Ok(days)
}

fn parse_interval(s: &str) -> Result<u32> {
    let bad = || Error::Invalid(format!("interval must look like 30m or 2h, got {s:?}"));
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?);
    let n: u32 = num.parse().map_err(|_| bad())?;
    let minutes = match unit {
        "m" | "min" => n,
        "h" => n.checked_mul(60).ok_or_else(bad)?,
        _ => return Err(bad()),
    };
    if minutes < MIN_INTERVAL_MINUTES {
        return Err(Error::Invalid(format!("interval must be at least {MIN_INTERVAL_MINUTES} minutes")));
    }
    Ok(minutes)
}

impl Schedule {
    pub fn parse(s: &str) -> Result<Schedule> {
        let s = s.trim();
        let (head, rest) = s.split_once(char::is_whitespace).map(|(h, r)| (h, r.trim())).unwrap_or((s, ""));
        match head.to_ascii_lowercase().as_str() {
            "daily" => Ok(Schedule::Daily { at: parse_time(rest)? }),
            "weekly" => {
                let (days, time) = rest
                    .rsplit_once(char::is_whitespace)
                    .ok_or_else(|| Error::Invalid("weekly needs days and a time: `weekly MON,WED 09:00`".into()))?;
                Ok(Schedule::Weekly { days: parse_days(days.trim())?, at: parse_time(time)? })
            }
            "every" => Ok(Schedule::Every { minutes: parse_interval(rest)? }),
            "once" => Ok(Schedule::Once { at: parse_rfc3339(rest)? }),
            "cron" => Ok(Schedule::Cron(CronExpr::parse(rest)?)),
            _ if s.split_whitespace().count() == 5 => Ok(Schedule::Cron(CronExpr::parse(s)?)),
            _ => Err(Error::Invalid(format!(
                "unknown schedule {s:?}; use `daily 08:30`, `weekly MON,FRI 09:00`, `every 30m`, `once <RFC3339>` or `cron <5 fields>`"
            ))),
        }
    }

    /// First firing strictly after `after`. `anchor` is where `every`
    /// intervals are counted from (the job's creation or re-enable time).
    pub fn next_after(&self, after: &Timestamp, anchor: &Timestamp, tz: &Tz) -> Option<Timestamp> {
        match self {
            Schedule::Daily { at } => self.next_wall(after, tz, *at, |_| true),
            Schedule::Weekly { days, at } => self.next_wall(after, tz, *at, |w| days.contains(&w)),
            Schedule::Every { minutes } => {
                let step = Duration::minutes(i64::from(*minutes));
                if after < anchor {
                    return Some(*anchor);
                }
                let k = (*after - *anchor).num_seconds() / step.num_seconds() + 1;
                Some(*anchor + step * k as i32)
            }
            Schedule::Once { at } => (at > after).then_some(*at),
            Schedule::Cron(c) => c.next_after(after, tz),
        }
    }

    fn next_wall(&self, after: &Timestamp, tz: &Tz, at: NaiveTime, day_ok: impl Fn(Weekday) -> bool) -> Option<Timestamp> {
        let today = local(tz, after).date();
        (0..=8).map(|d| today + Duration::days(d)).filter(|d| day_ok(d.weekday())).find_map(|d| {
            let t = resolve(tz, d.and_time(at));
            (t > *after).then_some(t)
        })
    }

    /// All firings in `[from, to]`, starting with `from` itself (assumed to be
    /// a firing). Capped at `cap`; the bool says whether the cap was hit.
    pub fn firings_between(&self, from: &Timestamp, to: &Timestamp, anchor: &Timestamp, tz: &Tz, cap: usize) -> (Vec<Timestamp>, bool) {
        let mut out = Vec::new();
        let mut t = *from;
        while t <= *to {
            if out.len() == cap {
                return (out, true);
            }
            out.push(t);
            match self.next_after(&t, anchor, tz) {
                Some(n) => t = n,
                None => break,
            }
        }
        (out, false)
    }

    pub fn is_once(&self) -> bool {
        matches!(self, Schedule::Once { .. })
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Schedule::Daily { at } => write!(f, "daily {}", at.format("%H:%M")),
            Schedule::Weekly { days, at } => {
                let names: Vec<String> = days
                    .iter()
                    .map(|w| DAY_NAMES.iter().find(|(_, d)| d == w).map(|(n, _)| n.to_ascii_uppercase()).unwrap_or_default())
                    .collect();
                write!(f, "weekly {} {}", names.join(","), at.format("%H:%M"))
            }
            Schedule::Every { minutes } if minutes % 60 == 0 => write!(f, "every {}h", minutes / 60),
            Schedule::Every { minutes } => write!(f, "every {minutes}m"),
            Schedule::Once { at } => write!(f, "once {}", to_rfc3339(at)),
            Schedule::Cron(c) => write!(f, "cron {}", c.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        parse_rfc3339(s).unwrap()
    }

    fn next(s: &str, after: &str, tz: Tz) -> String {
        let anchor = ts("2026-11-03T00:00:00Z");
        Schedule::parse(s).unwrap().next_after(&ts(after), &anchor, &tz).map(|t| t.to_rfc3339()).unwrap_or_default()
    }

    #[test]
    fn parse_and_display_round_trip() {
        for s in ["daily 08:30", "weekly MON,WED 09:00", "every 30m", "every 2h", "once 2026-11-05T01:00:00Z", "cron 0 9 * * 1-5"] {
            assert_eq!(Schedule::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(Schedule::parse("0 9 * * 1-5").unwrap().to_string(), "cron 0 9 * * 1-5");
        assert_eq!(Schedule::parse("weekly wed,mon 09:00").unwrap().to_string(), "weekly MON,WED 09:00");
        for bad in ["every 1m", "every 10s", "daily 25:00", "weekly XYZ 09:00", "hourly", "once tomorrow"] {
            assert!(Schedule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shanghai_daily_and_weekly() {
        let sh = chrono_tz::Asia::Shanghai;
        assert_eq!(next("daily 08:30", "2026-11-03T00:00:00Z", sh), "2026-11-03T00:30:00+00:00");
        assert_eq!(next("daily 08:30", "2026-11-03T00:30:00Z", sh), "2026-11-04T00:30:00+00:00");
        // 2026-11-03 is a Tuesday: next MON/WED 09:00 is Wednesday.
        assert_eq!(next("weekly MON,WED 09:00", "2026-11-03T02:00:00Z", sh), "2026-11-04T01:00:00+00:00");
    }

    #[test]
    fn interval_counts_from_anchor_in_real_time() {
        let sh = chrono_tz::Asia::Shanghai;
        assert_eq!(next("every 30m", "2026-11-03T00:10:00Z", sh), "2026-11-03T00:30:00+00:00");
        assert_eq!(next("every 30m", "2026-11-03T00:30:00Z", sh), "2026-11-03T01:00:00+00:00");
        assert_eq!(next("every 2h", "2026-11-02T00:00:00Z", sh), "2026-11-03T00:00:00+00:00");
        // Across the New York fall-back the interval stays 30 real minutes.
        let ny = chrono_tz::America::New_York;
        assert_eq!(next("every 30m", "2026-11-03T05:45:00Z", ny), "2026-11-03T06:00:00+00:00");
    }

    #[test]
    fn dst_daily_fires_once_on_fall_back_and_shifts_on_spring_forward() {
        let ny = chrono_tz::America::New_York;
        let s = Schedule::parse("daily 01:30").unwrap();
        let anchor = ts("2026-10-30T00:00:00Z");
        let (fired, capped) = s.firings_between(&ts("2026-10-31T05:30:00Z"), &ts("2026-11-02T23:00:00Z"), &anchor, &ny, 100);
        assert!(!capped);
        let got: Vec<String> = fired.iter().map(|t| t.to_rfc3339()).collect();
        // Oct 31 (EDT), Nov 1 once (first 01:30, EDT), Nov 2 (EST).
        assert_eq!(got, ["2026-10-31T05:30:00+00:00", "2026-11-01T05:30:00+00:00", "2026-11-02T06:30:00+00:00"]);
        // 02:30 does not exist on 2026-03-08: it runs at 03:00 EDT (07:00 UTC).
        assert_eq!(next("daily 02:30", "2026-03-08T00:00:00Z", ny), "2026-03-08T07:00:00+00:00");
        assert_eq!(next("daily 02:30", "2026-03-08T07:00:00Z", ny), "2026-03-09T06:30:00+00:00");
    }

    #[test]
    fn once_fires_a_single_time() {
        let sh = chrono_tz::Asia::Shanghai;
        assert_eq!(next("once 2026-11-05T01:00:00Z", "2026-11-03T00:00:00Z", sh), "2026-11-05T01:00:00+00:00");
        assert_eq!(next("once 2026-11-05T01:00:00Z", "2026-11-05T01:00:00Z", sh), "");
    }
}
