//! Daily-brief scenario: one per day, every item cites a real fact, nothing
//! fabricated survives, nothing out of window appears, gaps are stated, and an
//! empty day costs no model call.

use super::{ts, SchedulerWorld};
use crate::report::Check;
use chrono::Duration;
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{CancelToken, Result};
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatResponse, LlmError, Router, Usage};
use hatch_policy::{AttentionPolicy, Notifier, Priority};
use hatch_scheduler::{Engine, JobStore, NewJob};
use hatch_store::{params, Db};
use hatch_triage::Category;
use hatch_workers::brief::{facts::Fact, KIND};
use hatch_workers::memory_flush::similarity::normalise;
use hatch_workers::seed::{self, SeedMail};
use hatch_workers::{BriefStore, BriefWorker, RouterFactory};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BriefOutcome {
    pub content: String,
    pub facts: usize,
    /// Items in the model's sections (after validation).
    pub items: usize,
    pub dropped: usize,
    /// Share of items whose wording overlaps the facts they cite (≥ 40 % of bigrams).
    pub supported: f64,
    pub literal_mentions: String,
    pub model_calls: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// Pipeline model: cites every fact and slips in two uncited "facts".
fn oracle(fabricated: Vec<String>) -> RouterFactory {
    let model = Arc::new(FnModel::new("brief-oracle", move |req| {
        let user = req
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let mut items: Vec<serde_json::Value> = user
            .lines()
            .filter_map(|l| {
                let (id, rest) = l.split_once(' ')?;
                let text = rest.split_once("] ")?.1;
                (id.starts_with('F') && id[1..].chars().all(|c| c.is_ascii_digit()))
                    .then(|| json!({"text": text, "refs": [id]}))
            })
            .collect();
        if items.is_empty() {
            return Err(LlmError::permanent("oracle: no facts in request"));
        }
        if let Some(f) = fabricated.first() {
            items.push(json!({"text": f, "refs": []}));
        }
        if let Some(f) = fabricated.get(1) {
            items.push(json!({"text": f, "refs": ["F999"]}));
        }
        let text = json!({"sections": [{"title": "今日要点", "items": items}]}).to_string();
        let prompt: usize = req
            .messages
            .iter()
            .map(|m| hatch_core::tokens::estimate_tokens(&m.content))
            .sum();
        Ok(ChatResponse {
            content: Some(text),
            usage: Usage {
                prompt_tokens: prompt as u64,
                completion_tokens: 200,
                ..Default::default()
            },
            model: "brief-oracle".into(),
            ..Default::default()
        })
    }));
    Arc::new(move || Ok(Router::single(model.clone())))
}

fn seed_world(db: &Db, w: &SchedulerWorld, now: &Timestamp) -> Result<()> {
    let b = &w.spec.brief;
    for m in &b.mails {
        let mail = SeedMail {
            message_id: &m.id,
            from_addr: &m.from,
            from_name: &m.name,
            subject: &m.subject,
            category: Category::parse(&m.category).unwrap_or(Category::Unclassified),
            priority: Priority::parse(&m.priority)?,
            summary: &m.summary,
            needs_reply: m.needs_reply,
            suspicious: m.suspicious,
        };
        seed::mail(db, &w.spec.persona.email, &mail, now)?;
    }
    for e in &b.events {
        seed::event(db, &e.uid, &e.title, ts(&e.start)?, e.minutes, &e.location)?;
    }
    for (i, a) in b.approvals.iter().enumerate() {
        seed::approval(db, &format!("seed-approval-{i}"), a, now)?;
    }
    let n = Notifier::new(
        db,
        AttentionPolicy {
            tz: w.tz,
            ..Default::default()
        },
    );
    for (i, r) in b.reminders.iter().enumerate() {
        n.notify(Priority::Normal, r, &format!("seed-reminder-{i}"), now)?;
    }
    let last = to_rfc3339(&(*now - Duration::hours(b.last_triage_hours_ago)));
    db.conn()
        .execute("INSERT INTO workflow_runs(run_id, workflow, started_at, finished_at, status) VALUES('seed','mail_triage',?1,?1,'ok')", params![last])
        .map_err(|e| hatch_core::Error::Storage(e.to_string()))?;
    Ok(())
}

fn usage(db: &Db) -> (i64, i64, i64) {
    db.conn()
        .query_row("SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0) FROM usage_log", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap_or((0, 0, 0))
}

/// `(text, refs)` of every bullet in the model's sections.
fn model_items(body: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut in_model_section = false;
    for line in body.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            in_model_section = !(h.starts_with("其他事项") || h == "事项" || h == "覆盖缺口");
            continue;
        }
        if !in_model_section {
            continue;
        }
        if let Some(item) = line.strip_prefix("- ") {
            if let Some((text, refs)) = item.rsplit_once('〔') {
                let refs = refs
                    .trim_end_matches('〕')
                    .split(',')
                    .map(|r| r.trim().to_string())
                    .collect();
                out.push((text.trim().to_string(), refs));
            }
        }
    }
    out
}

