//! Low-cost protocol smoke test against a real endpoint.
//!
//! ```text
//! HATCH_LLM_BASE_URL=... HATCH_LLM_MODEL=... HATCH_LLM_API_KEY=... \
//!   cargo run -p hatch-llm --example probe -- chat responses
//! ```
//!
//! Protocols: `chat`, `responses`, `anthropic`, `gemini`. For each one it
//! makes three small requests (`max_tokens` 200): buffered, streaming, and a
//! buffered request that should produce a tool call. The key is read from the
//! environment and never printed.

use hatch_llm::provider::{AnthropicMessages, Gemini, OpenAiCompatible, OpenAiResponses};
use hatch_llm::{ChatMessage, ChatModel, ChatRequest, ChatResponse, LlmError, StreamEvent, ToolSpec};
use serde_json::json;
use std::time::Instant;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"))
}

fn show(label: &str, started: Instant, r: &Result<ChatResponse, LlmError>, events: Option<(usize, usize)>) {
    let ms = started.elapsed().as_millis();
    match r {
        Ok(r) => {
            let text: String = r.content.clone().unwrap_or_default().chars().take(60).collect();
            let calls: Vec<String> = r.tool_calls.iter().map(|c| format!("{}({})", c.name, c.arguments)).collect();
            let u = r.usage;
            let ev = events.map(|(d, t)| format!(" | stream deltas={d} tool_events={t}")).unwrap_or_default();
            println!(
                "{label:<28} OK   {ms:>6}ms | text={text:?} calls={calls:?} | tokens in={} out={} reasoning={} cached={}{ev}",
                u.prompt_tokens, u.completion_tokens, u.reasoning_tokens, u.cached_tokens
            );
        }
        Err(e) => {
            let msg: String = e.message.chars().take(220).collect();
            println!("{label:<28} FAIL {ms:>6}ms | {:?}: {msg}", e.kind);
        }
    }
}

fn main() {
    let (base, model, key) = (env("HATCH_LLM_BASE_URL"), env("HATCH_LLM_MODEL"), env("HATCH_LLM_API_KEY"));
    let protocols: Vec<String> = std::env::args().skip(1).collect();
    let protocols = if protocols.is_empty() { vec!["chat".to_string(), "responses".to_string()] } else { protocols };
    let mut plain = ChatRequest::new(vec![
        ChatMessage::system("你是简洁的助手。"),
        ChatMessage::user("用一句话（不超过15个字）说明什么是邮件分诊。"),
    ]);
    plain.max_tokens = Some(200);
    let mut tool = ChatRequest::new(vec![
        ChatMessage::system("需要查询日程时必须调用工具，不要猜。"),
        ChatMessage::user("我明天有哪些会议？"),
    ]);
    tool.max_tokens = Some(200);
    tool.tools.push(ToolSpec {
        name: "calendar_list".into(),
        description: "列出未来若干天的日程".into(),
        parameters: json!({"type": "object", "properties": {"days": {"type": "integer", "description": "天数"}}, "required": ["days"]}),
    });
    for p in protocols {
        let m: Box<dyn ChatModel> = match p.as_str() {
            "chat" => Box::new(OpenAiCompatible::new("probe", &base, &model, key.clone())),
            "responses" => Box::new(OpenAiResponses::new("probe", &base, &model, key.clone())),
            "anthropic" => Box::new(AnthropicMessages::new("probe", &base, &model, key.clone())),
            "gemini" => Box::new(Gemini::new("probe", &base, &model, key.clone())),
            other => {
                println!("unknown protocol {other}");
                continue;
            }
        };
        let t = Instant::now();
        show(&format!("{p} buffered"), t, &m.chat(&plain), None);
        let t = Instant::now();
        let (mut deltas, mut tools) = (0, 0);
        let r = m.chat_stream(&plain, &mut |e| match e {
            StreamEvent::TextDelta(_) => deltas += 1,
            StreamEvent::ToolCallStarted { .. } => tools += 1,
            StreamEvent::Done => {}
        });
        show(&format!("{p} stream"), t, &r, Some((deltas, tools)));
        let t = Instant::now();
        show(&format!("{p} tool-call"), t, &m.chat(&tool), None);
    }
}
