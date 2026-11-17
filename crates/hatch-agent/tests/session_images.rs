//! An image message survives session persistence: append, reload,
//! transcript view, and fork all carry the picture.

use hatch_agent::SessionStore;
use hatch_core::Trust;
use hatch_llm::ChatMessage;
use hatch_store::Db;

#[test]
fn image_message_round_trips_through_the_store() {
    let db = Db::open_in_memory().unwrap();
    let s = SessionStore::new(&db);
    let now = hatch_core::time::now();
    let id = s.create("t", &now).unwrap();
    let shot =
        ChatMessage::tool_with_image("c1", "截图已保存：/tmp/a.png", "aGVsbG8=", "image/png");
    s.append(&id, &shot, Trust::System, &now).unwrap();
    s.append(&id, &ChatMessage::user("之后的问题"), Trust::User, &now).unwrap();
    let (_, msgs) = s.active(&id).unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].message.text_content(), "截图已保存：/tmp/a.png");
    assert_eq!(
        msgs[0].message.images().collect::<Vec<_>>(),
        vec![("aGVsbG8=", "image/png")]
    );
    // The transcript shows the text, never the stored JSON envelope.
    let rows = s.transcript(&id).unwrap();
    assert_eq!(rows[0].content, "截图已保存：/tmp/a.png");
    assert!(!rows[0].content.contains("\"v\":1"));
    // A fork copies the image too (raw columns move together).
    let f = s.fork(&id, &now).unwrap();
    assert_eq!(s.active(&f).unwrap(), s.active(&id).unwrap());
}
