//! A tool result's image attachment survives the dispatcher: the
//! `DispatchResult` carries the bytes next to the text, so the agent
//! loop can attach them to the next model call.

use hatch_core::CancelToken;
use hatch_llm::{ToolCall, ToolSpec};
use hatch_policy::{ActionClass, Gatekeeper, PolicyConfig};
use hatch_store::Db;
use hatch_tools::{dispatch_call, ExecutorRegistry, Registry, Tool, ToolContext, ToolOutput};
use serde_json::{json, Value};
use std::sync::Arc;

/// A stub tool that returns a picture with its text.
struct Shot;

impl Tool for Shot {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "fake_shot".into(), description: "d".into(), parameters: json!({"type": "object"}) }
    }
    fn class(&self) -> ActionClass {
        ActionClass::BrowseLocal
    }
    fn call(&self, _args: &Value, _ctx: &ToolContext) -> hatch_core::Result<ToolOutput> {
        Ok(ToolOutput::system("截图已保存：/tmp/a.png").with_image("eHk=", "image/png"))
    }
}

fn ctx(db: &Db) -> ToolContext<'_> {
    ToolContext {
        turn_id: String::new(),
        db,
        now: hatch_core::time::now(),
        tz: chrono_tz::Asia::Shanghai,
        user_email: "me@126.com".into(),
        user_name: String::new(),
        origin: "chat:s1".into(),
        tainted: false,
        call_id: "c1".into(),
        cancel: CancelToken::new(),
        intent_review: None,
    }
}

#[test]
fn tool_image_survives_dispatch() {
    let db = Db::open_in_memory().unwrap();
    let reg = Registry::new().with(Arc::new(Shot));
    let exec = ExecutorRegistry::new();
    let gate = Gatekeeper::new(PolicyConfig::default());
    let call =
        ToolCall { id: "c".into(), name: "fake_shot".into(), arguments: json!({}) };
    let r = dispatch_call(&reg, &exec, &gate, &call, &ctx(&db));
    assert_eq!(r.content, "截图已保存：/tmp/a.png");
    let img = r.image.expect("image must survive dispatch");
    assert_eq!(img.data, "eHk=");
    assert_eq!(img.media_type, "image/png");
}
