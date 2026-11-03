//! The transport interface and registry.
//!
//! A [`Transport`] moves messages in and out of one channel. It does
//! not decide what to say and it does not run the discretion check —
//! both belong to the main agent and the [`Relay`](crate::relay::Relay),
//! which is the only caller of [`Transport::send`]. The transport's
//! job is the channel mechanics: read inbound messages, write
//! outbound ones, and call back into a [`TurnRunner`] for the agent
//! turn that answers each inbound message.
//!
//! A transport that is not configured reports guidance through
//! [`TransportStatus`] — like the connector framework, it never
//! promises delivery it cannot do. Real channels (WeChat / Feishu /
//! Telegram) are listed by [`TransportRegistry::not_implemented`] with
//! that guidance; they are names, not transports.

use crate::message::{ContentKind, InboundMessage, OutboundMessage};
use crate::pipe::PipeDirs;
use hatch_core::Result;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// What one agent turn produced for a chat: the answer text, an
/// optional widget (a transport with a UI renders it; a transport
/// without one sends the text fallback), an optional artifact to
/// deliver, and the content labels the discretion check needs.
/// `pending_approvals` are the approval ids the turn queued (RW-8:
/// approval events surface into the channel, so the user sees them
/// there instead of only in the CLI).
#[derive(Clone, Debug, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub widget: Option<Value>,
    pub artifact_id: Option<String>,
    pub labels: Vec<ContentKind>,
    pub pending_approvals: Vec<String>,
}

/// The agent-turn callback a transport calls for each inbound message.
/// `session` is the chat's own session id (resolved by the transport
/// through [`ChatStore`](crate::chats::ChatStore)); `input` is the
/// turn input, which for image messages already contains the
/// attachment reference and the "you cannot see this image" note.
pub trait TurnRunner: Send + Sync {
    fn run_turn(&self, msg: &InboundMessage, session: &str, input: &str) -> Result<TurnOutcome>;
}

