//! `hatch-transports` — multi-channel transports (Phase 5).
//!
//! The channel interface lives here, plus the one reference transport
//! that exists in this phase: the local pipe/file transport. Real
//! channels (WeChat / Feishu / Telegram) are **out of scope**: the
//! interface is ready for them, no real transport is implemented, and
//! no credentials for any real channel exist in this repo.
//!
//! # The interface
//!
//! * [`message`] — [`message::InboundMessage`] (channel, chat, sender,
//!   text and/or an image attachment) and [`message::OutboundMessage`]
//!   (the final text a transport sends). An image is a file reference
//!   plus metadata, never image bytes and never a prompt-injected
//!   instruction.
//! * [`transport`] — the [`transport::Transport`] trait, the
//!   [`transport::TurnRunner`] the transport calls back into, and the
//!   [`transport::TransportRegistry`] (register by name, list, status).
//!   A transport that is not configured reports guidance, never a
//!   delivery promise.
//! * [`discretion`] — per-chat sharing rules from
//!   `$HATCH_HOME/chats/<chat-id>/DISCRETION.md`. A discretion check
//!   runs before **any** outbound message and blocks or redacts
//!   violations. Secrets are blocked for every chat, rules or not.
//! * [`relay`] — the worker-relay rule. Workers/sub-agents only
//!   **prepare** content ([`relay::Draft`]); only the main agent
//!   relays and sends, through [`relay::Relay`]. A draft without a
//!   [`relay::MainDecision`] cannot be sent — there is no code path
//!   that sends a draft on its own.
//! * [`chats`] — per-chat state: the discretion file and the
//!   chat-id → session-id mapping. Each chat id maps to its own
//!   session/transcript; a message from chat A never enters chat B's
//!   context. Main chat and side chats share the same database —
//!   memory and goals — but never each other's transcript.
//! * [`capability`] — device capability manifest: a device declares
//!   what it can do; when a capability is missing the reply says so
//!   honestly and gives the enable path, instead of pretending.
//! * [`pipe`] — the reference transport: inbox JSON files in,
//!   outbox JSON files + a log out. It has no UI, so widgets are
//!   rendered with [`hatch_artifacts::render`] and the text fallback
//!   is what gets sent.
//! * [`outcome`] — reading back what a turn actually delivered
//!   (executed `deliver_artifact` calls) from the session transcript,
//!   so the transport can attach the delivery message.
//!
//! # Design authority
//!
//! The shape follows the Muse agent framework as documented in the
//! workspace deep-dive (`muse-hatch-architecture-deep-dive.md`
//! §9.4 multi-channel transports, §12 devices) and the gap roadmap
//! (R13 / RW-16): main + side chats share memory/goals with separate
//! transcripts; external-chat workers only prepare content and the
//! main agent relays and decides; per-chat `DISCRETION.md` rules are
//! recorded (including rules mentioned casually in conversation) and
//! enforced; device capability is declarative with an honest
//! missing-capability reply. Anything in this crate that goes beyond
//! that evidence — file layouts, the pipe transport's exact JSON
//! shapes, the redaction patterns, the default capability list — is
//! marked ORIGINAL in the module docs, never presented as Muse's
//! own behaviour.
//!
//! # What is real, what is not
//!
//! Real: the interface, the discretion check, the relay rule, the
//! per-chat session isolation, and the pipe transport end to end
//! (inbox → agent turn → outbox). Reference-only: the pipe transport
//! itself — it moves local files, it is not a chat app, and it proves
//! the interface rather than delivering to anyone.

pub mod capability;
pub mod chats;
pub mod discretion;
pub mod message;
pub mod outcome;
pub mod pipe;
pub mod relay;
pub mod transport;

pub use capability::{missing_reply, Capability, CapabilityManifest, CapabilityStore};
pub use chats::ChatStore;
pub use discretion::{check_outbound, CheckOutcome, DiscretionRules};
pub use message::{ContentKind, ImageAttachment, InboundMessage, OutboundMessage};
pub use outcome::executed_deliveries;
pub use pipe::{PipeDirs, PipeTransport};
pub use relay::{Draft, MainDecision, Relay, RelayOutcome};
pub use transport::{labels_for_tool_calls, PollReport, Transport, TransportRegistry, TransportStatus, TurnOutcome, TurnRunner};
