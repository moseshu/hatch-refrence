// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! The triage workflow with durable, resumable steps.
//!
//! Per mail: `classify` → `calendar` → `draft` → `notify` → `complete`.
//! A step marked `done` is never repeated; a failed step is retried on the next
//! run. Approvals use content-addressed idempotency keys, so even if the step
//! records were lost the same reply would map to the same approval.

use crate::classify::classify;
use crate::draft::{draft_body, reply_payload, DraftInput};
use crate::model::{Category, TriageResult};
use crate::receipt::{ItemReceipt, Receipt};
use crate::repo::{finish_run, start_run, Steps, TriageRepo};
use chrono::Duration;
use chrono_tz::Tz;
use hatch_connectors::{CalendarRepo, MailRepo, MailSource, StoredMail};
use hatch_core::time::{fmt_local, Timestamp};
use hatch_core::{ids, Result};
use hatch_llm::Router;
use hatch_memory::{MemoryStore, People};
use hatch_policy::{
    ActionClass, ActionRequest, ApprovalQueue, AttentionPolicy, DataLabel, Gatekeeper, NewApproval, Notifier,
};
use hatch_store::Db;
use hatch_tools::{route_action, EventPayload, ExecutorRegistry, RouteOutcome};

#[derive(Clone, Debug)]
pub struct TriageConfig {
    pub user_name: String,
    pub user_email: String,
    pub tz: Tz,
    pub fetch_limit: usize,
    pub approval_ttl_hours: i64,
}

impl Default for TriageConfig {
    fn default() -> Self {
        TriageConfig {
            user_name: "用户".into(),
            user_email: String::new(),
            tz: chrono_tz::Asia::Shanghai,
            fetch_limit: 50,
            approval_ttl_hours: 72,
        }
    }
}

pub struct TriageWorkflow<'a> {
    pub db: &'a Db,
    pub router: &'a Router,
    pub gate: &'a Gatekeeper,
    pub executors: &'a ExecutorRegistry,
    pub attention: AttentionPolicy,
    pub config: TriageConfig,
}

/// Signals that the run must stop (call budget exhausted).
struct Stop;

impl<'a> TriageWorkflow<'a> {
    pub fn run(&self, source: &mut dyn MailSource, now: &Timestamp) -> Result<Receipt> {
        let run_id = ids::random_id("run");
        start_run(self.db, &run_id, now)?;
        let expired = ApprovalQueue::new(self.db).expire_due(now)?;
        let mut r = Receipt { run_id: run_id.clone(), source: source.account(), ..Default::default() };
        if expired > 0 {
            r.not_done.push(format!("{expired} 个审批已过期作废，相关动作不会执行"));
        }
        self.sync(source, now, &mut r);
        let pending = Steps::new(self.db).unfinished_mails()?;
        for mail_id in pending {
            match self.process(&mail_id, now, &mut r) {
                Ok(()) => {}
                Err(Ok(Stop)) => {
                    r.budget_stopped = true;
                    break;
                }
                Err(Err(e)) => r.errors.push(format!("{mail_id}: {e}")),
            }
        }
        let t = self.router.totals();
        r.model_calls = t.calls;
        r.prompt_tokens = t.prompt_tokens;
        r.completion_tokens = t.completion_tokens;
        r.cached_tokens = t.cached_tokens;
        let status = if r.budget_stopped { "budget_stopped" } else if r.errors.is_empty() { "ok" } else { "partial" };
        finish_run(self.db, &run_id, status, &serde_json::to_string(&r)?, now)?;
        self.db.audit("triage", "run_finished", &format!("{run_id} status={status} processed={}", r.processed))?;
        Ok(r)
    }

    fn sync(&self, source: &mut dyn MailSource, now: &Timestamp, r: &mut Receipt) {
        let repo = MailRepo::new(self.db);
        let account = source.account();
        let cursor = match repo.cursor(&account) {
            Ok(c) => c,
            Err(e) => {
                r.errors.push(format!("读取游标失败：{e}"));
                return;
            }
        };
        let mails = match source.fetch_new(cursor, self.config.fetch_limit) {
            Ok(m) => m,
            Err(e) => {
                r.errors.push(format!("拉取邮件失败（本次只处理已同步的邮件）：{e}"));
                return;
            }
        };
        r.fetched = mails.len();
        let people = People::new(self.db);
        for m in &mails {
            match repo.store(&account, m, now) {
                Ok((_, true)) => r.new_mails += 1,
                Ok(_) => {}
                Err(e) => r.errors.push(format!("保存邮件失败：{e}")),
            }
            let _ = people.touch(&m.from_addr, &m.from_name, now);
            if let Some(uid) = m.uid {
                let _ = repo.set_cursor(&account, uid);
            }
        }
    }

