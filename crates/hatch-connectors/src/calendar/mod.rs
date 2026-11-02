// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Calendar: events, local store, conflicts and free slots.

pub mod ics;

use chrono::{Duration, NaiveTime, TimeZone};
use chrono_tz::Tz;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, rusqlite, Db, DbResultExt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub uid: String,
    pub title: String,
    pub starts_at: Timestamp,
    pub ends_at: Timestamp,
    pub location: String,
    pub source: String,
}

impl Event {
    pub fn overlaps(&self, start: &Timestamp, end: &Timestamp) -> bool {
        self.starts_at < *end && self.ends_at > *start
    }
}

const COLS: &str = "uid, title, starts_at, ends_at, location, source";

fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    let s: String = r.get(2)?;
    let e: String = r.get(3)?;
    Ok(Event {
        uid: r.get(0)?,
        title: r.get(1)?,
        starts_at: parse_rfc3339(&s).unwrap_or_default(),
        ends_at: parse_rfc3339(&e).unwrap_or_default(),
        location: r.get(4)?,
        source: r.get(5)?,
    })
}

pub struct CalendarRepo<'a> {
    db: &'a Db,
}

impl<'a> CalendarRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        CalendarRepo { db }
    }

    pub fn upsert(&self, e: &Event) -> Result<()> {
        if e.ends_at <= e.starts_at {
            return Err(Error::Invalid(format!("event {} ends before it starts", e.uid)));
        }
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO calendar_events({COLS}) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(uid) DO UPDATE SET title=excluded.title, starts_at=excluded.starts_at,
                 ends_at=excluded.ends_at, location=excluded.location, source=excluded.source"),
                params![e.uid, e.title, to_rfc3339(&e.starts_at), to_rfc3339(&e.ends_at), e.location, e.source],
            )
            .db()?;
        Ok(())
    }

    pub fn delete(&self, uid: &str) -> Result<bool> {
        Ok(self.db.conn().execute("DELETE FROM calendar_events WHERE uid = ?1", params![uid]).db()? > 0)
    }

    /// Events overlapping `[start, end)`, ordered by start.
    pub fn between(&self, start: &Timestamp, end: &Timestamp) -> Result<Vec<Event>> {
        let mut st = self
            .db
            .conn()
            .prepare(&format!(
                "SELECT {COLS} FROM calendar_events WHERE starts_at < ?2 AND ends_at > ?1 ORDER BY starts_at"
            ))
            .db()?;
        let rows = st.query_map(params![to_rfc3339(start), to_rfc3339(end)], row_to_event).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn conflicts(&self, start: &Timestamp, end: &Timestamp) -> Result<Vec<Event>> {
        self.between(start, end)
    }

    /// Up to `limit` free slots of `duration` inside local working hours,
    /// on half-hour boundaries, between `from` and `until`.
    pub fn free_slots(
        &self,
        from: &Timestamp,
        until: &Timestamp,
        duration: Duration,
        work_hours: (u32, u32),
        tz: &Tz,
        limit: usize,
    ) -> Result<Vec<(Timestamp, Timestamp)>> {
        let busy = self.between(from, until)?;
        let mut out = Vec::new();
        let mut day = tz.from_utc_datetime(&from.naive_utc()).date_naive();
        let last_day = tz.from_utc_datetime(&until.naive_utc()).date_naive();
        while day <= last_day && out.len() < limit {
            let open = NaiveTime::from_hms_opt(work_hours.0, 0, 0).unwrap_or_default();
            let close = NaiveTime::from_hms_opt(work_hours.1, 0, 0).unwrap_or_default();
            let (Some(start), Some(stop)) = (
                tz.from_local_datetime(&day.and_time(open)).earliest(),
                tz.from_local_datetime(&day.and_time(close)).earliest(),
            ) else {
                day = day.succ_opt().unwrap_or(day);
                continue;
            };
            let mut t = start.with_timezone(&chrono::Utc);
            let stop = stop.with_timezone(&chrono::Utc);
            while t + duration <= stop && out.len() < limit {
                let end = t + duration;
                if t >= *from && end <= *until && !busy.iter().any(|e| e.overlaps(&t, &end)) {
                    out.push((t, end));
                    t = end;
                } else {
                    t += Duration::minutes(30);
                }
            }
            match day.succ_opt() {
                Some(d) => day = d,
                None => break,
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(uid: &str, s: &str, e: &str) -> Event {
        Event {
            uid: uid.into(),
            title: uid.into(),
            starts_at: parse_rfc3339(s).unwrap(),
            ends_at: parse_rfc3339(e).unwrap(),
            location: String::new(),
            source: "test".into(),
        }
    }

    #[test]
    fn conflicts_and_free_slots() {
        let db = Db::open_in_memory().unwrap();
        let c = CalendarRepo::new(&db);
        let tz: Tz = "Asia/Shanghai".parse().unwrap();
        // 10:00-11:00 and 14:00-16:00 Shanghai on 2026-11-04.
        c.upsert(&ev("a", "2026-11-04T02:00:00Z", "2026-11-04T03:00:00Z")).unwrap();
        c.upsert(&ev("b", "2026-11-04T06:00:00Z", "2026-11-04T08:00:00Z")).unwrap();
        let s = parse_rfc3339("2026-11-04T06:30:00Z").unwrap();
        assert_eq!(c.conflicts(&s, &(s + Duration::hours(1))).unwrap()[0].uid, "b");
        let s = parse_rfc3339("2026-11-04T03:00:00Z").unwrap();
        assert!(c.conflicts(&s, &(s + Duration::hours(1))).unwrap().is_empty(), "back-to-back is not a conflict");
        let from = parse_rfc3339("2026-11-04T00:00:00Z").unwrap();
        let until = parse_rfc3339("2026-11-04T12:00:00Z").unwrap();
        let slots = c.free_slots(&from, &until, Duration::hours(1), (9, 18), &tz, 10).unwrap();
        let starts: Vec<String> = slots.iter().map(|(s, _)| hatch_core::time::fmt_local(s, &tz)).collect();
        assert_eq!(starts[0], "2026-11-04 09:00 (周三)");
        assert_eq!(starts[1], "2026-11-04 11:00 (周三)");
        assert!(starts.iter().all(|s| !s.contains("14:") && !s.contains("15:")));
        assert!(c.upsert(&ev("bad", "2026-11-04T02:00:00Z", "2026-11-04T01:00:00Z")).is_err());
        assert!(c.delete("a").unwrap());
    }
}
