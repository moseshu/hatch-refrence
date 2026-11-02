// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Mail connectors.

pub mod fixture;
pub mod imap;
pub mod mime;
pub mod parse;
pub mod repo;
pub mod smtp;

pub use repo::{MailRepo, StoredMail};

use hatch_core::time::Timestamp;
use hatch_core::Result;
use serde::{Deserialize, Serialize};

/// A received mail after MIME parsing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParsedMail {
    pub uid: Option<u32>,
    pub message_id: String,
    pub from_addr: String,
    pub from_name: String,
    pub to: Vec<String>,
    pub subject: String,
    pub sent_at: Option<Timestamp>,
    pub body: String,
}

/// A mail to be delivered. Recipients are always set by code, never by a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutgoingMail {
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

/// Where new mail comes from.
pub trait MailSource {
    fn account(&self) -> String;
    /// Mails with UID greater than `after_uid` (or recent ones when `None`), at most `limit`, oldest first.
    fn fetch_new(&mut self, after_uid: Option<u32>, limit: usize) -> Result<Vec<ParsedMail>>;
}

/// Where outgoing mail goes. Returns the Message-ID that was sent.
pub trait MailSender: Send + Sync {
    fn send(&self, mail: &OutgoingMail) -> Result<String>;
}
