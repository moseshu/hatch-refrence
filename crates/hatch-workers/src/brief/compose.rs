//! Turning facts into a brief: the model orders and phrases, code enforces
//! that every item cites existing facts, lists whatever the model left out,
//! and states coverage gaps itself.

use super::facts::{Fact, FactKind, Gathered};
use hatch_core::trust::wrap_untrusted;
use hatch_core::{CancelToken, Error, Result};
use hatch_llm::json_extract::extract_json;
use hatch_llm::{ChatMessage, ChatRequest, Router, Workload};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

pub const TEMPLATE: &str = include_str!("../../../../prompts/brief_zh.md");
const MAX_ITEM_CHARS: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct BriefItem {
    pub text: String,
    pub refs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BriefSection {
    pub title: String,
    pub items: Vec<BriefItem>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Composed {
    pub sections: Vec<BriefSection>,
    /// Items the model wrote (before validation).
    pub model_items: usize,
    /// Items dropped for missing or unknown references.
    pub dropped: usize,
    pub model: String,
    /// Why the model was not used (no model configured, call failed, bad output).
    pub fallback: Option<String>,
}

fn prompt(user_name: &str, date: &str, facts: &[Fact]) -> ChatRequest {
    let system = TEMPLATE.replace("{user_name}", user_name).replace("{date}", date);
    let list: Vec<String> = facts.iter().map(|f| format!("{} [{}] {}", f.id, f.kind.label(), f.text)).collect();
    let user = wrap_untrusted("brief_facts", &list.join("\n"));
    let mut req = ChatRequest::new(vec![ChatMessage::system(system), ChatMessage::user(user)]);
    req.json_mode = true;
    req.temperature = Some(0.2);
    req
}

/// Parse and validate the model's JSON against the known fact ids.
pub fn validate(v: &Value, facts: &[Fact]) -> (Vec<BriefSection>, usize, usize) {
    let known: BTreeSet<&str> = facts.iter().map(|f| f.id.as_str()).collect();
    let (mut seen, mut dropped) = (0, 0);
    let mut sections = Vec::new();
    for s in v.get("sections").and_then(Value::as_array).cloned().unwrap_or_default() {
        let title: String = s.get("title").and_then(Value::as_str).unwrap_or("").trim().chars().take(30).collect();
        let mut items = Vec::new();
        for it in s.get("items").and_then(Value::as_array).cloned().unwrap_or_default() {
            seen += 1;
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
            items.push(BriefItem { text: hatch_core::secrets::redact(&text), refs });
        }
        if !items.is_empty() {
            sections.push(BriefSection { title: if title.is_empty() { "事项".into() } else { title }, items });
        }
    }
    (sections, seen, dropped)
}

/// Ask the brief model; any failure other than cancellation falls back to a
/// plain list (the brief is still produced, and says why).
pub fn compose(router: Option<&Router>, user_name: &str, date: &str, g: &Gathered, cancel: &CancelToken) -> Result<Composed> {
    if g.facts.is_empty() {
        return Ok(Composed::default());
    }
    let Some(router) = router else {
        return Ok(Composed { fallback: Some("没有配置模型".into()), ..Default::default() });
    };
    match router.chat_with(Workload::Brief, &prompt(user_name, date, &g.facts), Some(cancel)) {
        Err(e) if e.is_cancelled() => Err(Error::Cancelled(e.message)),
        Err(e) => Ok(Composed { fallback: Some(format!("模型调用失败：{}", e.message)), ..Default::default() }),
        Ok(r) => {
            let text = r.content.unwrap_or_default();
            match extract_json(&text) {
                None => Ok(Composed { fallback: Some("模型输出不是 JSON".into()), model: r.model, ..Default::default() }),
                Some(v) => {
                    let (sections, model_items, dropped) = validate(&v, &g.facts);
                    let fallback = sections.is_empty().then(|| "模型输出没有任何有依据的条目".to_string());
                    Ok(Composed { sections, model_items, dropped, model: r.model, fallback })
                }
            }
        }
    }
}

/// The brief as Markdown. Everything after the model's sections is written by code.
pub fn render(date: &str, g: &Gathered, c: &Composed) -> String {
    let mut s = format!("# 每日简报 · {date}\n\n");
    if g.facts.is_empty() {
        s.push_str("无新事项。\n");
    } else {
        let count = |k: FactKind| g.facts.iter().filter(|f| f.kind == k).count();
        s.push_str(&format!(
            "共 {} 条事实：邮件 {}，日程 {}，冲突 {}，待审批 {}，提醒 {}。\n",
            g.facts.len(),
            count(FactKind::Mail),
            count(FactKind::Event),
            count(FactKind::Conflict),
            count(FactKind::Approval),
            count(FactKind::Reminder)
        ));
    }
    if let Some(why) = &c.fallback {
        if !g.facts.is_empty() {
            s.push_str(&format!("\n（{why}，以下按类别列出事实原文。）\n"));
        }
    }
    let mut cited = BTreeSet::new();
    for sec in &c.sections {
        s.push_str(&format!("\n## {}\n", sec.title));
        for it in &sec.items {
            s.push_str(&format!("- {} 〔{}〕\n", it.text, it.refs.join(",")));
            cited.extend(it.refs.iter().cloned());
        }
    }
    let rest: Vec<&Fact> = g.facts.iter().filter(|f| !cited.contains(&f.id)).collect();
    if !rest.is_empty() {
        s.push_str(if c.sections.is_empty() { "\n## 事项\n" } else { "\n## 其他事项（简报正文未提及，原样列出）\n" });
        for f in rest {
            s.push_str(&format!("- [{}] {} 〔{}〕\n", f.kind.label(), f.text, f.id));
        }
    }
    if !g.gaps.is_empty() {
        s.push_str("\n## 覆盖缺口\n");
        for gap in &g.gaps {
            s.push_str(&format!("- {gap}\n"));
        }
    }
    if !c.sections.is_empty() {
        s.push_str("\n## 依据\n");
        for f in &g.facts {
            s.push_str(&format!("- {} [{}] {}\n", f.id, f.kind.label(), f.text));
        }
    }
    let by = if c.model.is_empty() { "程序（未使用模型）".to_string() } else { c.model.clone() };
    s.push_str(&format!("\n---\n生成：{by}；模型条目 {}，因缺少依据丢弃 {}。\n", c.model_items, c.dropped));
    s
}