    fn process(&self, mail_id: &str, now: &Timestamp, r: &mut Receipt) -> std::result::Result<(), std::result::Result<Stop, hatch_core::Error>> {
        let steps = Steps::new(self.db);
        let mail = MailRepo::new(self.db).get(mail_id).map_err(Err)?;
        let person = People::new(self.db).get(&mail.from_addr).map_err(Err)?;
        r.processed += 1;
        let tz = self.config.tz;

        // 1. classify
        let triage = if steps.is_done(mail_id, "classify").map_err(Err)? {
            TriageRepo::new(self.db).get(mail_id).map_err(Err)?.unwrap_or_else(|| TriageResult::unclassified(mail_id, "missing", "none"))
        } else {
            let t = classify(self.router, &mail, person.as_ref(), now, &tz).map_err(|_| Ok(Stop))?;
            TriageRepo::new(self.db).save(&t, now).map_err(Err)?;
            let status = if t.category == Category::Unclassified { "failed" } else { "done" };
            steps.set(mail_id, "classify", status, t.category.as_str(), now).map_err(Err)?;
            t
        };
        *r.by_category.entry(triage.category.as_str().to_string()).or_default() += 1;
        let mut item = ItemReceipt {
            mail_id: mail_id.to_string(),
            from: if mail.from_name.is_empty() { mail.from_addr.clone() } else { format!("{} <{}>", mail.from_name, mail.from_addr) },
            subject: mail.subject.clone(),
            category: triage.category.label_zh().to_string(),
            priority: triage.priority.as_str().to_string(),
            summary: triage.summary.clone(),
            suspicious: triage.suspicious,
            notes: triage.notes.clone(),
            actions: vec![],
        };
        if triage.category == Category::Unclassified {
            r.not_done.push(format!("「{}」分类失败，下次运行会重试", mail.subject));
            r.items.push(item);
            return Ok(());
        }

        // 2. calendar
        let calendar_note = if steps.is_done(mail_id, "calendar").map_err(Err)? {
            steps.get(mail_id, "calendar").map_err(Err)?.map(|(_, o)| o).unwrap_or_default()
        } else {
            let note = self.handle_events(&mail, &triage, now, r, &mut item).map_err(Err)?;
            steps.set(mail_id, "calendar", "done", &note, now).map_err(Err)?;
            note
        };

        // 3. draft
        if triage.needs_reply && !triage.suspicious && !triage.category.never_reply() && !steps.is_done(mail_id, "draft").map_err(Err)? {
            let style: Vec<String> = MemoryStore::new(self.db)
                .with_tags(&["style", "preference"], now)
                .map_err(Err)?
                .into_iter()
                .map(|c| c.text)
                .collect();
            let input = DraftInput {
                mail: &mail,
                user_name: &self.config.user_name,
                style: &style.join("；"),
                calendar_note: &calendar_note,
                reply_hint: &triage.reply_hint,
            };
            match draft_body(self.router, &input, &tz).map_err(|_| Ok(Stop))? {
                Ok(body) => {
                    let id = self.queue_reply(&mail, &body, now, r, &mut item).map_err(Err)?;
                    steps.set(mail_id, "draft", "done", &id, now).map_err(Err)?;
                }
                Err(reason) => {
                    steps.set(mail_id, "draft", "failed", &reason, now).map_err(Err)?;
                    r.not_done.push(format!("「{}」需要回复，但草稿生成失败（{reason}），下次运行会重试", mail.subject));
                    r.items.push(item);
                    return Ok(());
                }
            }
        }

        // 4. notify
        if !steps.is_done(mail_id, "notify").map_err(Err)? {
            let text = format!(
                "[{}] {}：{}{}",
                triage.category.label_zh(),
                item.from,
                if triage.summary.is_empty() { &mail.subject } else { &triage.summary },
                if triage.suspicious { "（⚠可疑）" } else { "" }
            );
            let d = Notifier::new(self.db, self.attention.clone()).notify(triage.priority, &text, mail_id, now).map_err(Err)?;
            item.actions.push(format!("通知方式：{}", d.as_str()));
            steps.set(mail_id, "notify", "done", d.as_str(), now).map_err(Err)?;
        }
        steps.set(mail_id, "complete", "done", "", now).map_err(Err)?;
        r.items.push(item);
        Ok(())
    }