/// Labels for a turn, derived from the tools it used — the
/// producer's declaration for the discretion check. Heuristic and
/// documented as such: a turn that searched mail may quote mail; a
/// turn that read workspace files may quote files. It is the
/// *reliable* path only in combination with the text patterns in
/// [`discretion`](crate::discretion); a turn that paraphrases mail
/// without calling a mail tool is not labelled (see SECURITY.md §9g).
pub fn labels_for_tool_calls(names: &[String]) -> Vec<ContentKind> {
    let mut out = Vec::new();
    for n in names {
        let kind = if n.starts_with("mail_") || n == "propose_email" {
            Some(ContentKind::Mail)
        } else if n.starts_with("fs_") {
            Some(ContentKind::Files)
        } else {
            None
        };
        if let Some(k) = kind {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// What one poll of a transport did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollReport {
    pub processed: usize,
    pub sent: usize,
    pub blocked: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// How a transport describes itself. `configured == false` means
/// "not set up": the detail says what is missing and `guidance` says
/// how to fix it. Nothing here is a delivery promise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportStatus {
    pub name: String,
    pub configured: bool,
    pub detail: String,
    pub guidance: String,
}

/// One channel.
pub trait Transport: Send + Sync {
    fn name(&self) -> &str;
    fn status(&self) -> TransportStatus;
    /// Chat ids this transport knows about (seen in its inbox / store).
    fn list_chats(&self) -> Result<Vec<String>>;
    /// Send one final outbound message. Called only by the relay, after
    /// the discretion check. Returns a receipt (for the pipe transport:
    /// the outbox file path).
    fn send(&self, msg: &OutboundMessage) -> Result<String>;
    /// Read pending inbound messages, run one agent turn per message
    /// through `runner`, and hand each outcome to `deliver` (the relay).
    /// `deliver` returns the relay's outcome so the transport can
    /// record what happened (sent / blocked).
    fn poll_once(
        &self,
        runner: &dyn TurnRunner,
        deliver: &dyn Fn(&InboundMessage, &TurnOutcome) -> Result<crate::relay::RelayOutcome>,
    ) -> Result<PollReport>;
}

/// Transports registered by name.
pub struct TransportRegistry {
    transports: Vec<Arc<dyn Transport>>,
}

impl TransportRegistry {
    pub fn new() -> Self {
        TransportRegistry { transports: Vec::new() }
    }

    pub fn register(&mut self, t: Arc<dyn Transport>) {
        self.transports.retain(|x| x.name() != t.name());
        self.transports.push(t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Transport>> {
        self.transports.iter().find(|t| t.name() == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.transports.iter().map(|t| t.name().to_string()).collect()
    }

    pub fn statuses(&self) -> Vec<TransportStatus> {
        self.transports.iter().map(|t| t.status()).collect()
    }

    /// The reference registry: only the pipe transport exists.
    pub fn builtin(home: &Path, db: Arc<hatch_store::Db>) -> Self {
        let mut reg = TransportRegistry::new();
        let pipe = crate::pipe::PipeTransport::new(
            PipeDirs::default_under(home),
            crate::chats::ChatStore::new(home),
            home.join("artifacts"),
            db,
        );
        reg.register(Arc::new(pipe));
        reg
    }

    /// Real channels, named so the CLI can answer for them honestly:
    /// known, explicitly out of scope in this phase, with guidance
    /// instead of a transport. A future real transport registers
    /// itself here under the same name.
    pub fn not_implemented() -> Vec<TransportStatus> {
        [
            ("wechat", "微信（WeChat）"),
            ("feishu", "飞书（Feishu / Lark）"),
            ("telegram", "Telegram"),
        ]
        .iter()
        .map(|(name, label)| TransportStatus {
            name: (*name).to_string(),
            configured: false,
            detail: format!("{label}：本阶段未实现（OUT OF SCOPE），没有真实接入"),
            guidance: format!(
                "真实 {label} 接入需要按 docs/SECURITY.md §9g 的要求实现 webhook 签名校验与按聊天鉴权后，再注册为同名 transport；本阶段请用 pipe 参考传输"
            ),
        })
        .collect()
    }
}

impl Default for TransportRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chats::ChatStore;
    use crate::pipe::PipeTransport;
    use crate::relay::RelayOutcome;

    fn registry_with_pipe(dir: &Path) -> TransportRegistry {
        let mut reg = TransportRegistry::new();
        reg.register(Arc::new(PipeTransport::new(
            PipeDirs::default_under(dir),
            ChatStore::new(dir),
            dir.join("artifacts"),
            Arc::new(hatch_store::Db::open_in_memory().unwrap()),
        )));
        reg
    }

    #[test]
    fn builtin_registers_only_pipe_and_names_the_real_channels() {
        let dir = tempfile::tempdir().unwrap();
        let reg = TransportRegistry::builtin(dir.path(), Arc::new(hatch_store::Db::open_in_memory().unwrap()));
        assert_eq!(reg.names(), vec!["pipe"]);
        assert!(reg.get("wechat").is_none(), "no real transport is registered");
        let missing = TransportRegistry::not_implemented();
        assert_eq!(missing.len(), 3);
        assert!(missing.iter().all(|s| !s.configured));
        assert!(missing.iter().any(|s| s.name == "wechat"));
        assert!(missing.iter().any(|s| s.name == "feishu"));
        assert!(missing.iter().any(|s| s.name == "telegram"));
        assert!(missing.iter().all(|s| s.guidance.contains("SECURITY.md")));
    }

    #[test]
    fn registry_replaces_a_same_named_transport() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = registry_with_pipe(dir.path());
        reg.register(Arc::new(PipeTransport::new(
            PipeDirs::default_under(dir.path()),
            ChatStore::new(dir.path()),
            dir.path().join("artifacts"),
            Arc::new(hatch_store::Db::open_in_memory().unwrap()),
        )));
        assert_eq!(reg.names(), vec!["pipe"]);
        let _ = RelayOutcome::Blocked { reason: String::new() };
    }
}
