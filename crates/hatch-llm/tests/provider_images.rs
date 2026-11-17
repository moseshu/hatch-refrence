//! Image content parts reach every provider's wire format.
//!
//! Each test builds a text+image request and asserts the exact JSON
//! shape `build_body` produces. No network is involved.

use hatch_llm::provider::{AnthropicMessages, Gemini, OpenAiCompatible, OpenAiResponses};
use hatch_llm::{ChatMessage, ChatRequest, ToolCall};
use serde_json::json;

fn image_request() -> ChatRequest {
    ChatRequest::new(vec![
        ChatMessage::user("看这张图").with_image("aGVsbG8=", "image/png"),
        ChatMessage::assistant_tools(
            "",
            vec![ToolCall { id: "c1".into(), name: "t".into(), arguments: json!({}) }],
        ),
        ChatMessage::tool_with_image("c1", "截图已保存", "eHk=", "image/png"),
    ])
}

#[test]
fn openai_chat_body_carries_image_parts() {
    let p = OpenAiCompatible::new("c", "https://api.example.com/v1/", "m", "secret".into());
    let b = p.build_body(&image_request());
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    // User message: text + image_url parts.
    assert_eq!(msgs[0]["content"][0], json!({"type": "text", "text": "看这张图"}));
    assert_eq!(
        msgs[0]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}})
    );
    // Assistant message carries the tool call; tool result is text-only
    // and the image follows as its own captioned user message.
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "c1");
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["content"], "截图已保存");
    assert_eq!(msgs[3]["role"], "user");
    assert_eq!(
        msgs[3]["content"][1],
        json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,eHk="}})
    );
}

#[test]
fn openai_responses_body_carries_image_parts() {
    let p = OpenAiResponses::new("r", "https://api.example.com/v1/", "m", "secret".into());
    let b = p.build_body(&image_request());
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 4);
    // User message: input_text + input_image parts.
    assert_eq!(input[0]["content"][0], json!({"type": "input_text", "text": "看这张图"}));
    assert_eq!(
        input[0]["content"][1],
        json!({"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="})
    );
    // Tool result stays a function_call_output; the image follows as
    // its own user message item.
    assert_eq!(
        input[2],
        json!({"type": "function_call_output", "call_id": "c1", "output": "截图已保存"})
    );
    assert_eq!(input[3]["type"], "message");
    assert_eq!(
        input[3]["content"][1],
        json!({"type": "input_image", "image_url": "data:image/png;base64,eHk="})
    );
}

#[test]
fn gemini_body_carries_image_parts() {
    let g = Gemini::new("g", "", "models/gemini-x", "secret".into());
    let req = ChatRequest::new(vec![
        ChatMessage::user("看这张图").with_image("aGVsbG8=", "image/png"),
        ChatMessage::assistant_tools(
            "",
            vec![ToolCall {
                id: "gemini_call_0".into(),
                name: "browser_screenshot".into(),
                arguments: json!({}),
            }],
        ),
        ChatMessage::tool_with_image("gemini_call_0", "截图已保存", "eHk=", "image/jpeg"),
    ]);
    let b = g.build_body(&req);
    assert_eq!(b["contents"][0]["role"], "user");
    assert_eq!(b["contents"][0]["parts"][0], json!({"text": "看这张图"}));
    assert_eq!(
        b["contents"][0]["parts"][1],
        json!({"inline_data": {"mime_type": "image/png", "data": "aGVsbG8="}})
    );
    let last = b["contents"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["parts"][0]["functionResponse"]["name"], "browser_screenshot");
    assert_eq!(
        last["parts"][1],
        json!({"inline_data": {"mime_type": "image/jpeg", "data": "eHk="}})
    );
}

#[test]
fn anthropic_body_carries_image_parts() {
    let p = AnthropicMessages::new("a", "https://x", "m", "k".into());
    let req = ChatRequest::new(vec![
        ChatMessage::user("看这张图").with_image("aGVsbG8=", "image/png"),
        ChatMessage::assistant_tools(
            "",
            vec![ToolCall { id: "1".into(), name: "a".into(), arguments: json!({}) }],
        ),
        ChatMessage::tool_with_image("1", "截图已保存", "eHk=", "image/jpeg"),
    ]);
    let b = p.build_body(&req);
    assert_eq!(b["messages"][0]["content"][0], json!({"type": "text", "text": "看这张图"}));
    assert_eq!(
        b["messages"][0]["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}})
    );
    let tool_result = &b["messages"][2]["content"][0];
    assert_eq!(tool_result["type"], "tool_result");
    assert_eq!(tool_result["content"][0], json!({"type": "text", "text": "截图已保存"}));
    assert_eq!(
        tool_result["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "eHk="}})
    );
}
