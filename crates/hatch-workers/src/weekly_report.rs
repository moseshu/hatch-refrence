//! `weekly_report`: the past week's mail, calendar and finished tasks,
//! rendered as a Markdown report. Facts are gathered by code; the model
//! (when a router is configured) only writes a short summary on top —
//! the report is complete without it, so a week with nothing to say
//! costs no model call. One report per ISO week, deduped by the key
//! `weekly-report:<iso-week>`.
//!
//! This is the third first scenario (dev plan §1): the weekly routine.
//! The scheduler job is the routine path; the `weekly-report` ledger
//! kind is the run-it-now path, and both share `generate()`.

use chrono::{Datelike, Duration, NaiveTime};
use chrono_tz::Tz;
use hatch_connectors::CalendarRepo;
use hatch_core::time::{fmt_local, local_to_utc, to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_llm::{ChatMessage, ChatRequest, Workload};
use hatch_policy::AttentionPolicy;
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_store::{params, rusqlite, Db, DbResultExt};

use crate::{deliver, make_router, RouterFactory, WorkerOutputs};

/// Scheduler job kind and ledger task kind for the weekly report.
pub const KIND: &str = "weekly_report";

/// Mail considered: the past 7 days, most recent first.
const MAIL_WINDOW_DAYS: i64 = 7;
const MAX_MAIL: usize = 30;
const MAX_TASKS: usize = 20;

pub struct WeeklyReportWorker {
    pub router: Option<RouterFactory>,
    pub attention: AttentionPolicy,
    pub user_name: String,
    pub mail_configured: bool,
}

pub struct Generated {
    /// ISO week id, e.g. `2026-W46` — also the dedup key suffix.
    pub week: String,
    /// Local week range, e.g. `2026-11-09～11-15`.
    pub week_label: String,
    pub content: String,
    pub mail_count: usize,
    pub event_count: usize,
    pub tasks_done: usize,
}

struct MailRow {
    from: String,
    subject: String,
    category: String,
    summary: String,
}

struct TaskRow {
    title: String,
    kind: String,
}

/// ISO week id for a timestamp in a timezone, e.g. `2026-W46`.
pub fn week_id(now: &Timestamp, tz: &Tz) -> String {
    week_of(now, tz).0
}

fn week_of(now: &Timestamp, tz: &Tz) -> (String, String, Timestamp, Timestamp) {
    let date = now.with_timezone(tz).date_naive();
    let monday = date - Duration::days(i64::from(date.weekday().num_days_from_monday()));
    let sunday = monday + Duration::days(6);
    let iso = date.iso_week();
    let week = format!("{}-W{:02}", iso.year(), iso.week());
    let label = format!("{}～{}", monday.format("%Y-%m-%d"), sunday.format("%m-%d"));
    let start = local_to_utc(tz, monday.and_time(NaiveTime::MIN));
    let end = local_to_utc(tz, (monday + Duration::days(7)).and_time(NaiveTime::MIN));
    (week, label, start, end)
}

fn mails(db: &Db, since: &Timestamp) -> Result<Vec<MailRow>> {
    let c = db.conn();
    let mut st = c
        .prepare(
            "SELECT m.from_name, m.from_addr, m.subject, t.category, t.summary
             FROM triage_results t JOIN mail_messages m ON m.id = t.mail_id
             WHERE t.triaged_at >= ?1 ORDER BY t.triaged_at DESC LIMIT ?2",
        )
        .db()?;
    let rows = st
        .query_map(params![to_rfc3339(since), MAX_MAIL as i64], |r| {
            let name: String = r.get(0)?;
            let addr: String = r.get(1)?;
            Ok(MailRow {
                from: if name.is_empty() { addr } else { name },
                subject: r.get(2)?,
                category: r.get(3)?,
                summary: r.get(4)?,
            })
        })
        .db()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().db()
}

fn tasks_done(db: &Db, since: &Timestamp, until: &Timestamp) -> Result<Vec<TaskRow>> {
    let c = db.conn();
    let mut st = c
        .prepare(
            "SELECT title, kind FROM tasks
             WHERE status = 'done' AND finished_at >= ?1 AND finished_at < ?2
             ORDER BY finished_at DESC LIMIT ?3",
        )
        .db()?;
    let rows = st
        .query_map(
            params![to_rfc3339(since), to_rfc3339(until), MAX_TASKS as i64],
            |r| {
                Ok(TaskRow {
                    title: r.get(0)?,
                    kind: r.get(1)?,
                })
            },
        )
        .db()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().db()
}

fn section(out: &mut String, title: &str, lines: &[String]) {
    out.push_str(&format!("## {title}\n\n"));
    if lines.is_empty() {
        out.push_str("（无）\n\n");
    } else {
        for l in lines {
            out.push_str("- ");
            out.push_str(l);
            out.push('\n');
        }
        out.push('\n');
    }
}

impl WeeklyReportWorker {
    /// Gather the week's facts by code and render the report. The model
    /// summary is best-effort: any failure leaves the report complete
    /// without it, and a cancelled run aborts.
    pub fn generate(
        &self,
        db: &Db,
        now: &Timestamp,
        tz: &Tz,
        cancel: &CancelToken,
    ) -> Result<Generated> {
        let (week, week_label, week_start, week_end) = week_of(now, tz);
        let since = *now - Duration::days(MAIL_WINDOW_DAYS);
        let mail = mails(db, &since)?;
        let cal = CalendarRepo::new(db);
        let this_week = cal.between(&week_start, &week_end)?;
        let next_week = cal.between(&week_end, &(week_end + Duration::days(7)))?;
        let done = tasks_done(db, &since, now)?;

        let mut gaps: Vec<String> = Vec::new();
        if !self.mail_configured {
            gaps.push("邮件：没有配置邮箱来源，周报不包含邮件。".into());
        } else if mail.is_empty() {
            gaps.push("邮件：过去 7 天没有已分诊的邮件。".into());
        }
        if this_week.is_empty() && next_week.is_empty() {
            gaps.push("日历：本周和下周都没有日程。".into());
        }
        if done.is_empty() {
            gaps.push("任务：过去 7 天没有完成的任务。".into());
        }

        let mut out = String::new();
        out.push_str(&format!("# 周报 {week}（{week_label}）\n\n"));
        let mail_lines: Vec<String> = mail
            .iter()
            .map(|m| {
                let summary = if m.summary.is_empty() {
                    String::new()
                } else {
                    format!("：{}", m.summary)
                };
                format!("[{}] {} —「{}」{summary}", m.category, m.from, m.subject)
            })
            .collect();
        section(&mut out, "本周邮件", &mail_lines);
        let event_lines = |events: &[hatch_connectors::Event]| -> Vec<String> {
            events
                .iter()
                .map(|e| {
                    format!(
                        "{} {}–{} {}",
                        fmt_local(&e.starts_at, tz),
                        e.starts_at.with_timezone(tz).format("%H:%M"),
                        e.ends_at.with_timezone(tz).format("%H:%M"),
                        e.title
                    )
                })
                .collect()
        };
        section(&mut out, "本周日程", &event_lines(&this_week));
        section(&mut out, "下周日程", &event_lines(&next_week));
        let task_lines: Vec<String> = done
            .iter()
            .map(|t| format!("[{}] {}", t.kind, t.title))
            .collect();
        section(&mut out, "本周完成任务", &task_lines);

        // Best-effort model summary: it only phrases what is already
        // above, and any failure leaves the report as-is.
        if let Ok(router) = make_router(&self.router) {
            let req = ChatRequest::new(vec![
                ChatMessage::system(format!(
                    "你是 {name} 的助手。写一段不超过三行的中文摘要，只复述用户提供的周报内容，不要添加新事实。",
                    name = self.user_name
                )),
                ChatMessage::user(out.clone()),
            ]);
            match router.chat_with(Workload::Brief, &req, Some(cancel)) {
                Err(e) if e.is_cancelled() => return Err(hatch_core::Error::Cancelled(e.message)),
                Err(_) => gaps.push("摘要：模型调用失败，本周报没有模型摘要。".into()),
                Ok(r) => {
                    let summary = r.content.unwrap_or_default();
                    if summary.trim().is_empty() {
                        gaps.push("摘要：模型没有返回摘要。".into());
                    } else {
                        out.push_str("## 摘要\n\n");
                        out.push_str(summary.trim());
                        out.push('\n');
                    }
                }
            }
            // Record the call cost even when the summary failed: the
            // tokens were still spent.
            let _ = crate::record_usage(db, &router, now);
        }

        if !gaps.is_empty() {
            out.push_str("\n## 缺口\n\n");
            for g in &gaps {
                out.push_str(&format!("- {g}\n"));
            }
        }

        Ok(Generated {
            week,
            week_label,
            content: out,
            mail_count: mail.len(),
            event_count: this_week.len() + next_week.len(),
            tasks_done: done.len(),
        })
    }
}

impl JobHandler for WeeklyReportWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let (week, _, _, _) = week_of(&ctx.now, &ctx.tz);
        let key = format!("weekly-report:{week}");
        if ctx.dedup_seen(&key)? {
            return Ok(JobOutcome::Skipped {
                reason: format!("{week} 周报已生成"),
            });
        }
        let g = self.generate(ctx.db, &ctx.now, &ctx.tz, &ctx.cancel)?;
        WorkerOutputs::new(ctx.db).add(
            "weekly-report",
            &format!("周报 {week}"),
            &g.content,
            &ctx.now,
        )?;
        deliver(
            ctx.db,
            &self.attention,
            ctx.job,
            &format!(
                "{} 周报已生成（邮件 {} 封，日程 {} 项，完成任务 {} 个）",
                g.week, g.mail_count, g.event_count, g.tasks_done
            ),
            &key,
            &ctx.now,
        )?;
        ctx.dedup_mark(&key)?;
        Ok(JobOutcome::Done {
            summary: format!(
                "{} 周报已生成：邮件 {} 封，日程 {} 项，完成任务 {} 个",
                g.week, g.mail_count, g.event_count, g.tasks_done
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed::{self, SeedMail};
    use hatch_core::time::{parse_rfc3339, to_rfc3339};
    use hatch_policy::Priority;
    use hatch_store::Db;
    use hatch_triage::Category;

    fn worker() -> WeeklyReportWorker {
        WeeklyReportWorker {
            router: None,
            attention: AttentionPolicy::default(),
            user_name: "Moses".into(),
            mail_configured: true,
        }
    }

    fn seed_task(db: &Db, id: &str, title: &str, status: &str, finished: Option<&Timestamp>) {
        let now = to_rfc3339(&parse_rfc3339("2026-11-03T13:00:00Z").unwrap());
        let fin = finished.map(to_rfc3339);
        db.conn()
            .execute(
                "INSERT INTO tasks(id, kind, title, owner, status, last_heartbeat, created_at, updated_at, finished_at)
                 VALUES(?1, 'meeting-brief', ?2, 'test', ?3, ?4, ?4, ?4, ?5)",
                params![id, title, status, now, fin],
            )
            .unwrap();
    }

    #[test]
    fn generate_renders_week_facts_and_excludes_out_of_window() {
        let db = Db::open_in_memory().unwrap();
        let now = parse_rfc3339("2026-11-03T13:00:00Z").unwrap(); // Tue, NY: 2026-W45
        let tz: Tz = "America/New_York".parse().unwrap();
        let old = now - Duration::days(10);

        seed::mail(
            &db,
            "moses@126.example",
            &SeedMail {
                message_id: "<w1@corp>",
                from_addr: "wang@corp.example",
                from_name: "王经理",
                subject: "Q4 预算请今天确认",
                category: Category::Urgent,
                priority: Priority::parse("urgent").unwrap(),
                summary: "财务 18:00 前要最终数字",
                needs_reply: true,
                suspicious: false,
            },
            &now,
        )
        .unwrap();
        seed::mail(
            &db,
            "moses@126.example",
            &SeedMail {
                message_id: "<w-old@corp>",
                from_addr: "old@corp.example",
                from_name: "",
                subject: "上上周的旧邮件",
                category: Category::Newsletter,
                priority: Priority::parse("low").unwrap(),
                summary: "旧内容",
                needs_reply: false,
                suspicious: false,
            },
            &old,
        )
        .unwrap();
        seed::event(
            &db,
            "ev-this",
            "架构评审",
            parse_rfc3339("2026-11-04T20:00:00Z").unwrap(),
            60,
            "",
        )
        .unwrap();
        seed::event(
            &db,
            "ev-next",
            "下周计划会",
            parse_rfc3339("2026-11-10T20:00:00Z").unwrap(),
            30,
            "",
        )
        .unwrap();
        seed::event(
            &db,
            "ev-old",
            "上周回顾",
            parse_rfc3339("2026-10-28T20:00:00Z").unwrap(),
            30,
            "",
        )
        .unwrap();
        seed_task(
            &db,
            "tsk_in",
            "已完成任务A",
            "done",
            Some(&(now - Duration::days(1))),
        );
        seed_task(&db, "tsk_old", "十天前完成的任务", "done", Some(&old));
        seed_task(&db, "tsk_open", "还没做完的任务", "running", None);

        let g = worker()
            .generate(&db, &now, &tz, &CancelToken::new())
            .unwrap();
        assert_eq!(g.week, "2026-W45");
        assert_eq!(g.week_label, "2026-11-02～11-08");
        assert_eq!(g.mail_count, 1);
        assert_eq!(g.event_count, 2);
        assert_eq!(g.tasks_done, 1);
        assert!(g.content.contains("Q4 预算请今天确认"), "{}", g.content);
        assert!(g.content.contains("架构评审"), "{}", g.content);
        assert!(g.content.contains("下周计划会"), "{}", g.content);
        assert!(g.content.contains("已完成任务A"), "{}", g.content);
        assert!(!g.content.contains("上上周的旧邮件"), "{}", g.content);
        assert!(!g.content.contains("上周回顾"), "{}", g.content);
        assert!(!g.content.contains("十天前完成的任务"), "{}", g.content);
        assert!(!g.content.contains("还没做完的任务"), "{}", g.content);
        // No router configured: the report is complete without a
        // summary, and no usage was recorded.
        assert!(!g.content.contains("## 摘要"), "{}", g.content);
        let calls: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM usage_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(calls, 0);
    }

    #[test]
    fn week_id_follows_the_local_timezone_not_utc() {
        let tz: Tz = "America/New_York".parse().unwrap();
        // 2026-11-02T00:30Z is still Sunday evening in New York.
        let sunday = parse_rfc3339("2026-11-02T00:30:00Z").unwrap();
        assert_eq!(week_id(&sunday, &tz), "2026-W44");
        // 2026-11-02T05:30Z is Monday 00:30 in New York.
        let monday = parse_rfc3339("2026-11-02T05:30:00Z").unwrap();
        assert_eq!(week_id(&monday, &tz), "2026-W45");
    }
}
