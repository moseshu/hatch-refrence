//! Mapping wall-clock times to instants across DST transitions.
//!
//! * A wall time that occurs twice (clocks fall back) maps to its **earlier**
//!   instance, so a daily job fires once, not twice.
//! * A wall time that does not exist (clocks spring forward) is moved forward
//!   to the first wall time that exists after the gap (02:30 → 03:00 when
//!   clocks jump from 02:00 to 03:00).

use chrono::{Duration, LocalResult, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use hatch_core::time::Timestamp;

/// Longest DST gap we search across (no real zone exceeds this).
const MAX_GAP_MINUTES: i64 = 180;

pub fn resolve(tz: &Tz, naive: NaiveDateTime) -> Timestamp {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => t.with_timezone(&chrono::Utc),
        LocalResult::Ambiguous(early, _) => early.with_timezone(&chrono::Utc),
        LocalResult::None => {
            for m in 1..=MAX_GAP_MINUTES {
                let probe = naive + Duration::minutes(m);
                match tz.from_local_datetime(&probe) {
                    LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => return t.with_timezone(&chrono::Utc),
                    LocalResult::None => continue,
                }
            }
            // Unreachable for real zones; fall back to reading the wall time as UTC.
            chrono::Utc.from_utc_datetime(&naive)
        }
    }
}

/// Wall-clock time of `ts` in `tz`.
pub fn local(tz: &Tz, ts: &Timestamp) -> NaiveDateTime {
    ts.with_timezone(tz).naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    #[test]
    fn gap_moves_forward_and_overlap_takes_the_earlier_instant() {
        let ny = chrono_tz::America::New_York;
        // 2026-03-08 02:30 does not exist in New York (02:00 → 03:00 EDT = 07:00 UTC).
        assert_eq!(resolve(&ny, at(2026, 3, 8, 2, 30)).to_rfc3339(), "2026-03-08T07:00:00+00:00");
        // 2026-11-01 01:30 happens twice; the first is EDT (05:30 UTC).
        assert_eq!(resolve(&ny, at(2026, 11, 1, 1, 30)).to_rfc3339(), "2026-11-01T05:30:00+00:00");
        let sh = chrono_tz::Asia::Shanghai;
        assert_eq!(resolve(&sh, at(2026, 11, 3, 8, 30)).to_rfc3339(), "2026-11-03T00:30:00+00:00");
    }
}
