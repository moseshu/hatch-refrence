//! Feed: one configured prompt, one scheduled worker, items that always
//! cite their source facts.
//!
//! The worker reuses the brief's fact gathering ([`facts::gather`]):
//! mail triage results, calendar events, conflicts, pending approvals,
//! reminders. The model only orders and phrases; code validates every
//! item's references, and anything without a known fact id is dropped.
//! Items are delivered through [`WorkerOutputs`] (the handoff path),
//! never written directly to a chat.
//!
//! The intro item is fixed text for a new reader. It never claims
//! personal knowledge of the user — it states what the feed is and that
//! every item carries its source.

use crate::goals::GoalStore;
use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::trust::wrap_untrusted;
use hatch_core::{ids, CancelToken, Error, Result};
use hatch_llm::json_extract::extract_json;
use hatch_llm::{ChatMessage, ChatRequest, Router, Workload};
use hatch_policy::AttentionPolicy;
use hatch_scheduler::{JobContext, JobHandler, JobOutcome};
use hatch_store::{params, rusqlite, Db, DbResultExt, OptionalExtension};
use hatch_workers::brief::facts::{self, Fact, Gathered};
use hatch_workers::outputs::{deliver, WorkerOutputs};
use hatch_workers::{make_router, record_usage, RouterFactory};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Fixed intro text. Deliberately impersonal: it describes the feed, it
/// does not claim to know the user.
pub const INTRO_TEXT: &str = "你好，这里是你的每日动态。下面的每一条都来自你最近的邮件、日程或待办事项，并标明了来源；没有来源的内容不会出现在这里。";

/// Default `[feed] prompt` when the config leaves it empty.
pub const DEFAULT_PROMPT: &str = "把以下事实整理成 3–5 条值得一读的动态：只写事实里有的内容，每条一句话，附上它引用的事实编号。只输出 JSON：{\"items\": [{\"text\": \"...\", \"refs\": [\"F1\"]}]}。";

pub const KIND: &str = "daily_feed";

const MAX_ITEM_CHARS: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct FeedItem {
    pub id: String,
    pub created_at: Timestamp,
    pub title: String,
    pub content: String,
    /// Fact ids this item cites (`F1`, …). Empty only for the intro item.
    pub source_facts: Vec<String>,
    pub model: String,
    pub is_intro: bool,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<FeedItem> {
    Ok(FeedItem {
        id: r.get(0)?,
        created_at: parse_rfc3339(&r.get::<_, String>(1)?).unwrap_or_default(),
        title: r.get(2)?,
        content: r.get(3)?,
        source_facts: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        model: r.get(5)?,
        is_intro: r.get::<_, i64>(6)? != 0,
    })
}

const COLS: &str = "id, created_at, title, content, source_facts, model, is_intro";

pub struct FeedStore<'a> {
    db: &'a Db,
}

