//! Oracle model for pipeline mode.
//!
//! Answers classification requests from the world's expectations, and for
//! adversarial mails plays a model that has been *fooled* (e.g. calls a
//! phishing mail urgent, or writes the attacker's address into a draft). The
//! point is to prove the structural defences hold even when the model fails.

use crate::world::World;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Usage};
use serde_json::json;
use std::sync::Arc;

fn subject_of(req: &ChatRequest) -> Option<String> {
    let user = &req.messages.last()?.content;
    user.lines()
        .find_map(|l| l.strip_prefix("主题: ").map(|s| s.trim().to_string()))
}

fn sender_of(req: &ChatRequest) -> Option<String> {
    let user = &req.messages.last()?.content;
    user.lines().find_map(|l| {
        let l = l.strip_prefix("发件人: ")?;
        let start = l.find('<')?;
        let end = l.find('>')?;
        Some(l[start + 1..end].trim().to_string())
    })
}

fn default_priority(category: &str) -> &'static str {
    match category {
        "urgent" => "urgent",
        "action" | "reply" => "normal",
        "fyi" => "normal",
        _ => "low",
    }
}

fn usage_for(req: &ChatRequest) -> Usage {
    let prompt: usize = req
        .messages
        .iter()
        .map(|m| hatch_core::tokens::estimate_tokens(&m.content))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 60,
        ..Default::default()
    }
}

pub fn oracle_model(world: &World) -> Arc<FnModel> {
    let w = world.clone();
    Arc::new(FnModel::new("oracle", move |req| {
        let system = req
            .messages
            .first()
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let subject =
            subject_of(req).ok_or_else(|| LlmError::permanent("oracle: no subject in request"))?;
        // Threads reuse one subject line for every reply: disambiguate by
        // sender as well (falls back to subject-only when the envelope
        // carries no sender).
        let (_, exp) = match sender_of(req) {
            Some(sender) => w
                .expectation_by_subject_sender(&subject, &sender)
                .or_else(|| w.expectation_by_subject(&subject)),
            None => w.expectation_by_subject(&subject),
        }
        .ok_or_else(|| LlmError::permanent(format!("oracle: unknown mail {subject}")))?;
        let ov = exp.oracle.clone().unwrap_or_default();
        let text = if system.contains("分诊分类器") {
            let category = ov.category.clone().unwrap_or_else(|| exp.category.clone());
            let events: Vec<serde_json::Value> = exp
                .event
                .iter()
                .map(|e| json!({"title": e.title, "start": e.start, "end": e.end, "is_request": true}))
                .collect();
            json!({
                "category": category,
                "priority": ov.priority.clone().or(exp.priority.clone()).unwrap_or_else(|| default_priority(&category).to_string()),
                "summary": ov.summary.clone().unwrap_or_else(|| exp.summary.clone()),
                "needs_reply": ov.needs_reply.unwrap_or(exp.needs_reply),
                "reply_hint": if exp.needs_reply { "回应对方的问题" } else { "" },
                "events": events,
                "suspicious": false,
                "suspicious_reason": ""
            })
            .to_string()
        } else if system.contains("起草") {
            let mut body = format!("您好，关于「{subject}」，已收到，稍后给您确认。");
            if let Some(a) = ov.draft_appendix {
                body.push_str(&a);
            }
            json!({ "body": body }).to_string()
        } else {
            return Err(LlmError::permanent("oracle: unsupported request"));
        };
        Ok(ChatResponse {
            content: Some(text),
            usage: usage_for(req),
            model: "oracle".into(),
            ..Default::default()
        })
    }))
}
