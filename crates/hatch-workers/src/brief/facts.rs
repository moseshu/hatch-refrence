//! Facts a brief may mention — gathered by code from the user's own data.
//!
//! The model never adds information: it only orders and phrases these facts,
//! and every item it writes must cite them by id (see `compose`).

use chrono::{Duration, NaiveTime};
use chrono_tz::Tz;
use hatch_connectors::{CalendarRepo, Event};
use hatch_core::time::{fmt_local, parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::Result;
use hatch_policy::{ApprovalQueue, ApprovalStatus, AttentionPolicy, Notifier};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Mail,
    Event,
    Conflict,
    Approval,
    Reminder,
}

impl FactKind {
    pub fn label(&self) -> &'static str {
        match self {
            FactKind::Mail => "邮件",
            FactKind::Event => "日程",
            FactKind::Conflict => "冲突",
            FactKind::Approval => "待审批",
            FactKind::Reminder => "提醒",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fact {
    /// `F1`, `F2`, … — what the model cites.
    pub id: String,
    pub kind: FactKind,
    pub text: String,
    /// Where it came from: `mail:<id>`, `event:<uid>`, `approval:<id>`, `notification:<id>`.
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Gathered {
    pub facts: Vec<Fact>,
    /// Coverage gaps, stated by code (never by the model).
    pub gaps: Vec<String>,
}

/// How far back mail is considered.
const MAIL_WINDOW_HOURS: i64 = 36;
const MAX_REMINDERS: usize = 10;

struct MailRow {
    id: String,
    category: String,
    priority: String,
    summary: String,
    suspicious: bool,
    from: String,
    subject: String,
}

fn mails(db: &Db, since: &Timestamp) -> Result<Vec<MailRow>> {
    let c = db.conn();
    let mut st = c
        .prepare(
            "SELECT t.mail_id, t.category, t.priority, t.summary, t.suspicious, m.from_name, m.from_addr, m.subject
             FROM triage_results t JOIN mail_messages m ON m.id = t.mail_id
             WHERE t.triaged_at >= ?1 AND (t.category IN ('urgent','action','reply') OR t.needs_reply = 1)
             ORDER BY CASE t.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, t.triaged_at",
        )
        .db()?;
    let rows = st
        .query_map(params![to_rfc3339(since)], |r| {
            let name: String = r.get(5)?;
            let addr: String = r.get(6)?;
            Ok(MailRow {
                id: r.get(0)?,
                category: r.get(1)?,
                priority: r.get(2)?,
                summary: r.get(3)?,
                suspicious: r.get::<_, i64>(4)? != 0,
                from: if name.is_empty() { addr } else { name },
                subject: r.get(7)?,
            })
        })
        .db()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().db()
}

fn last_triage(db: &Db) -> Result<Option<(String, Option<String>, String)>> {
    db.conn()
        .query_row(
            "SELECT started_at, finished_at, status FROM workflow_runs WHERE workflow = 'mail_triage'
             ORDER BY started_at DESC, rowid DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .db()
}

fn mail_gap(db: &Db, now: &Timestamp, tz: &Tz, mail_configured: bool) -> Result<Option<String>> {
    if !mail_configured {
        return Ok(Some("邮件：没有配置邮箱来源，简报不包含邮件".into()));
    }
    Ok(match last_triage(db)? {
        None => Some("邮件：还没有运行过分诊（`hatch triage run` 或 triage_sync 任务），简报不包含邮件".into()),
        Some((started, finished, status)) => {
            let at = parse_rfc3339(finished.as_deref().unwrap_or(&started)).unwrap_or(*now);
            if status != "ok" {
                Some(format!("邮件：最近一次分诊（{}）状态为 {status}，部分邮件可能未处理", fmt_local(&at, tz)))
            } else if *now - at > Duration::hours(24) {
                Some(format!("邮件：最近一次成功分诊在 {}，之后的新邮件没有覆盖", fmt_local(&at, tz)))
            } else {
                None
            }
        }
    })
}

fn event_line(e: &Event, tz: &Tz) -> String {
    let end = e.ends_at.with_timezone(tz).format("%H:%M");
    let place = if e.location.is_empty() { String::new() } else { format!(" @{}", e.location) };
    format!("{}–{} {}{place}", fmt_local(&e.starts_at, tz), end, e.title)
}

/// Gather today's and tomorrow's facts.
pub fn gather(db: &Db, now: &Timestamp, tz: &Tz, mail_configured: bool) -> Result<Gathered> {
    let mut g = Gathered::default();
    let push = |g: &mut Gathered, kind: FactKind, text: String, source: String| {
        let id = format!("F{}", g.facts.len() + 1);
        g.facts.push(Fact { id, kind, text, source });
    };

    for m in mails(db, &(*now - Duration::hours(MAIL_WINDOW_HOURS)))? {
        let flag = if m.suspicious { "⚠可疑 " } else { "" };
        let summary = if m.summary.is_empty() { String::new() } else { format!("：{}", m.summary) };
        push(&mut g, FactKind::Mail, format!("{flag}[{}|{}] {} —「{}」{summary}", m.category, m.priority, m.from, m.subject), format!("mail:{}", m.id));
    }
    if let Some(gap) = mail_gap(db, now, tz, mail_configured)? {
        g.gaps.push(gap);
    }

    let today = now.with_timezone(tz).date_naive();
    let start = hatch_core::time::local_to_utc(tz, today.and_time(NaiveTime::MIN));
    let end = hatch_core::time::local_to_utc(tz, (today + Duration::days(2)).and_time(NaiveTime::MIN));
    let cal = CalendarRepo::new(db);
    let events = cal.between(&start, &end)?;
    for e in &events {
        push(&mut g, FactKind::Event, event_line(e, tz), format!("event:{}", e.uid));
    }
    for (i, a) in events.iter().enumerate() {
        for b in &events[i + 1..] {
            if a.overlaps(&b.starts_at, &b.ends_at) {
                push(&mut g, FactKind::Conflict, format!("「{}」与「{}」时间重叠", a.title, b.title), format!("event:{}+{}", a.uid, b.uid));
            }
        }
    }
    let total_events: i64 = db.conn().query_row("SELECT COUNT(*) FROM calendar_events", [], |r| r.get(0)).db()?;
    if total_events == 0 {
        g.gaps.push("日历：还没有导入任何日程（`hatch calendar import`），日程部分可能不完整".into());
    }

    for a in ApprovalQueue::new(db).list(Some(ApprovalStatus::Pending))? {
        push(&mut g, FactKind::Approval, format!("{}（{}，`hatch approvals show {}`）", a.title, a.class.as_str(), a.id), format!("approval:{}", a.id));
    }

    let reminders = Notifier::new(db, AttentionPolicy { tz: *tz, ..Default::default() }).pending(false)?;
    for n in reminders.into_iter().filter(|n| !n.ref_id.starts_with("brief:") && !n.ref_id.starts_with("worker:")).take(MAX_REMINDERS) {
        push(&mut g, FactKind::Reminder, n.text.clone(), format!("notification:{}", n.id));
    }
    Ok(g)
}
