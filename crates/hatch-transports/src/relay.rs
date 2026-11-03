//! The worker-relay rule (Muse deep-dive §9.4; roadmap R13/RW-16).
//!
//! Evidence (E-behaviour): in an external side chat a "worker" talks
//! to other people on the user's behalf; the worker's messages go
//! through the **main agent's relay** to the user, and the worker
//! cannot decide for the user.
//!
//! What that means here, as code:
//!
//! * A worker only **prepares** content: a [`Draft`] (text + facts +
//!   labels + the capability it would need). A draft is inert — there
//!   is no method that sends a draft on its own.
//! * Only the main agent relays: [`Relay::deliver_draft`] requires a
//!   [`MainDecision`]. The decision is constructed only by main-agent
//!   code (the turn runner / CLI); a worker never decides, never
//!   approves, and the decision may rewrite the text (the main agent
//!   relays in its own words).
//! * A draft never passes on private info beyond the chat's rules:
//!   the relay runs the discretion check on the **final** (possibly
//!   rewritten) text before anything is sent, exactly as it does for
//!   the main agent's own turn answers.
//! * A draft that needs a device capability the chat's device does
//!   not have is not relayed: the reply is the honest
//!   capability-missing response instead (see [`capability`](crate::capability)).
//!
//! The main agent's own turn answers take the shorter path
//! ([`Relay::deliver_main`]): the turn *is* the main agent deciding,
//! so no separate decision object is needed — but the discretion
//! check, widget rendering and artifact delivery still run.

use crate::capability::{missing_reply, CapabilityManifest};
use crate::chats::ChatStore;
use crate::discretion::{check_outbound, CheckOutcome};
use crate::message::{ContentKind, InboundMessage, OutboundMessage};
use crate::transport::{Transport, TurnOutcome};
use hatch_artifacts::store::ArtifactStore;
use hatch_artifacts::widget::Widget;
use hatch_core::{ids, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Content a worker prepared for one chat. Inert until the main agent
/// relays it. `facts` are the worker's sources for the main agent's
/// decision; they are never sent to the chat.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub chat_id: String,
    pub text: String,
    #[serde(default)]
    pub labels: Vec<ContentKind>,
    #[serde(default)]
    pub widget: Option<serde_json::Value>,
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// Which worker prepared this (a sub-agent run id).
    pub prepared_by: String,
    /// The facts the draft rests on, for the main agent — not sent.
    #[serde(default)]
    pub facts: Vec<String>,
    /// Device capability the draft would need (e.g. `computer.control`).
    #[serde(default)]
    pub needs_capability: Option<String>,
}

/// The main agent's decision about a draft. Constructed only by
/// main-agent code; workers cannot construct one through any API in
/// this crate. `final_text` lets the main agent relay in its own
/// words instead of forwarding the draft verbatim.
#[derive(Clone, Debug)]
pub struct MainDecision {
    pub approved: bool,
    pub final_text: Option<String>,
    pub note: String,
}

impl MainDecision {
    /// Relay the draft (as-is, or as `final_text` when given).
    pub fn approve(note: impl Into<String>) -> Self {
        MainDecision { approved: true, final_text: None, note: note.into() }
    }

    /// Relay the draft rewritten in the main agent's own words.
    pub fn approve_as(text: impl Into<String>, note: impl Into<String>) -> Self {
        MainDecision { approved: true, final_text: Some(text.into()), note: note.into() }
    }

    /// Do not relay.
    pub fn refuse(note: impl Into<String>) -> Self {
        MainDecision { approved: false, final_text: None, note: note.into() }
    }
}

/// What relaying a draft or a turn answer produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayOutcome {
    /// Sent. `redacted` is true when the discretion check rewrote part
    /// of the text before sending.
    Sent { message: OutboundMessage, redacted: bool },
    /// Nothing was sent. `reason` names the rule, never the content.
    Blocked { reason: String },
}

/// The only path from prepared content to a sent message.
pub struct Relay {
    transport: Arc<dyn Transport>,
    chats: ChatStore,
    artifacts_root: PathBuf,
    /// The device this relay serves (the pipe transport's local
    /// device). `None` = no capability checks on drafts.
    device: Option<CapabilityManifest>,
}

impl Relay {
    pub fn new(
        transport: Arc<dyn Transport>,
        chats: ChatStore,
        artifacts_root: PathBuf,
        device: Option<CapabilityManifest>,
    ) -> Self {
        Relay { transport, chats, artifacts_root, device }
    }

