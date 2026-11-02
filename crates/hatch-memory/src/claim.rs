// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Claim types.

use hatch_core::time::Timestamp;
use hatch_core::Trust;
use serde::Serialize;

/// A stored claim.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Claim {
    pub id: String,
    /// What the claim is about: `user`, `person:alice@x.com`, `preference:reply_style`…
    pub subject: String,
    pub text: String,
    pub tags: Vec<String>,
    /// Where it came from: `user:chat`, `mail:<id>`, `import:notes.md`…
    pub source: String,
    pub trust: Trust,
    pub confidence: f64,
    pub created_at: Timestamp,
    pub valid_until: Option<Timestamp>,
    pub supersedes: Option<String>,
    pub replaced_by: Option<String>,
    /// Inferred from behaviour rather than stated by the user.
    pub inferred: bool,
}

impl Claim {
    /// Current = not replaced and not expired at `now`.
    pub fn is_current(&self, now: &Timestamp) -> bool {
        self.replaced_by.is_none() && self.valid_until.map(|v| v > *now).unwrap_or(true)
    }
}

/// Input to [`crate::MemoryStore::add`].
#[derive(Clone, Debug)]
pub struct NewClaim {
    pub subject: String,
    pub text: String,
    pub tags: Vec<String>,
    pub source: String,
    pub trust: Trust,
    pub confidence: f64,
    pub valid_until: Option<Timestamp>,
    pub inferred: bool,
}

impl NewClaim {
    /// A fact stated by the user in conversation.
    pub fn stated(subject: &str, text: &str, source: &str) -> Self {
        NewClaim {
            subject: subject.to_string(),
            text: text.to_string(),
            tags: vec![],
            source: source.to_string(),
            trust: Trust::User,
            confidence: 0.9,
            valid_until: None,
            inferred: false,
        }
    }

    pub fn with_tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|t| t.to_string()).collect();
        self
    }

    pub fn with_trust(mut self, trust: Trust, confidence: f64) -> Self {
        self.trust = trust;
        self.confidence = confidence;
        self
    }

    pub fn inferred(mut self) -> Self {
        self.inferred = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddOutcome {
    Added(String),
    /// An identical current claim already exists; nothing was written.
    Duplicate(String),
}

impl AddOutcome {
    pub fn id(&self) -> &str {
        match self {
            AddOutcome::Added(id) | AddOutcome::Duplicate(id) => id,
        }
    }
}

/// Provenance of a claim: what it replaced and what replaced it.
#[derive(Clone, Debug, Serialize)]
pub struct Explanation {
    pub claim: Claim,
    /// Older versions, newest first.
    pub history: Vec<Claim>,
    /// Newer versions, oldest first.
    pub successors: Vec<Claim>,
}

/// Result of a forget request — honest about what could not be erased.
#[derive(Clone, Debug, Serialize)]
pub struct ForgetReport {
    pub claims_removed: usize,
    pub person_removed: bool,
    pub caveat: String,
}

pub const FORGET_CAVEAT: &str = "已从本地记忆库删除上述条目及其全部历史版本。注意：已经发出的邮件、外部服务中的副本、\
模型服务商侧的请求日志不在本系统控制范围内，无法被撤回。";