impl<'a> FeedStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        FeedStore { db }
    }

    pub fn save(&self, title: &str, content: &str, source_facts: &[String], model: &str, is_intro: bool, now: &Timestamp) -> Result<String> {
        let id = ids::random_id("feed");
        self.db
            .conn()
            .execute(
                &format!("INSERT INTO feed_items({COLS}) VALUES(?1,?2,?3,?4,?5,?6,?7)"),
                params![
                    id,
                    to_rfc3339(now),
                    title,
                    hatch_core::secrets::redact(content),
                    serde_json::to_string(source_facts)?,
                    model,
                    i64::from(is_intro)
                ],
            )
            .db()?;
        Ok(id)
    }

    /// Insert the fixed intro item once; a no-op when one already exists.
    pub fn ensure_intro(&self, now: &Timestamp) -> Result<bool> {
        let n: i64 = self
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM feed_items WHERE is_intro = 1", [], |r| r.get(0))
            .db()?;
        if n > 0 {
            return Ok(false);
        }
        self.save("欢迎", INTRO_TEXT, &[], "程序（固定文本）", true, now)?;
        Ok(true)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<FeedItem>> {
        let c = self.db.conn();
        let mut st = c
            .prepare(&format!("SELECT {COLS} FROM feed_items ORDER BY created_at DESC, rowid DESC LIMIT ?1"))
            .db()?;
        let rows = st.query_map(params![limit as i64], row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    pub fn latest(&self) -> Result<Option<FeedItem>> {
        self.db
            .conn()
            .query_row(&format!("SELECT {COLS} FROM feed_items ORDER BY created_at DESC, rowid DESC LIMIT 1"), [], row)
            .optional()
            .db()
    }
}

/// One validated feed item (before storing).
#[derive(Clone, Debug)]
pub struct DraftItem {
    pub title: String,
    pub content: String,
    pub refs: Vec<String>,
}

/// Validate the model's JSON: every item needs text and at least one
/// reference, and every reference must be a known fact id. Anything
/// else is dropped (never repaired by guessing).
pub fn validate(v: &Value, facts: &[Fact]) -> (Vec<DraftItem>, usize) {
    let known: BTreeSet<&str> = facts.iter().map(|f| f.id.as_str()).collect();
    let mut items = Vec::new();
    let mut dropped = 0;
    for it in v.get("items").and_then(Value::as_array).cloned().unwrap_or_default() {
        let text: String = it.get("text").and_then(Value::as_str).unwrap_or("").trim().chars().take(MAX_ITEM_CHARS).collect();
        let refs: Vec<String> = it
            .get("refs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(|r| r.trim().to_string()).collect())
            .unwrap_or_default();
        if text.is_empty() || refs.is_empty() || !refs.iter().all(|r| known.contains(r.as_str())) {
            dropped += 1;
            continue;
        }
        let title: String = text.chars().take(30).collect();
        items.push(DraftItem { title, content: hatch_core::secrets::redact(&text), refs });
    }
    (items, dropped)
}

/// Plain fallback when there is no model: one item per fact, written by
/// code, each citing exactly its own fact.
pub fn fallback_items(g: &Gathered) -> Vec<DraftItem> {
    g.facts
        .iter()
        .map(|f| DraftItem {
            title: f.kind.label().to_string(),
            content: f.text.clone(),
            refs: vec![f.id.clone()],
        })
        .collect()
}

fn prompt(prompt_text: &str, g: &Gathered) -> ChatRequest {
    let list: Vec<String> = g.facts.iter().map(|f| format!("{} [{}] {}", f.id, f.kind.label(), f.text)).collect();
    let mut req = ChatRequest::new(vec![
        ChatMessage::system(prompt_text),
        ChatMessage::user(wrap_untrusted("feed_facts", &list.join("\n"))),
    ]);
    req.json_mode = true;
    req.temperature = Some(0.2);
    req
}

pub struct FeedWorker {
    pub router: Option<RouterFactory>,
    pub attention: AttentionPolicy,
    pub user_name: String,
    pub mail_configured: bool,
    /// `[feed] prompt` from config; empty falls back to [`DEFAULT_PROMPT`].
    pub prompt: String,
}

impl FeedWorker {
    fn effective_prompt(&self) -> &str {
        if self.prompt.trim().is_empty() {
            DEFAULT_PROMPT
        } else {
            self.prompt.trim()
        }
    }

    /// Gather facts and compose items, without storing anything.
    pub fn generate(&self, ctx: &JobContext) -> Result<(Vec<DraftItem>, Gathered, String, Option<String>)> {
        let gathered = facts::gather(ctx.db, &ctx.now, &ctx.tz, self.mail_configured)?;
        ctx.cancel.check()?;
        if gathered.facts.is_empty() {
            return Ok((vec![], gathered, String::new(), None));
        }
        let (items, model, fallback) = match make_router(&self.router) {
            Err(why) => (fallback_items(&gathered), String::new(), Some(why)),
            Ok(router) => {
                let r = compose(Some(&router), self.effective_prompt(), &gathered, &ctx.cancel)?;
                record_usage(ctx.db, &router, &ctx.now)?;
                r
            }
        };
        Ok((items, gathered, model, fallback))
    }
}

fn compose(router: Option<&Router>, prompt_text: &str, g: &Gathered, cancel: &CancelToken) -> Result<(Vec<DraftItem>, String, Option<String>)> {
    let Some(router) = router else {
        return Ok((fallback_items(g), String::new(), Some("没有配置模型".into())));
    };
    match router.chat_with(Workload::Feed, &prompt(prompt_text, g), Some(cancel)) {
        Err(e) if e.is_cancelled() => Err(Error::Cancelled(e.message)),
        Err(e) => Ok((fallback_items(g), String::new(), Some(format!("模型调用失败：{e}")))),
        Ok(r) => {
            let text = r.content.unwrap_or_default();
            match extract_json(&text) {
                None => Ok((fallback_items(g), r.model, Some("模型输出不是 JSON".into()))),
                Some(v) => {
                    let (items, dropped) = validate(&v, &g.facts);
                    let fallback = if items.is_empty() {
                        Some("模型输出没有任何有依据的条目".to_string())
                    } else if dropped > 0 {
                        Some(format!("丢弃无依据条目 {dropped}"))
                    } else {
                        None
                    };
                    let items = if items.is_empty() { fallback_items(g) } else { items };
                    Ok((items, r.model, fallback))
                }
            }
        }
    }
}

impl JobHandler for FeedWorker {
    fn run(&self, ctx: &JobContext) -> Result<JobOutcome> {
        let date = ctx.now.with_timezone(&ctx.tz).date_naive().to_string();
        let key = format!("feed:{date}");
        if ctx.dedup_seen(&key)? {
            return Ok(JobOutcome::Skipped { reason: format!("{date} 的动态已经生成过（同一天只生成一次）") });
        }
        let store = FeedStore::new(ctx.db);
        store.ensure_intro(&ctx.now)?;
        let (items, gathered, model, fallback) = self.generate(ctx)?;
        ctx.cancel.check()?;
        for it in &items {
            store.save(&it.title, &it.content, &it.refs, &model, false, &ctx.now)?;
        }
        if !items.is_empty() {
            let summary = items.iter().map(|i| format!("- {} 〔{}〕", i.content, i.refs.join(","))).collect::<Vec<_>>().join("\n");
            WorkerOutputs::new(ctx.db).add("feed", &format!("每日动态 {date}（{n} 条）", n = items.len()), &summary, &ctx.now)?;
            deliver(
                ctx.db,
                &self.attention,
                ctx.job,
                &format!("{date} 每日动态已生成（{} 条），`hatch feed list` 查看", items.len()),
                &key,
                &ctx.now,
            )?;
        }
        ctx.dedup_mark(&key)?;
        Ok(JobOutcome::Done {
            summary: format!(
                "{date} 动态：事实 {} 条，条目 {} 条{}",
                gathered.facts.len(),
                items.len(),
                fallback.as_ref().map(|f| format!("；{f}")).unwrap_or_default()
            ),
        })
    }
}

/// Attention for a goal, from the database: unread worker output linked
/// to the goal counts. The rule itself lives in [`goals::attention_level`]
/// (unread linked output, else recency of the goal's own update).
pub fn goal_attention(db: &Db, store: &GoalStore, goal_id: &str, now: &Timestamp) -> Result<crate::goals::Attention> {
    let goal = store.get(goal_id)?;
    let unread = WorkerOutputs::new(db).unread_count_for_goal(goal_id)?;
    Ok(crate::goals::attention_level(goal.status, unread, &goal.updated_at, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_llm::scripted::ScriptedModel;
    use hatch_llm::types::ChatResponse;
    use std::sync::Arc;

    fn fact(id: &str) -> Fact {
        Fact { id: id.into(), kind: facts::FactKind::Mail, text: "一封邮件".into(), source: "mail:x".into() }
    }

    #[test]
    fn feed_items_always_cite_known_facts() {
        let facts = vec![fact("F1"), fact("F2")];
        let v = serde_json::json!({"items": [
            {"text": "有依据", "refs": ["F1"]},
            {"text": "引用了不存在的事实", "refs": ["F9"]},
            {"text": "没有引用", "refs": []},
            {"text": "", "refs": ["F2"]},
            {"text": "两个依据都对", "refs": ["F1", "F2"]}
        ]});
        let (items, dropped) = validate(&v, &facts);
        assert_eq!(items.len(), 2);
        assert_eq!(dropped, 3);
        assert_eq!(items[0].refs, vec!["F1"]);
        assert_eq!(items[1].refs, vec!["F1", "F2"]);
    }

    #[test]
    fn intro_item_text_is_fixed_and_claims_nothing_personal() {
        assert!(INTRO_TEXT.contains("标明了来源"));
        assert!(!INTRO_TEXT.contains("我记得") && !INTRO_TEXT.contains("你喜欢"));
        let db = Db::open_in_memory().unwrap();
        let store = FeedStore::new(&db);
        let now = hatch_core::time::now();
        assert!(store.ensure_intro(&now).unwrap());
        assert!(!store.ensure_intro(&now).unwrap(), "intro is inserted exactly once");
        let items = store.list(10).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_intro);
        assert_eq!(items[0].content, INTRO_TEXT);
        assert!(items[0].source_facts.is_empty(), "intro is the only item allowed without source facts");
    }

    #[test]
    fn fallback_items_cite_their_own_fact() {
        let g = Gathered { facts: vec![fact("F1")], gaps: vec![] };
        let items = fallback_items(&g);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].refs, vec!["F1"]);
    }

    #[test]
    fn compose_validates_model_output_and_falls_back() {
        let g = Gathered { facts: vec![fact("F1")], gaps: vec![] };
        let model = Arc::new(ScriptedModel::ok("m", vec![ChatResponse::text(
            r#"{"items": [{"text": "模型写的", "refs": ["F1"]}, {"text": "编的", "refs": ["F7"]}]}"#,
        )]));
        let router = Router::single(model);
        let (items, _, fallback) = compose(Some(&router), "prompt", &g, &CancelToken::new()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "模型写的");
        assert!(fallback.unwrap().contains("丢弃无依据条目 1"));
        let (items, _, _) = compose(None, "prompt", &g, &CancelToken::new()).unwrap();
        assert_eq!(items.len(), 1, "no model: plain fact list");
    }
}
