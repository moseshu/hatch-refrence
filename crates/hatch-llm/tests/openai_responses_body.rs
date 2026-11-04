//! `build_body` tests for the json_mode input guarantee.
//!
//! Bianxie's gateway rejects `text.format: json_object` unless an `input`
//! message contains the word "json" (it does not count `instructions`,
//! where system messages go). See `ensure_json_word_in_input` in
//! `openai_responses.rs` for the probe evidence.

use hatch_llm::provider::OpenAiResponses;
use hatch_llm::types::{ChatMessage, ChatRequest};
use serde_json::json;

fn provider() -> OpenAiResponses {
    OpenAiResponses::new("r", "https://api.example.com/v1/", "m", "secret".into())
}

fn user_req(content: &str, json_mode: bool) -> ChatRequest {
    let mut req = ChatRequest::new(vec![
        ChatMessage::system("只输出一个 JSON 对象。"),
        ChatMessage::user(content),
    ]);
    req.json_mode = json_mode;
    req
}

#[test]
fn json_mode_appends_json_instruction_to_last_user_input() {
    let p = provider();
    let b = p.build_body(&user_req("发件人：a@example.com\n主题：开会", true));
    assert_eq!(b["text"]["format"]["type"], "json_object");
    // The system prompt mentions JSON, but it lives in `instructions`,
    // not `input` — so the provider must append to the user message.
    assert_eq!(b["instructions"], "只输出一个 JSON 对象。");
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": "发件人：a@example.com\n主题：开会\n请以 JSON 对象回复。"})
    );
}

#[test]
fn json_mode_leaves_input_alone_when_json_already_present() {
    let p = provider();
    let b = p.build_body(&user_req("请以 JSON 格式回复这封邮件。", true));
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": "请以 JSON 格式回复这封邮件。"})
    );
}

#[test]
fn json_mode_off_leaves_input_untouched() {
    let p = provider();
    let b = p.build_body(&user_req("发件人：a@example.com", false));
    assert!(b.get("text").is_none());
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": "发件人：a@example.com"})
    );
}

#[test]
fn json_mode_adds_a_user_message_when_none_exists() {
    let p = provider();
    let mut req = ChatRequest::new(vec![ChatMessage::system("只输出一个 JSON 对象。")]);
    req.json_mode = true;
    let b = p.build_body(&req);
    assert_eq!(b["input"].as_array().unwrap().len(), 1);
    assert_eq!(
        b["input"][0],
        json!({"role": "user", "content": "请以 JSON 对象回复。"})
    );
}

#[test]
fn json_mode_appends_to_last_user_message_not_first() {
    let p = provider();
    let mut req = ChatRequest::new(vec![
        ChatMessage::system("只输出一个 JSON 对象。"),
        ChatMessage::user("第一封邮件"),
        ChatMessage::assistant("收到"),
        ChatMessage::user("第二封邮件"),
    ]);
    req.json_mode = true;
    let b = p.build_body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input[0]["content"], "第一封邮件");
    assert_eq!(input[2]["content"], "第二封邮件\n请以 JSON 对象回复。");
}
