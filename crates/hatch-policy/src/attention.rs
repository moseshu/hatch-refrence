//! When to interrupt the user.
//!
//! Interruptions are a budget, not a free resource. Each notification gets a
//! priority; the policy maps it to Immediate / Digest / Silent using quiet
//! hours and a daily cap on immediate interruptions. Everything that is not
//! interrupting goes to the digest (`hatch digest`), so nothing is lost.

use chrono::{Datelike, TimeZone, Timelike};
use chrono_tz::Tz;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use hatch_store::{params, Db, DbResultExt};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

impl Priority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }

    pub fn parse(s: &str) -> Result<Priority> {
        match s {
            "low" => Ok(Priority::Low),
            "normal" => Ok(Priority::Normal),
            "high" => Ok(Priority::High),
            "urgent" => Ok(Priority::Urgent),
            _ => Err(Error::Invalid(format!("unknown priority {s}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Immediate,
    Digest,
    Silent,
}

impl Delivery {
    pub fn as_str(&self) -> &'static str {
        match self {
            Delivery::Immediate => "immediate",
            Delivery::Digest => "digest",
            Delivery::Silent => "silent",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AttentionPolicy {
    /// Local hour quiet time starts (e.g. 22).
    pub quiet_start: u32,
    /// Local hour quiet time ends (e.g. 8).
    pub quiet_end: u32,
    pub daily_budget: u32,
    pub tz: Tz,
}

impl Default for AttentionPolicy {
    fn default() -> Self {
        AttentionPolicy { quiet_start: 22, quiet_end: 8, daily_budget: 5, tz: chrono_tz::Asia::Shanghai }
    }
}

impl AttentionPolicy {
    pub fn is_quiet(&self, now: &Timestamp) -> bool {
        let h = self.tz.from_utc_datetime(&now.naive_utc()).hour();
        if self.quiet_start == self.quiet_end {
            false
        } else if self.quiet_start < self.quiet_end {
            h >= self.quiet_start && h < self.quiet_end
        } else {
            h >= self.quiet_start || h < self.quiet_end
        }
    }

    pub fn route(&self, p: Priority, now: &Timestamp, used_today: u32) -> Delivery {
        let budget_left = used_today < self.daily_budget;
        match p {
            Priority::Low => Delivery::Silent,
            Priority::Normal => Delivery::Digest,
            Priority::High if budget_left && !self.is_quiet(now) => Delivery::Immediate,
            Priority::Urgent if budget_left => Delivery::Immediate,
            _ => Delivery::Digest,
        }
    }

    /// Local calendar date key, e.g. `2026-11-03`.
    pub fn local_day(&self, now: &Timestamp) -> String {
        let l = self.tz.from_utc_datetime(&now.naive_utc());
        format!("{:04}-{:02}-{:02}", l.year(), l.month(), l.day())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Notification {
    pub id: String,
    pub ts: String,
    pub delivery: String,
    pub priority: String,
    pub text: String,
    pub ref_id: String,
    pub delivered: bool,
}

pub struct Notifier<'a> {
    db: &'a Db,
    policy: AttentionPolicy,
}

impl<'a> Notifier<'a> {
    pub fn new(db: &'a Db, policy: AttentionPolicy) -> Self {
        Notifier { db, policy }
    }

    fn immediate_used_today(&self, now: &Timestamp) -> Result<u32> {
        let day = self.policy.local_day(now);
        let mut st = self.db.conn().prepare("SELECT ts FROM notifications WHERE delivery = 'immediate'").db()?;
        let rows = st.query_map([], |r| r.get::<_, String>(0)).db()?;
        let mut n = 0;
        for ts in rows {
            let ts = ts.db()?;
            if let Ok(t) = hatch_core::time::parse_rfc3339(&ts) {
                if self.policy.local_day(&t) == day {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Route and store a notification. `ref_id` makes it idempotent per item.
    pub fn notify(&self, p: Priority, text: &str, ref_id: &str, now: &Timestamp) -> Result<Delivery> {
        let id = ids::stable_id("ntf", &[ref_id]);
        let exists: i64 = self
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM notifications WHERE id = ?1", params![id], |r| r.get(0))
            .db()?;
        if exists > 0 {
            return Ok(Delivery::Silent);
        }
        let d = self.policy.route(p, now, self.immediate_used_today(now)?);
        self.db
            .conn()
            .execute(
                "INSERT INTO notifications(id, ts, delivery, priority, text, ref_id, delivered) VALUES(?1,?2,?3,?4,?5,?6,0)",
                params![id, to_rfc3339(now), d.as_str(), p.as_str(), hatch_core::secrets::redact(text), ref_id],
            )
            .db()?;
        Ok(d)
    }

    /// Undelivered notifications (immediate first, then digest).
    pub fn pending(&self, include_silent: bool) -> Result<Vec<Notification>> {
        let mut st = self
            .db
            .conn()
            .prepare(
                "SELECT id, ts, delivery, priority, text, ref_id, delivered FROM notifications
                 WHERE delivered = 0 AND (?1 OR delivery != 'silent')
                 ORDER BY CASE delivery WHEN 'immediate' THEN 0 WHEN 'digest' THEN 1 ELSE 2 END,
                          CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, ts",
            )
            .db()?;
        let rows = st
            .query_map(params![include_silent], |r| {
                Ok(Notification {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    delivery: r.get(2)?,
                    priority: r.get(3)?,
                    text: r.get(4)?,
                    ref_id: r.get(5)?,
                    delivered: r.get::<_, i64>(6)? != 0,
                })
            })
            .db()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().db()
    }

    pub fn mark_all_delivered(&self) -> Result<usize> {
        self.db.conn().execute("UPDATE notifications SET delivered = 1 WHERE delivered = 0", []).db()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::parse_rfc3339;

    #[test]
    fn routing_rules() {
        let p = AttentionPolicy::default();
        let day = parse_rfc3339("2026-11-03T06:00:00Z").unwrap(); // 14:00 Shanghai
        let night = parse_rfc3339("2026-11-03T15:00:00Z").unwrap(); // 23:00 Shanghai
        assert!(!p.is_quiet(&day));
        assert!(p.is_quiet(&night));
        assert_eq!(p.route(Priority::High, &day, 0), Delivery::Immediate);
        assert_eq!(p.route(Priority::High, &night, 0), Delivery::Digest);
        assert_eq!(p.route(Priority::Urgent, &night, 0), Delivery::Immediate);
        assert_eq!(p.route(Priority::Urgent, &day, 5), Delivery::Digest);
        assert_eq!(p.route(Priority::Normal, &day, 0), Delivery::Digest);
        assert_eq!(p.route(Priority::Low, &day, 0), Delivery::Silent);
    }

    #[test]
    fn budget_is_enforced_and_notifications_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let policy = AttentionPolicy { daily_budget: 2, ..Default::default() };
        let n = Notifier::new(&db, policy);
        let t = parse_rfc3339("2026-11-03T06:00:00Z").unwrap();
        assert_eq!(n.notify(Priority::Urgent, "a", "m1", &t).unwrap(), Delivery::Immediate);
        assert_eq!(n.notify(Priority::Urgent, "b", "m2", &t).unwrap(), Delivery::Immediate);
        assert_eq!(n.notify(Priority::Urgent, "c", "m3", &t).unwrap(), Delivery::Digest);
        assert_eq!(n.notify(Priority::Urgent, "a again", "m1", &t).unwrap(), Delivery::Silent);
        n.notify(Priority::Normal, "验证码 123456", "m4", &t).unwrap();
        let pending = n.pending(false).unwrap();
        assert_eq!(pending.len(), 4);
        assert_eq!(pending[0].delivery, "immediate");
        assert!(pending.iter().all(|p| !p.text.contains("123456")));
        assert_eq!(n.mark_all_delivered().unwrap(), 4);
        assert!(n.pending(true).unwrap().is_empty());
    }
}