    /// A worker submits a draft for the main agent. It is stored under
    /// `chats/<chat-id>/drafts/<draft-id>.json` for the main agent to
    /// pick up — and **nothing is sent**. Returns the draft id.
    pub fn submit_draft(&self, draft: &Draft) -> Result<String> {
        let id = ids::random_id("dft");
        let dir = self.chats.chat_dir(&draft.chat_id)?.join("drafts");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{id}.json")), serde_json::to_string_pretty(draft)?)?;
        Ok(id)
    }

    /// Drafts still waiting for the main agent's decision.
    pub fn pending_drafts(&self, chat_id: &str) -> Result<Vec<(String, Draft)>> {
        let dir = self.chats.chat_dir(chat_id)?.join("drafts");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if let Ok(raw) = std::fs::read_to_string(e.path()) {
                if let Ok(d) = serde_json::from_str::<Draft>(&raw) {
                    out.push((id.to_string(), d));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Deliver the main agent's own turn answer. The turn itself is the
    /// main agent deciding, so no separate decision object is needed.
    pub fn deliver_main(&self, msg: &InboundMessage, outcome: &TurnOutcome) -> Result<RelayOutcome> {
        self.deliver(&msg.chat, outcome.text.clone(), &outcome.labels, outcome.widget.as_ref(), outcome.artifact_id.as_deref(), &outcome.pending_approvals)
    }

    /// Relay a worker's draft. Requires the main agent's decision; a
    /// refused decision sends nothing. The discretion check runs on the
    /// final text, and a missing device capability blocks the relay
    /// with the honest capability reply.
    pub fn deliver_draft(&self, draft: &Draft, decision: &MainDecision) -> Result<RelayOutcome> {
        if !decision.approved {
            return Ok(RelayOutcome::Blocked {
                reason: format!("主 agent 未批准转述（{}）", decision.note),
            });
        }
        if let (Some(want), Some(device)) = (&draft.needs_capability, &self.device) {
            if !device.supports(want) {
                return Ok(RelayOutcome::Blocked {
                    reason: missing_reply(device, want),
                });
            }
        }
        let text = decision.final_text.clone().unwrap_or_else(|| draft.text.clone());
        // Labels describe the draft. When the main agent rewrites it,
        // the rewrite is its own text: it is checked unlabelled (the
        // secret scan and the line patterns still run). A rewrite that
        // paraphrases mail content therefore passes the label check —
        // the main agent is accountable for what it relays, and the
        // label path cannot detect paraphrase (see SECURITY.md §9g).
        let labels: &[ContentKind] =
            if decision.final_text.is_some() { &[] } else { &draft.labels };
        self.deliver(&draft.chat_id, text, labels, draft.widget.as_ref(), draft.artifact_id.as_deref(), &[])
    }

    fn deliver(
        &self,
        chat_id: &str,
        text: String,
        labels: &[ContentKind],
        widget: Option<&serde_json::Value>,
        artifact_id: Option<&str>,
        pending_approvals: &[String],
    ) -> Result<RelayOutcome> {
        // Discretion first: nothing below may send before this passes.
        let rules = self.chats.read_discretion(chat_id)?;
        let (mut text, redacted) = match check_outbound(&rules, &text, labels) {
            CheckOutcome::Allow => (text, false),
            CheckOutcome::Block { reason } => return Ok(RelayOutcome::Blocked { reason }),
            CheckOutcome::Redact { text, .. } => (text, true),
        };
        // Widgets: this relay has no UI of its own, so the text fallback
        // is the rendering (Phase 4 `render(widget, token_present=false)`).
        if let Some(w) = widget {
            let widget = Widget::from_json(w)?;
            let rendered = hatch_artifacts::render(&widget, false);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&rendered.text);
        }
        // Artifact delivery is the single entry point: the delivered
        // message contains the absolute path.
        if let Some(id) = artifact_id {
            let store = ArtifactStore::new(&self.artifacts_root);
            match store.deliver(id) {
                Ok(message) => {
                    text.push('\n');
                    text.push_str(&message);
                }
                Err(e) => {
                    text.push_str(&format!("\n（artifact {id} 未能交付：{e}）"));
                }
            }
        }
        if !pending_approvals.is_empty() {
            text.push_str(&format!(
                "\n待你确认：{}（`hatch approvals show <id>` 查看）",
                pending_approvals.join(", ")
            ));
        }
        let message = OutboundMessage { chat_id: chat_id.to_string(), text, redacted };
        self.transport.send(&message)?;
        Ok(RelayOutcome::Sent { message, redacted })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{PollReport, TurnRunner};
    use std::path::Path;
    use std::sync::Mutex;

    /// A transport that only records what it was asked to send.
    struct RecordingTransport {
        sent: Mutex<Vec<OutboundMessage>>,
    }

    impl RecordingTransport {
        fn new() -> Self {
            RecordingTransport { sent: Mutex::new(Vec::new()) }
        }
    }

    impl Transport for RecordingTransport {
        fn name(&self) -> &str {
            "recording"
        }
        fn status(&self) -> crate::transport::TransportStatus {
            crate::transport::TransportStatus {
                name: "recording".into(),
                configured: true,
                detail: "test double".into(),
                guidance: String::new(),
            }
        }
        fn list_chats(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn send(&self, msg: &OutboundMessage) -> Result<String> {
            self.sent.lock().unwrap().push(msg.clone());
            Ok("recorded".into())
        }
        fn poll_once(
            &self,
            _runner: &dyn TurnRunner,
            _deliver: &dyn Fn(&InboundMessage, &TurnOutcome) -> Result<RelayOutcome>,
        ) -> Result<PollReport> {
            Ok(PollReport::default())
        }
    }

    fn relay(dir: &Path) -> (Arc<RecordingTransport>, Relay) {
        let t = Arc::new(RecordingTransport::new());
        let r = Relay::new(t.clone(), ChatStore::new(dir), dir.join("artifacts"), None);
        (t, r)
    }

    fn draft(chat: &str) -> Draft {
        Draft {
            chat_id: chat.into(),
            text: "草稿内容".into(),
            labels: vec![],
            widget: None,
            artifact_id: None,
            prepared_by: "sa_worker1".into(),
            facts: vec!["事实一".into()],
            needs_capability: None,
        }
    }

    #[test]
    fn draft_without_main_decision_does_not_send() {
        let dir = tempfile::tempdir().unwrap();
        let (t, r) = relay(dir.path());
        let d = draft("family");
        let id = r.submit_draft(&d).unwrap();
        assert_eq!(t.sent.lock().unwrap().len(), 0, "submitting a draft must not send");
        assert_eq!(r.pending_drafts("family").unwrap().len(), 1);
        assert_eq!(r.pending_drafts("family").unwrap()[0].0, id);
        // A refused decision also sends nothing.
        let out = r.deliver_draft(&d, &MainDecision::refuse("不合适")).unwrap();
        assert!(matches!(out, RelayOutcome::Blocked { .. }));
        assert!(t.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn draft_with_main_decision_sends_and_the_main_agent_may_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let (t, r) = relay(dir.path());
        let d = draft("family");
        let out = r.deliver_draft(&d, &MainDecision::approve_as("主 agent 转述：草稿内容", "已核对")).unwrap();
        let RelayOutcome::Sent { message, redacted } = out else {
            panic!("expected sent, got {out:?}")
        };
        assert!(!redacted);
        assert_eq!(message.text, "主 agent 转述：草稿内容");
        assert_eq!(t.sent.lock().unwrap().len(), 1);
        // The worker's facts are never sent.
        assert!(!t.sent.lock().unwrap()[0].text.contains("事实一"));
    }

    #[test]
    fn draft_is_discretion_checked_on_the_final_text() {
        let dir = tempfile::tempdir().unwrap();
        let (t, r) = relay(dir.path());
        let store = ChatStore::new(dir.path());
        store.write_discretion("family", "forbid: mail\n").unwrap();
        let d = Draft { labels: vec![ContentKind::Mail], ..draft("family") };
        let out = r.deliver_draft(&d, &MainDecision::approve("好")).unwrap();
        assert!(matches!(out, RelayOutcome::Blocked { .. }), "{out:?}");
        assert!(t.sent.lock().unwrap().is_empty(), "blocked draft must not send");
        // Rewriting the draft to clean text lets the same decision relay it.
        let out = r
            .deliver_draft(&d, &MainDecision::approve_as("只是问候", "已改写，去掉邮件内容"))
            .unwrap();
        assert!(matches!(out, RelayOutcome::Sent { .. }), "{out:?}");
        assert_eq!(t.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn main_turn_answer_is_checked_too() {
        let dir = tempfile::tempdir().unwrap();
        let (t, r) = relay(dir.path());
        let store = ChatStore::new(dir.path());
        store.write_discretion("family", "forbid: mail\n").unwrap();
        let msg = InboundMessage {
            channel: "pipe".into(),
            chat: "family".into(),
            sender: "alice".into(),
            text: "hi".into(),
            image: None,
        };
        let outcome = TurnOutcome { text: "邮件说……".into(), labels: vec![ContentKind::Mail], ..Default::default() };
        let out = r.deliver_main(&msg, &outcome).unwrap();
        assert!(matches!(out, RelayOutcome::Blocked { .. }));
        assert!(t.sent.lock().unwrap().is_empty());
    }
}
