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
/// Optional wire protocol for the env-defined model (default `openai_compatible`).
pub const ENV_PROVIDER: &str = "HATCH_LLM_PROVIDER";
/// Accepted `provider` values. `openai_compatible` and `openai_chat` are the
/// same Chat Completions protocol.
pub const PROVIDERS: &[&str] = &["openai_compatible", "openai_chat", "openai_responses", "anthropic", "gemini"];
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
    /// Stream answers in `hatch chat` / `hatch ask` (also `--stream`).
    pub stream: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig { max_calls_per_run: Some(100), max_tokens: Some(4000), reasoning_effort: None, stream: false }
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

/// `[workspace]` — the directory file and shell tools may touch. Empty = off.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub root: String,
}

/// `[sandbox]` — how `shell_run` commands are isolated.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// `namespaces` (default) or `none` (no isolation; results are labelled 未隔离).
    pub mode: String,
    pub max_timeout_secs: u64,
    pub cpu_secs: u64,
    pub memory_mb: u64,
    pub file_size_mb: u64,
    pub max_processes: u64,
    pub max_output_kb: usize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig { mode: "namespaces".into(), max_timeout_secs: 600, cpu_secs: 120, memory_mb: 2048, file_size_mb: 64, max_processes: 512, max_output_kb: 64 }
    }
}

/// `[web]` — web access for the chat agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub fetch: bool,
    pub fetch_max_kb: u64,
    /// SearXNG-compatible endpoint; empty = no `web_search` tool.
    pub search_url: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig { fetch: true, fetch_max_kb: 512, search_url: String::new() }
    }
}

/// `[agent]`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    /// Model ↔ tool round trips per user turn.
    pub max_steps: usize,
    /// Offer `delegate` / `delegate_parallel`.
    pub subagents: bool,
    /// Main agent = 0, its sub-agents = 1, …
    pub max_depth: usize,
    pub max_parallel: usize,
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection { max_steps: 24, subagents: true, max_depth: 2, max_parallel: 3 }
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
    pub workspace: WorkspaceConfig,
    pub sandbox: SandboxConfig,
    pub web: WebConfig,
    pub agent: AgentSection,
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
        let provider = get(ENV_PROVIDER).unwrap_or_else(|| "openai_compatible".into());
        Some(ResolvedModel {
            cfg: ModelConfig {
                name: "env".into(),
                provider,
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
            if !PROVIDERS.contains(&m.provider.as_str()) {
                bail!("model {}: provider must be one of {}", m.name, PROVIDERS.join(" / "));
            }
        }
        if let Some(m) = Self::env_model() {
            if !PROVIDERS.contains(&m.cfg.provider.as_str()) {
                bail!("{ENV_PROVIDER} must be one of {}", PROVIDERS.join(" / "));
            }
        }
        for (k, v) in &self.policy {
            hatch_policy::ActionClass::parse(k).map_err(|e| anyhow::anyhow!("[policy] {e}"))?;
            hatch_policy::Autonomy::parse(v).map_err(|e| anyhow::anyhow!("[policy] {e}"))?;
        }
        hatch_core::time::parse_tz(&self.user.timezone).map_err(|e| anyhow::anyhow!("[user] {e}"))?;
        hatch_sandbox::SandboxMode::parse(&self.sandbox.mode).map_err(|e| anyhow::anyhow!("[sandbox] {e}"))?;
        if self.agent.max_steps == 0 {
            bail!("[agent] max_steps must be at least 1");
        }
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
        assert!(!c.limits.stream, "streaming is opt-in");
    }

    #[test]
    fn every_protocol_is_accepted_and_unknown_ones_rejected() {
        for p in PROVIDERS {
            let text = format!("[[models]]\nname = \"x\"\nprovider = \"{p}\"\nbase_url = \"https://e\"\nmodel = \"m\"\n");
            AppConfig::parse(&text).unwrap().validate().unwrap();
        }
        let bad = "[[models]]\nname = \"x\"\nprovider = \"openai_realtime\"\nbase_url = \"\"\nmodel = \"m\"\n";
        assert!(AppConfig::parse(bad).unwrap().validate().is_err());
    }
}
