//! Inbound and outbound channel messages.
//!
//! An inbound message carries its channel, its chat id, its sender, and
//! text and/or one image attachment. The image is a **reference**: a
//! path (or file name) plus mime type and size. Image bytes are never
//! put into the model's prompt, and an image is never an instruction —
//! the turn input says so explicitly (see [`pipe`](crate::pipe)).
//!
//! An outbound message is the final text a transport sends, after the
//! discretion check, widget rendering and artifact delivery have all
//! run. It carries no widget JSON: a transport without a UI sends the
//! text fallback, and a future UI transport re-renders from the
//! [`TurnOutcome`](crate::transport::TurnOutcome) instead.

use hatch_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// What kind of content a draft or turn result is (partly) made of.
/// Declared by the producer (the turn runner labels a turn by the
/// tools it used); the discretion check treats a label as the
/// producer's own statement about the content, so a labelled violation
/// blocks outright instead of being pattern-redacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    /// Content taken from the user's mail.
    Mail,
    /// Messages (or quotes) from other people.
    OtherPeople,
    /// Secret material (passwords, keys, tokens). Blocked everywhere.
    Secrets,
    /// Content taken from workspace files.
    Files,
}

impl ContentKind {
    /// The name used in DISCRETION.md rules.
    pub fn rule_name(&self) -> &'static str {
        match self {
            ContentKind::Mail => "mail",
            ContentKind::OtherPeople => "other_people",
            ContentKind::Secrets => "secrets",
            ContentKind::Files => "files",
        }
    }

    pub fn parse(s: &str) -> Result<ContentKind> {
        match s.trim() {
            "mail" => Ok(ContentKind::Mail),
            "other_people" => Ok(ContentKind::OtherPeople),
            "secrets" => Ok(ContentKind::Secrets),
            "files" => Ok(ContentKind::Files),
            other => Err(Error::Invalid(format!(
                "unknown sharing-rule category `{other}` (mail | other_people | secrets | files)"
            ))),
        }
    }

    /// Human-readable name, used in block reasons.
    pub fn display(&self) -> &'static str {
        match self {
            ContentKind::Mail => "邮件内容",
            ContentKind::OtherPeople => "他人的消息",
            ContentKind::Secrets => "秘密",
            ContentKind::Files => "工作区文件内容",
        }
    }
}

/// A chat id: what identifies one conversation on one channel.
/// Restricted to safe characters so it can be a directory name under
/// `$HATCH_HOME/chats/<chat-id>/` without any path traversal.
pub fn valid_chat_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn check_chat_id(id: &str) -> Result<()> {
    if valid_chat_id(id) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid chat id `{id}` (letters, digits, `-`, `_` only, at most 64 characters)"
        )))
    }
}

/// An image attached to an inbound message: a reference, not bytes.
/// `path` is the file name (pipe transport) or a path the transport
/// resolved; `mime` and `size_bytes` are metadata for the model note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub path: String,
    pub mime: String,
    pub size_bytes: u64,
}

impl ImageAttachment {
    /// Guess a mime type from the file extension; unknown → generic.
    pub fn mime_for(path: &str) -> &'static str {
        match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "heic" => "image/heic",
            _ => "application/octet-stream",
        }
    }
}

/// One message arriving from a channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Transport name, e.g. `pipe`.
    pub channel: String,
    /// The conversation this message belongs to.
    pub chat: String,
    /// Who sent it (a name or id on that channel).
    pub sender: String,
    /// Text body; empty when the message is image-only.
    #[serde(default)]
    pub text: String,
    /// Optional image attachment (reference only).
    #[serde(default)]
    pub image: Option<ImageAttachment>,
}

impl InboundMessage {
    pub fn validate(&self) -> Result<()> {
        check_chat_id(&self.chat)?;
        if self.channel.trim().is_empty() {
            return Err(Error::Invalid("inbound message needs a non-empty `channel`".into()));
        }
        if self.sender.trim().is_empty() {
            return Err(Error::Invalid("inbound message needs a non-empty `sender`".into()));
        }
        if self.text.trim().is_empty() && self.image.is_none() {
            return Err(Error::Invalid("inbound message has neither text nor an image".into()));
        }
        Ok(())
    }
}

/// The final outbound message: what the transport actually sends.
/// Built by the relay after discretion, widget rendering and artifact
/// delivery; a transport writes/sends exactly this text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub chat_id: String,
    pub text: String,
    /// True when the discretion check redacted part of the draft.
    #[serde(default)]
    pub redacted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_are_safe_directory_names() {
        assert!(valid_chat_id("family-1"));
        assert!(valid_chat_id("a_b2"));
        assert!(!valid_chat_id("../evil"));
        assert!(!valid_chat_id("a/b"));
        assert!(!valid_chat_id(""));
        assert!(!valid_chat_id("a b"));
        assert!(!valid_chat_id(&"x".repeat(65)));
    }

    #[test]
    fn inbound_validation_rejects_empty_and_bad_chat() {
        let base = InboundMessage {
            channel: "pipe".into(),
            chat: "family".into(),
            sender: "alice".into(),
            text: "hi".into(),
            image: None,
        };
        assert!(base.validate().is_ok());
        let mut bad = base.clone();
        bad.chat = "../x".into();
        assert!(bad.validate().is_err());
        let mut empty = base.clone();
        empty.text = "  ".into();
        assert!(empty.validate().is_err(), "text-only message needs text");
        let mut img_only = base.clone();
        img_only.text = String::new();
        img_only.image = Some(ImageAttachment { path: "p.png".into(), mime: "image/png".into(), size_bytes: 10 });
        assert!(img_only.validate().is_ok());
    }

    #[test]
    fn content_kind_round_trips_through_rule_names() {
        for k in [ContentKind::Mail, ContentKind::OtherPeople, ContentKind::Secrets, ContentKind::Files] {
            assert_eq!(ContentKind::parse(k.rule_name()).unwrap(), k);
        }
        assert!(ContentKind::parse("mail_content").is_err());
    }
}
