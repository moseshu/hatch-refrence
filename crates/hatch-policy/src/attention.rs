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
    /// Task this notification is about ('' = not task-linked).
    pub task_id: String,
    /// Kind of task event: 'approval' / 'completed' / 'failed' / ''.
    pub kind: String,
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
        let conn = self.db.conn();
        let mut st = conn.prepare("SELECT ts FROM notifications WHERE delivery = 'immediate'").db()?;
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
        self.notify_for_task("", "", p, text, ref_id, now)
    }

    /// Route and store a notification about a task event. `ref_id` must
    /// name the event (conventionally `task:<id>:<kind>:<detail>`): the
    /// same ref_id twice stores one notification, so a retried worker
    /// cannot spam the user about the same approval, completion or
    /// failure. The task id is recorded on the row so the inbox can
    /// filter per task.
    pub fn notify_for_task(
        &self,
        task_id: &str,
        kind: &str,
        p: Priority,
        text: &str,
        ref_id: &str,
        now: &Timestamp,
    ) -> Result<Delivery> {
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
                "INSERT INTO notifications(id, ts, delivery, priority, text, ref_id, delivered, task_id, kind) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8)",
                params![id, to_rfc3339(now), d.as_str(), p.as_str(), hatch_core::secrets::redact(text), ref_id, task_id, kind],
            )
            .db()?;
        Ok(d)
    }

    /// Undelivered notifications (immediate first, then digest).
    pub fn pending(&self, include_silent: bool) -> Result<Vec<Notification>> {
        self.pending_where(include_silent, None)
    }

    /// Undelivered notifications about one task.
    pub fn pending_for_task(&self, task_id: &str, include_silent: bool) -> Result<Vec<Notification>> {
        self.pending_where(include_silent, Some(task_id))
    }

    fn pending_where(&self, include_silent: bool, task_id: Option<&str>) -> Result<Vec<Notification>> {
        let conn = self.db.conn();
        let mut st = conn.prepare(
                "SELECT id, ts, delivery, priority, text, ref_id, delivered, task_id, kind FROM notifications
                 WHERE delivered = 0 AND (?1 OR delivery != 'silent') AND (?2 IS NULL OR task_id = ?2)
                 ORDER BY CASE delivery WHEN 'immediate' THEN 0 WHEN 'digest' THEN 1 ELSE 2 END,
                          CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, ts",
            )
            .db()?;
        let rows = st
            .query_map(params![include_silent, task_id], |r| {
                Ok(Notification {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    delivery: r.get(2)?,
                    priority: r.get(3)?,
                    text: r.get(4)?,
                    ref_id: r.get(5)?,
                    delivered: r.get::<_, i64>(6)? != 0,
                    task_id: r.get(7)?,
                    kind: r.get(8)?,
                })
            })
            .db()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().db()
    }

    /// All notifications, newest first (delivered included) — the inbox
    /// view. `task_id` narrows to one task.
    pub fn list(&self, task_id: Option<&str>) -> Result<Vec<Notification>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT id, ts, delivery, priority, text, ref_id, delivered, task_id, kind FROM notifications
                 WHERE (?1 IS NULL OR task_id = ?1)
                 ORDER BY ts DESC, id",
            )
            .db()?;
        let rows = st
            .query_map(params![task_id], |r| {
                Ok(Notification {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    delivery: r.get(2)?,
                    priority: r.get(3)?,
                    text: r.get(4)?,
                    ref_id: r.get(5)?,
                    delivered: r.get::<_, i64>(6)? != 0,
                    task_id: r.get(7)?,
                    kind: r.get(8)?,
                })
            })
            .db()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().db()
    }

    /// Mark the given notification ids delivered. Returns how many rows moved.
    pub fn mark_delivered(&self, ids: &[String]) -> Result<usize> {
        let mut n = 0;
        for id in ids {
            n += self
                .db
                .conn()
                .execute("UPDATE notifications SET delivered = 1 WHERE id = ?1 AND delivered = 0", params![id])
                .db()?;
        }
        Ok(n)
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

    #[test]
    fn task_notifications_are_linked_filtered_and_deduped() {
        let db = Db::open_in_memory().unwrap();
        let n = Notifier::new(&db, AttentionPolicy::default());
        let t = parse_rfc3339("2026-11-03T06:00:00Z").unwrap(); // 14:00 Shanghai
        // The same approval twice stores one notification.
        assert_eq!(
            n.notify_for_task("tsk_1", "approval", Priority::High, "任务 A 等待审批", "task:tsk_1:approval:apr_1", &t).unwrap(),
            Delivery::Immediate
        );
        assert_eq!(
            n.notify_for_task("tsk_1", "approval", Priority::High, "任务 A 等待审批", "task:tsk_1:approval:apr_1", &t).unwrap(),
            Delivery::Silent
        );
        n.notify_for_task("tsk_1", "completed", Priority::Normal, "任务 A 已完成", "task:tsk_1:completed", &t).unwrap();
        n.notify_for_task("tsk_2", "failed", Priority::High, "任务 B 失败", "task:tsk_2:failed:x", &t).unwrap();
        let mine = n.pending_for_task("tsk_1", true).unwrap();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|m| m.task_id == "tsk_1"));
        assert_eq!(mine.iter().find(|m| m.kind == "approval").unwrap().text, "任务 A 等待审批");
        // Marking one delivered removes it from the pending list only.
        assert_eq!(n.mark_delivered(&[mine[0].id.clone()]).unwrap(), 1);
        assert_eq!(n.pending_for_task("tsk_1", true).unwrap().len(), 1);
        assert_eq!(n.pending(true).unwrap().len(), 2);
        // A pre-v18 row (no task link) still reads through the new columns.
        db.conn()
            .execute(
                "INSERT INTO notifications(id, ts, delivery, priority, text, ref_id, delivered) VALUES('ntf_old', '2026-11-03T06:00:00Z', 'digest', 'normal', '旧通知', 'old', 0)",
                [],
            )
            .unwrap();
        assert!(n.pending(true).unwrap().iter().any(|m| m.id == "ntf_old" && m.task_id.is_empty() && m.kind.is_empty()));
    }
}
