//! A user turn may carry images: they land on the user message, reach
//! the model request, and persist in the session history.

mod common;

use common::{model, Harness};
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore};
use hatch_llm::{ChatResponse, Role};

#[test]
fn user_images_reach_the_model_and_the_history() {
    let h = Harness::new();
    let (router, seen) = model(|_| ChatResponse::text("看到了"));
    let agent = Agent {
        db: &h.db,
        router: &router,
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: AgentConfig::default(),
    };
    let now = hatch_core::time::now();
    let s = SessionStore::new(&h.db).create("t", &now).unwrap();
    let opts = RunOptions {
        images: vec![("aGVsbG8=".to_string(), "image/png".to_string())],
        ..Default::default()
    };
    let r = agent.run(&s, "看这张图", &now, opts).unwrap();
    assert_eq!(r.answer.as_deref(), Some("看到了"));
    // The model request's user message carries the image part.
    let reqs = seen.lock().unwrap();
    let user = reqs[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .expect("user message");
    assert_eq!(user.text_content(), "看这张图");
    assert_eq!(
        user.images().collect::<Vec<_>>(),
        vec![("aGVsbG8=", "image/png")]
    );
    // And it persisted with the session.
    let (_, history) = SessionStore::new(&h.db).active(&s).unwrap();
    assert!(history
        .iter()
        .any(|m| m.message.images().count() == 1));
}

#[test]
fn text_only_turn_has_no_image_parts() {
    let h = Harness::new();
    let (router, seen) = model(|_| ChatResponse::text("好"));
    let agent = Agent {
        db: &h.db,
        router: &router,
        tools: &h.tools,
        executors: &h.exec,
        gate: &h.gate,
        config: AgentConfig::default(),
    };
    let now = hatch_core::time::now();
    let s = SessionStore::new(&h.db).create("t", &now).unwrap();
    agent.run(&s, "你好", &now, RunOptions::default()).unwrap();
    let reqs = seen.lock().unwrap();
    assert!(reqs[0].messages.iter().all(|m| m.images().count() == 0));
}
