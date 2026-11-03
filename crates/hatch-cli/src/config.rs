// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Configuration: `$HATCH_HOME/config.toml` plus environment overrides.
//!
//! Secrets are never stored in the file — only the *names* of environment
//! variables that hold them. `HATCH_LLM_BASE_URL` + `HATCH_LLM_MODEL` +
//! `HATCH_LLM_API_KEY` together define a Chat-Completions-compatible model that
//! takes over every workload, so hatch can run without any config file.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const EXAMPLE: &str = include_str!("../../../config/hatch.example.toml");
pub const ENV_BASE_URL: &str = "HATCH_LLM_BASE_URL";
pub const ENV_MODEL: &str = "HATCH_LLM_MODEL";
pub const ENV_API_KEY: &str = "HATCH_LLM_API_KEY";
/// Optional: `low` / `medium` / `high` for reasoning models.
pub const ENV_REASONING_EFFORT: &str = "HATCH_LLM_REASONING_EFFORT";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    pub name: String,
    pub email: String,
    pub timezone: String,
}

impl Default for UserConfig {
    fn default() -> Self {
        UserConfig { name: "用户".into(), email: String::new(), timezone: "Asia/Shanghai".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    pub name: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key_env: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub main: Vec<String>,
    pub triage: Vec<String>,
    pub draft: Vec<String>,
    pub compaction: Vec<String>,
    pub memory_flush: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_calls_per_run: Option<u64>,
    /// Output cap per call. Reasoning models count hidden reasoning against it,
    /// so this must leave room for thinking *and* the answer.
    pub max_tokens: Option<u32>,
    /// `low` / `medium` / `high`; only sent when set (env `HATCH_LLM_REASONING_EFFORT` overrides).
    pub reasoning_effort: Option<String>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig { max_calls_per_run: Some(100), max_tokens: Some(4000), reasoning_effort: None }
    }
}

impl LimitsConfig {
    /// Effective reasoning effort: environment first, then the config file.
    pub fn effective_reasoning_effort(&self) -> Option<String> {
        std::env::var(ENV_REASONING_EFFORT)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| self.reasoning_effort.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    pub source: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password_env: String,
    pub mailbox: String,
    pub sender: String,
    pub outbox_dir: String,
    pub fixture_path: String,
    pub fetch_limit: usize,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig {
            source: "imap".into(),
            imap_host: "imap.126.com".into(),
            imap_port: 993,
            smtp_host: "smtp.126.com".into(),
            smtp_port: 465,
            username: String::new(),
            password_env: "HATCH_MAIL_PASSWORD".into(),
            mailbox: "INBOX".into(),
            sender: "outbox".into(),
            outbox_dir: "outbox".into(),
            fixture_path: String::new(),
            fetch_limit: 50,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionConfig {
    pub quiet_start: u32,
    pub quiet_end: u32,
    pub daily_budget: u32,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        AttentionConfig { quiet_start: 22, quiet_end: 8, daily_budget: 5 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub user: UserConfig,
    pub models: Vec<ModelConfig>,
    pub routing: RoutingConfig,
    pub limits: LimitsConfig,
    pub mail: MailConfig,
    pub attention: AttentionConfig,
    pub policy: BTreeMap<String, String>,
}

/// A model definition with its key resolved from the environment (never from disk).
pub struct ResolvedModel {
    pub cfg: ModelConfig,
    pub api_key: Option<String>,
}

impl AppConfig {
    pub fn parse(text: &str) -> Result<AppConfig> {
        toml::from_str(text).context("invalid config.toml")
    }

    pub fn load(path: &Path) -> Result<AppConfig> {
        if !path.exists() {
            return Ok(AppConfig::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Env-defined model, if all three variables are set and non-empty.
    pub fn env_model() -> Option<ResolvedModel> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let (base, model, key) = (get(ENV_BASE_URL)?, get(ENV_MODEL)?, get(ENV_API_KEY)?);
        Some(ResolvedModel {
            cfg: ModelConfig {
                name: "env".into(),
                provider: "openai_compatible".into(),
                base_url: base,
                model,
                api_key_env: ENV_API_KEY.into(),
            },
            api_key: Some(key),
        })
    }

    /// Models to use: the env model alone if defined, else the configured ones.
    pub fn resolved_models(&self) -> Vec<ResolvedModel> {
        if let Some(m) = Self::env_model() {
            return vec![m];
        }
        self.models
            .iter()
            .map(|m| ResolvedModel {
                cfg: m.clone(),
                api_key: if m.api_key_env.is_empty() { None } else { std::env::var(&m.api_key_env).ok().filter(|v| !v.is_empty()) },
            })
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        for m in &self.models {
            if !matches!(m.provider.as_str(), "openai_compatible" | "anthropic") {
                bail!("model {}: provider must be openai_compatible or anthropic", m.name);
            }
        }
        for (k, v) in &self.policy {
            hatch_policy::ActionClass::parse(k).map_err(|e| anyhow::anyhow!("[policy] {e}"))?;
            hatch_policy::Autonomy::parse(v).map_err(|e| anyhow::anyhow!("[policy] {e}"))?;
        }
        hatch_core::time::parse_tz(&self.user.timezone).map_err(|e| anyhow::anyhow!("[user] {e}"))?;
        Ok(())
    }
}

/// `$HATCH_HOME`, or `~/.hatch`.
pub fn home_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = override_dir {
        return p.to_path_buf();
    }
    if let Ok(h) = std::env::var("HATCH_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let base = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    base.join(".hatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_and_validates() {
        let c = AppConfig::parse(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.models[0].provider, "openai_compatible");
        assert_eq!(c.mail.imap_host, "imap.126.com");
        assert_eq!(c.limits.max_calls_per_run, Some(100));
        assert_eq!(c.limits.max_tokens, Some(4000));
        assert!(!EXAMPLE.contains("sk-"), "example must not contain keys");
    }

    #[test]
    fn defaults_and_bad_values() {
        let c = AppConfig::parse("").unwrap();
        assert_eq!(c.user.timezone, "Asia/Shanghai");
        assert_eq!(c.mail.sender, "outbox", "default sender never sends real mail");
        let bad = AppConfig::parse("[policy]\npayment = \"silent\"\nwarp = \"x\"").unwrap();
        assert!(bad.validate().is_err());
    }
}