    fn handle_events(&self, mail: &StoredMail, t: &TriageResult, now: &Timestamp, r: &mut Receipt, item: &mut ItemReceipt) -> Result<String> {
        let tz = self.config.tz;
        let cal = CalendarRepo::new(self.db);
        let mut notes = Vec::new();
        for ev in t.events.iter().filter(|e| e.is_request) {
            let end = ev.end.unwrap_or(ev.start + Duration::hours(1));
            let when = fmt_local(&ev.start, &tz);
            let conflicts = cal.conflicts(&ev.start, &end)?;
            if conflicts.is_empty() {
                notes.push(format!("对方提议的 {when} 目前空闲，可以接受。"));
                let p = EventPayload {
                    uid: ids::stable_id("evt", &[&mail.id, &ev.start.to_rfc3339()]),
                    title: if ev.title.is_empty() { mail.subject.clone() } else { ev.title.clone() },
                    starts_at: ev.start,
                    ends_at: end,
                    location: String::new(),
                };
                let approval = NewApproval {
                    idempotency_key: ids::stable_id("triage-event", &[&mail.id, &ev.start.to_rfc3339()]),
                    kind: "create_event".into(),
                    class: ActionClass::WriteCalendar,
                    title: format!("加入日程：{}（{when}）", p.title),
                    preview: p.preview(&tz),
                    payload: serde_json::to_value(&p)?,
                    reasons: vec![],
                    origin: format!("triage:{}", r.run_id),
                    ttl: Duration::hours(self.config.approval_ttl_hours),
                };
                let req = ActionRequest { tainted: true, ..ActionRequest::new(ActionClass::WriteCalendar, approval.title.clone()) };
                self.record(route_action(self.db, self.gate, self.executors, &req, approval.clone(), now)?, &approval.title, r, item);
            } else {
                let busy: Vec<String> = conflicts.iter().map(|c| format!("「{}」", c.title)).collect();
                let slots = cal.free_slots(now, &(ev.start + Duration::days(5)), end - ev.start, (9, 18), &tz, 3)?;
                let slot_text: Vec<String> = slots.iter().map(|(s, _)| fmt_local(s, &tz)).collect();
                r.conflicts.push(format!("{} 提议的 {when} 与 {} 冲突；可选空闲：{}", mail.from_addr, busy.join("、"), slot_text.join("，")));
                item.actions.push(format!("检测到日程冲突：{when}"));
                notes.push(format!(
                    "对方提议的 {when} 已有安排（不要透露具体内容）。可以改约这些空闲时段：{}。",
                    slot_text.join("，")
                ));
            }
        }
        Ok(notes.join(" "))
    }

    fn queue_reply(&self, mail: &StoredMail, body: &str, now: &Timestamp, r: &mut Receipt, item: &mut ItemReceipt) -> Result<String> {
        let payload = reply_payload(mail, body);
        let mut labels = Vec::new();
        if hatch_core::secrets::contains_secret(body) {
            labels.push(DataLabel::Secret);
        }
        let people = People::new(self.db);
        let unknown: Vec<String> = payload.to.iter().filter(|t| !people.is_known(t).unwrap_or(false)).cloned().collect();
        let title = format!("回复 {}：{}", mail.from_addr, payload.subject);
        let approval = NewApproval {
            idempotency_key: ids::stable_id("triage-reply", &[&mail.id]),
            kind: "send_email".into(),
            class: ActionClass::SendMessage,
            title: title.clone(),
            preview: payload.preview(),
            payload: serde_json::to_value(&payload)?,
            reasons: vec![],
            origin: format!("triage:{}", r.run_id),
            ttl: Duration::hours(self.config.approval_ttl_hours),
        };
        let req = ActionRequest {
            recipients: payload.to.clone(),
            unknown_recipients: unknown,
            labels,
            tainted: true,
            ..ActionRequest::new(ActionClass::SendMessage, title.clone())
        };
        let outcome = route_action(self.db, self.gate, self.executors, &req, approval, now)?;
        let id = match &outcome {
            RouteOutcome::Queued { id, .. } | RouteOutcome::Executed { id, .. } | RouteOutcome::AlreadyHandled { id, .. } | RouteOutcome::Failed { id, .. } => id.clone(),
            RouteOutcome::Denied { .. } => String::new(),
        };
        self.record(outcome, &title, r, item);
        Ok(id)
    }

    fn record(&self, o: RouteOutcome, title: &str, r: &mut Receipt, item: &mut ItemReceipt) {
        match o {
            RouteOutcome::Queued { id, existed: false, .. } => {
                item.actions.push(format!("已提交审批：{title}（{id}）"));
                r.approvals_queued.push((id, title.to_string()));
            }
            RouteOutcome::Queued { id, existed: true, .. } => item.actions.push(format!("审批已在队列中：{id}")),
            RouteOutcome::Executed { id, result } => {
                item.actions.push(format!("已自动执行（{id}）：{result}"));
                r.auto_executed.push(format!("{title} — {result}"));
            }
            RouteOutcome::AlreadyHandled { id, status } => item.actions.push(format!("此前已处理（{id}，{}）", status.as_str())),
            RouteOutcome::Denied { reason } => r.not_done.push(format!("{title}：被策略拒绝（{reason}）")),
            RouteOutcome::Failed { id, error } => r.errors.push(format!("{title}（{id}）执行失败：{error}")),
        }
    }
}
