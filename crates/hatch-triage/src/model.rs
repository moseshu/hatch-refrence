//! Triage data model and validation of classifier output.

use hatch_core::time::{parse_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_policy::Priority;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Urgent,
    Action,
    Reply,
    Fyi,
    Newsletter,
    Notification,
    Spam,
    /// Classifier failed or returned something invalid.
    Unclassified,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Urgent,
        Category::Action,
        Category::Reply,
        Category::Fyi,
        Category::Newsletter,
        Category::Notification,
        Category::Spam,
        Category::Unclassified,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Urgent => "urgent",
            Category::Action => "action",
            Category::Reply => "reply",
            Category::Fyi => "fyi",
            Category::Newsletter => "newsletter",
            Category::Notification => "notification",
            Category::Spam => "spam",
            Category::Unclassified => "unclassified",
        }
    }

    pub fn label_zh(&self) -> &'static str {
        match self {
            Category::Urgent => "紧急",
            Category::Action => "待办",
            Category::Reply => "待回复",
            Category::Fyi => "知会",
            Category::Newsletter => "订阅",
            Category::Notification => "通知",
            Category::Spam => "垃圾",
            Category::Unclassified => "未分类",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s.trim().to_ascii_lowercase())
    }

    /// Categories for which drafting a reply never makes sense.
    pub fn never_reply(&self) -> bool {
        matches!(
            self,
            Category::Newsletter | Category::Notification | Category::Spam | Category::Unclassified
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractedEvent {
    pub title: String,
    pub start: Timestamp,
    pub end: Option<Timestamp>,
    pub is_request: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriageResult {
    pub mail_id: String,
    pub category: Category,
    pub priority: Priority,
    pub summary: String,
    pub needs_reply: bool,
    pub reply_hint: String,
    pub events: Vec<ExtractedEvent>,
    pub suspicious: bool,
    /// Why code adjusted the model's answer, injection reasons, errors.
    pub notes: Vec<String>,
    pub model: String,
}

impl TriageResult {
    pub fn unclassified(mail_id: &str, why: &str, model: &str) -> Self {
        TriageResult {
            mail_id: mail_id.to_string(),
            category: Category::Unclassified,
            priority: Priority::Normal,
            summary: String::new(),
            needs_reply: false,
            reply_hint: String::new(),
            events: vec![],
            suspicious: false,
            notes: vec![format!("分类失败：{why}")],
            model: model.to_string(),
        }
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Validate classifier JSON. Unknown or malformed fields are an error, not a guess.
pub fn from_model_json(mail_id: &str, v: &Value, model: &str) -> Result<TriageResult> {
    let category = Category::parse(&s(v, "category"))
        .filter(|c| *c != Category::Unclassified)
        .ok_or_else(|| Error::Invalid(format!("bad category {:?}", v.get("category"))))?;
    let priority = Priority::parse(&s(v, "priority").to_ascii_lowercase())?;
    let summary: String = s(v, "summary").chars().take(200).collect();
    let needs_reply = v
        .get("needs_reply")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::Invalid("needs_reply must be boolean".into()))?;
    let mut events = Vec::new();
    for e in v
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let start = s(&e, "start");
        let Ok(start) = parse_rfc3339(&start) else {
            continue;
        };
        let end = parse_rfc3339(&s(&e, "end")).ok().filter(|t| *t > start);
        events.push(ExtractedEvent {
            title: s(&e, "title").chars().take(100).collect(),
            start,
            end,
            is_request: e
                .get("is_request")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    let suspicious = v
        .get("suspicious")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut notes = Vec::new();
    let reason = s(v, "suspicious_reason");
    if suspicious && !reason.is_empty() {
        notes.push(format!("模型判定可疑：{reason}"));
    }
    Ok(TriageResult {
        mail_id: mail_id.to_string(),
        category,
        priority,
        summary,
        needs_reply,
        reply_hint: s(v, "reply_hint").chars().take(200).collect(),
        events,
        suspicious,
        notes,
        model: model.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_and_invalid() {
        let v = json!({"category": "Reply", "priority": "high", "summary": "约周三开会", "needs_reply": true,
            "reply_hint": "确认时间", "events": [{"title": "评审", "start": "2026-11-04T15:00:00+08:00", "end": "", "is_request": true},
            {"title": "bad", "start": "明天"}], "suspicious": false});
        let r = from_model_json("m1", &v, "x").unwrap();
        assert_eq!(r.category, Category::Reply);
        assert_eq!(r.events.len(), 1);
        assert!(r.events[0].end.is_none());
        assert!(from_model_json(
            "m1",
            &json!({"category": "weird", "priority": "high", "needs_reply": true}),
            "x"
        )
        .is_err());
        assert!(from_model_json(
            "m1",
            &json!({"category": "fyi", "priority": "high", "needs_reply": "yes"}),
            "x"
        )
        .is_err());
        assert!(from_model_json(
            "m1",
            &json!({"category": "unclassified", "priority": "low", "needs_reply": false}),
            "x"
        )
        .is_err());
        assert!(Category::Spam.never_reply());
    }
}
