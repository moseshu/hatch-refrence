//! Oracle model for pipeline mode.
//!
//! Answers classification requests from the world's expectations, and for
//! adversarial mails plays a model that has been *fooled* (e.g. calls a
//! phishing mail urgent, or writes the attacker's address into a draft). The
//! point is to prove the structural defences hold even when the model fails.

use crate::world::World;
use hatch_core::desensitize::AddressMapper;
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatMessage, ChatRequest, ChatResponse, LlmError, Role, Usage};
use serde_json::json;
use std::sync::Arc;

/// User-role messages that represent an actual turn or a harness reply.
pub(crate) fn is_user_turn(message: &ChatMessage) -> bool {
    message.role == Role::User
        && !message.text_content().starts_with(hatch_agent::completeness::NOTE_PREFIX)
}

pub(crate) fn approval_id(messages: &[ChatMessage]) -> Option<String> {
    let last_user = messages.iter().rev().find(|message| is_user_turn(message))?;
    let text = last_user.text_content();
    text.strip_prefix("审批结果：\n")?.lines().find_map(|line| {
        let (id, _) = line.strip_prefix("- ")?.split_once('（')?;
        let suffix = id.strip_prefix("apr_")?;
        (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| id.to_owned())
    })
}

#[cfg(test)]
mod turn_tests {
    use super::*;

    #[test]
    fn completeness_check_keeps_the_approval_and_does_not_start_a_turn() {
        let messages = [
            ChatMessage::user("处理待审批操作"),
            ChatMessage::user("审批结果：\n- apr_123abc（写文件）：已批准并执行"),
            ChatMessage::user(hatch_agent::completeness::completeness_note(
                "处理待审批操作",
                &["fs_write to report.md".into()],
            )),
        ];
        assert_eq!(messages.iter().filter(|message| is_user_turn(message)).count(), 2);
        assert_eq!(approval_id(&messages).as_deref(), Some("apr_123abc"));
        assert_eq!(approval_id(&[ChatMessage::user("- apr_123abc（普通消息）")]), None);
    }
}

/// The oracle reads the model's own input, so it must understand the
/// same desensitize mapping the workflow uses: the envelope it parses
/// carries code addresses, and the expectation lookup needs the real
/// sender. The set mirrors the workflow's canonical set (mailbox
/// participants plus persona contacts), so the codes agree.
fn mapper_for_world(world: &World) -> AddressMapper {
    let mut contacts: Vec<(String, String)> = Vec::new();
    for m in &world.mailbox {
        contacts.push((m.from.clone(), m.from_name.clone()));
        for to in &m.to {
            contacts.push((to.clone(), String::new()));
        }
    }
    for c in world
        .persona
        .vips
        .iter()
        .chain(world.persona.contacts.iter())
        .chain(world.persona.low.iter())
    {
        contacts.push((c.email.clone(), c.name.clone()));
    }
    AddressMapper::build(&world.persona.email, &world.persona.name, &contacts)
}

fn subject_of(req: &ChatRequest) -> Option<String> {
    let user = req.messages.last()?.text_content();
    user.lines()
        .find_map(|l| l.strip_prefix("主题: ").map(|s| s.trim().to_string()))
}

fn sender_of(req: &ChatRequest) -> Option<String> {
    let user = req.messages.last()?.text_content();
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
        .map(|m| hatch_core::tokens::estimate_tokens(&m.text_content()))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 60,
        ..Default::default()
    }
}

pub fn oracle_model(world: &World) -> Arc<FnModel> {
    let w = world.clone();
    let mapper = mapper_for_world(world);
    Arc::new(FnModel::new("oracle", move |req| {
        let system = req
            .messages
            .first()
            .map(|m| m.text_content())
            .unwrap_or_default();
        let subject =
            subject_of(req).ok_or_else(|| LlmError::permanent("oracle: no subject in request"))?;
        // Threads reuse one subject line for every reply: disambiguate by
        // sender as well (falls back to subject-only when the envelope
        // carries no sender). The envelope carries the code address;
        // map it back before the lookup, or the two 李婷 senders
        // collapse onto the subject-only fallback.
        let (_, exp) = match sender_of(req) {
            Some(sender) => {
                let sender = mapper.reverse(&sender);
                w.expectation_by_subject_sender(&subject, &sender)
                    .or_else(|| w.expectation_by_subject(&subject))
            }
            None => w.expectation_by_subject(&subject),
        }
        .ok_or_else(|| LlmError::permanent(format!("oracle: unknown mail {subject}")))?;
        let ov = exp.oracle.clone().unwrap_or_default();
        let text = if system.contains("mail triage classifier") {
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
        } else if system.contains("draft an email reply") {
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