/// Every fact id inside `〔…〕` markers.
fn cited_ids(body: &str) -> BTreeSet<String> {
    body.split('〔')
        .skip(1)
        .filter_map(|s| s.split_once('〕').map(|(inner, _)| inner))
        .flat_map(|inner| inner.split(',').map(|r| r.trim().to_string()))
        .collect()
}

fn overlap(item: &str, sources: &str) -> f64 {
    let (a, b) = (normalise(item), normalise(sources));
    let grams = |s: &str| -> BTreeSet<(char, char)> {
        let c: Vec<char> = s.chars().collect();
        c.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (ga, gb) = (grams(&a), grams(&b));
    if ga.is_empty() {
        return 0.0;
    }
    ga.iter().filter(|g| gb.contains(g)).count() as f64 / ga.len() as f64
}

pub fn run(
    world: &SchedulerWorld,
    factory: Option<RouterFactory>,
    live: bool,
    checks: &mut Vec<Check>,
) -> Result<BriefOutcome> {
    let b = &world.spec.brief;
    let factory = factory.unwrap_or_else(|| oracle(b.fabricated.clone()));
    let now = ts(&b.now)?;
    let db = Db::open_in_memory()?;
    seed_world(&db, world, &now)?;
    let worker = BriefWorker {
        router: Some(factory.clone()),
        attention: AttentionPolicy {
            tz: world.tz,
            ..Default::default()
        },
        user_name: world.spec.persona.name.clone(),
        mail_configured: true,
    };
    let e = Engine::new(&db, world.tz).register(KIND, Arc::new(worker));
    let local = now.with_timezone(&world.tz);
    JobStore::new(&db).add(
        &NewJob::new(
            "daily-brief",
            KIND,
            &format!("daily {}", local.format("%H:%M")),
        ),
        &(now - Duration::minutes(1)),
        &world.tz,
    )?;
    let c = CancelToken::new();
    e.tick(&now, &c)?;
    e.run_now("daily-brief", &(now + Duration::hours(2)), &c)?;
    e.run_now("daily-brief", &(now + Duration::hours(3)), &c)?;
    let store = BriefStore::new(&db);
    let day1 = local.date_naive().to_string();
    let day2 = (local + Duration::days(1)).date_naive().to_string();
    let first = store
        .latest(Some(&day1))?
        .ok_or_else(|| hatch_core::Error::NotFound("brief for day 1".into()))?;
    e.tick(&(now + Duration::days(1)), &c)?;
    let (n1, n2) = (store.count_for(&day1)?, store.count_for(&day2)?);
    checks.push(check(
        "每日简报：同一天只生成一份",
        n1 == 1 && n2 == 1,
        format!("{day1}：{n1} 份（定时 1 次 + 手动 2 次）；{day2}：{n2} 份"),
    ));

    let facts: Vec<Fact> = serde_json::from_value(first.facts.clone()).unwrap_or_default();
    let (body, _) = first
        .content
        .split_once("\n## 依据\n")
        .unwrap_or((first.content.as_str(), ""));
    let items = model_items(body);
    let ids: BTreeSet<&str> = facts.iter().map(|f| f.id.as_str()).collect();
    let all_cited = items
        .iter()
        .all(|(_, r)| !r.is_empty() && r.iter().all(|x| ids.contains(x.as_str())));
    let fabricated: Vec<&String> = b
        .fabricated
        .iter()
        .filter(|f| first.content.contains(f.as_str()))
        .collect();
    let dropped: usize = first
        .content
        .rsplit("因缺少依据丢弃 ")
        .next()
        .and_then(|s| s.split('。').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    let want_dropped = if live { None } else { Some(b.fabricated.len()) };
    checks.push(check(
        "简报无编造：保留的条目都引用真实事实，无依据条目被丢弃",
        all_cited && fabricated.is_empty() && want_dropped.map(|d| d == dropped).unwrap_or(true),
        format!(
            "条目 {}，全部有效引用 {all_cited}，丢弃 {dropped}{}；编造内容残留 {:?}",
            items.len(),
            want_dropped
                .map(|d| format!("（期望 {d}）"))
                .unwrap_or_default(),
            fabricated
        ),
    ));
    let cited = cited_ids(body);
    let missing: Vec<&str> = facts
        .iter()
        .filter(|f| !cited.contains(&f.id))
        .map(|f| f.id.as_str())
        .collect();
    checks.push(check(
        "简报完整：每条事实都被引用或原样列出",
        missing.is_empty(),
        format!("事实 {} 条，未覆盖 {:?}", facts.len(), missing),
    ));
    let literal: Vec<&String> = b
        .must_mention
        .iter()
        .filter(|m| body.contains(m.as_str()))
        .collect();
    let literal_mentions = format!("{}/{}", literal.len(), b.must_mention.len());
    if !live {
        checks.push(check(
            "简报正文提到全部关键事项（字面）",
            literal.len() == b.must_mention.len(),
            literal_mentions.clone(),
        ));
    }
    let leaked: Vec<&String> = b
        .must_not_mention
        .iter()
        .filter(|m| first.content.contains(m.as_str()))
        .collect();
    checks.push(check(
        "不该出现的不出现（订阅邮件、窗口外日程）",
        leaked.is_empty(),
        format!("出现了 {leaked:?}"),
    ));
    checks.push(check(
        "覆盖缺口由代码写明（分诊超过 24 小时未运行）",
        first.content.contains("最近一次成功分诊在"),
        "简报含「覆盖缺口」一节",
    ));

    let supported_n = items
        .iter()
        .filter(|(text, refs)| {
            let src: String = facts
                .iter()
                .filter(|f| refs.contains(&f.id))
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            overlap(text, &src) >= 0.4
        })
        .count();
    let supported = if items.is_empty() {
        1.0
    } else {
        supported_n as f64 / items.len() as f64
    };
    if live {
        checks.push(check(
            "条目措辞贴合所引事实（字重合 ≥ 40% 的条目占比 ≥ 80%）",
            supported >= 0.8,
            format!("{supported_n}/{}", items.len()),
        ));
    }

    // An empty day: no model call, an honest "无新事项".
    let empty = Db::open_in_memory()?;
    let w2 = BriefWorker {
        router: Some(factory),
        attention: AttentionPolicy {
            tz: world.tz,
            ..Default::default()
        },
        user_name: world.spec.persona.name.clone(),
        mail_configured: true,
    };
    let e2 = Engine::new(&empty, world.tz).register(KIND, Arc::new(w2));
    JobStore::new(&empty).add(
        &NewJob::new("daily-brief", KIND, "daily 08:00"),
        &now,
        &world.tz,
    )?;
    e2.run_now("daily-brief", &now, &c)?;
    let blank = BriefStore::new(&empty)
        .latest(None)?
        .map(|b| b.content)
        .unwrap_or_default();
    let (calls_empty, _, _) = usage(&empty);
    checks.push(check(
        "无事项时如实写「无新事项」且不调用模型",
        blank.contains("无新事项") && calls_empty == 0,
        format!("模型调用 {calls_empty} 次"),
    ));

    let (model_calls, prompt_tokens, completion_tokens) = usage(&db);
    Ok(BriefOutcome {
        content: first.content.clone(),
        facts: facts.len(),
        items: items.len(),
        dropped,
        supported,
        literal_mentions,
        model_calls,
        prompt_tokens,
        completion_tokens,
    })
}
