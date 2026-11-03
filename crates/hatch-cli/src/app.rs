// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Wiring: config → database, router, connectors, executors, policy.

use crate::config::{home_dir, AppConfig, ResolvedModel};
use anyhow::{anyhow, bail, Context, Result};
use chrono_tz::Tz;
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_connectors::mail::imap::{ImapConfig, ImapSource};
use hatch_connectors::mail::smtp::{SmtpConfig, SmtpSender};
use hatch_connectors::{MailSender, MailSource};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_llm::provider::{AnthropicMessages, OpenAiCompatible};
use hatch_llm::{ChatModel, Router, Workload};
use hatch_policy::{ActionClass, AttentionPolicy, Autonomy, Gatekeeper, PolicyConfig};
use hatch_store::{params, Db};
use hatch_tools::{CreateEventExecutor, ExecutorRegistry, SendEmailExecutor};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct App {
    pub home: PathBuf,
    pub config: AppConfig,
    pub db: Db,
    pub tz: Tz,
}

/// Per-command model limits.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub max_calls: Option<u64>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
}

pub fn build_model(m: &ResolvedModel) -> Result<Arc<dyn ChatModel>> {
    let key = m.api_key.clone().ok_or_else(|| {
        anyhow!("model `{}`: environment variable {} is not set", m.cfg.name, m.cfg.api_key_env)
    })?;
    Ok(match m.cfg.provider.as_str() {
        "openai_compatible" => Arc::new(OpenAiCompatible::new(&m.cfg.name, &m.cfg.base_url, &m.cfg.model, key)),
        "anthropic" => Arc::new(AnthropicMessages::new(&m.cfg.name, &m.cfg.base_url, &m.cfg.model, key)),
        p => bail!("unknown provider {p}"),
    })
}

impl App {
    pub fn open(home_override: Option<&Path>) -> Result<App> {
        let home = home_dir(home_override);
        let config = AppConfig::load(&home.join("config.toml"))?;
        config.validate()?;
        let db = Db::open(&home.join("hatch.db")).context("opening database")?;
        let tz = hatch_core::time::parse_tz(&config.user.timezone)?;
        Ok(App { home, config, db, tz })
    }

    pub fn user_email(&self) -> String {
        if !self.config.user.email.is_empty() {
            self.config.user.email.clone()
        } else {
            self.config.mail.username.clone()
        }
    }

    pub fn router(&self, limits: Limits) -> Result<Router> {
        let resolved = self.config.resolved_models();
        if resolved.is_empty() {
            bail!(
                "没有配置模型。请在 {} 中配置 [[models]]，或设置环境变量 {}、{}、{}",
                self.home.join("config.toml").display(),
                crate::config::ENV_BASE_URL,
                crate::config::ENV_MODEL,
                crate::config::ENV_API_KEY
            );
        }
        let env_mode = resolved.len() == 1 && resolved[0].cfg.name == "env";
        let mut by_name: HashMap<String, Arc<dyn ChatModel>> = HashMap::new();
        let mut order = Vec::new();
        for m in &resolved {
            by_name.insert(m.cfg.name.clone(), build_model(m)?);
            order.push(m.cfg.name.clone());
        }
        let chain = |names: &[String]| -> Result<Vec<Arc<dyn ChatModel>>> {
            let names: Vec<String> = if names.is_empty() || env_mode { order.clone() } else { names.to_vec() };
            names
                .iter()
                .map(|n| by_name.get(n).cloned().ok_or_else(|| anyhow!("[routing] refers to unknown model `{n}`")))
                .collect()
        };
        let r = &self.config.routing;
        let mut router = Router::new(chain(&[])?)
            .route(Workload::Main, chain(&r.main)?)
            .route(Workload::Triage, chain(&r.triage)?)
            .route(Workload::Draft, chain(&r.draft)?)
            .route(Workload::Compaction, chain(&r.compaction)?)
            .route(Workload::MemoryFlush, chain(&r.memory_flush)?);
        if let Some(n) = limits.max_calls {
            router = router.with_max_calls(n);
        }
        if let Some(t) = limits.max_tokens {
            router = router.with_default_max_tokens(t);
        }
        if let Some(e) = limits.reasoning_effort {
            router = router.with_reasoning_effort(e);
        }
        Ok(router)
    }

    pub fn default_limits(&self) -> Limits {
        Limits {
            max_calls: self.config.limits.max_calls_per_run,
            max_tokens: self.config.limits.max_tokens,
            reasoning_effort: self.config.limits.effective_reasoning_effort(),
        }
    }

    /// Persist usage records (tokens only — no prices are invented).
    pub fn record_usage(&self, router: &Router, now: &Timestamp) -> Result<()> {
        for r in router.take_records() {
            self.db
                .conn()
                .execute(
                    "INSERT INTO usage_log(ts, workload, model, prompt_tokens, completion_tokens, cached_tokens) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        to_rfc3339(now),
                        r.workload.as_str(),
                        r.model,
                        r.usage.prompt_tokens as i64,
                        r.usage.completion_tokens as i64,
                        r.usage.cached_tokens as i64
                    ],
                )
                .map_err(|e| anyhow!(e.to_string()))?;
        }
        Ok(())
    }

    pub fn policy(&self) -> Result<Gatekeeper> {
        let mut cfg = PolicyConfig::default();
        for (k, v) in &self.config.policy {
            let class = ActionClass::parse(k)?;
            let want = Autonomy::parse(v)?;
            let got = cfg.set(class, want);
            if got != want {
                eprintln!("提示：{k} 的自主度上限为 {got:?}，已从 {want:?} 下调");
            }
        }
        Ok(Gatekeeper::new(cfg))
    }

    pub fn attention(&self) -> AttentionPolicy {
        let a = &self.config.attention;
        AttentionPolicy { quiet_start: a.quiet_start, quiet_end: a.quiet_end, daily_budget: a.daily_budget, tz: self.tz }
    }

    fn mail_password(&self) -> Result<String> {
        let var = &self.config.mail.password_env;
        std::env::var(var).ok().filter(|v| !v.is_empty()).ok_or_else(|| {
            anyhow!("环境变量 {var} 未设置。126/163 邮箱请使用“授权码”（设置 → POP3/SMTP/IMAP 中生成），不是登录密码")
        })
    }

    pub fn outbox_dir(&self) -> PathBuf {
        let d = PathBuf::from(&self.config.mail.outbox_dir);
        if d.is_absolute() {
            d
        } else {
            self.home.join(d)
        }
    }

    /// Mail sender. `force_outbox` (dry-run) guarantees nothing leaves the machine.
    pub fn sender(&self, force_outbox: bool) -> Result<Arc<dyn MailSender>> {
        let m = &self.config.mail;
        if force_outbox || m.sender != "smtp" {
            return Ok(Arc::new(FileOutbox::new(&self.outbox_dir(), &self.user_email())?));
        }
        Ok(Arc::new(SmtpSender::new(
            SmtpConfig { host: m.smtp_host.clone(), port: m.smtp_port, username: m.username.clone(), from: self.user_email() },
            self.mail_password()?,
        )))
    }

    pub fn executors(&self, force_outbox: bool) -> Result<ExecutorRegistry> {
        Ok(ExecutorRegistry::new()
            .with(Arc::new(SendEmailExecutor::new(self.sender(force_outbox)?, &self.user_email())))
            .with(Arc::new(CreateEventExecutor)))
    }

    pub fn source(&self, fixture: Option<&Path>) -> Result<Box<dyn MailSource>> {
        let m = &self.config.mail;
        let fixture = fixture.map(Path::to_path_buf).or_else(|| (m.source == "fixture" && !m.fixture_path.is_empty()).then(|| PathBuf::from(&m.fixture_path)));
        if let Some(p) = fixture {
            let account = if self.user_email().is_empty() { "fixture".to_string() } else { self.user_email() };
            return Ok(Box::new(FixtureMailbox::load(&account, &p)?));
        }
        if m.username.is_empty() {
            bail!("[mail] username 未配置");
        }
        Ok(Box::new(ImapSource::new(
            ImapConfig {
                host: m.imap_host.clone(),
                port: m.imap_port,
                username: m.username.clone(),
                mailbox: m.mailbox.clone(),
                send_id: ImapConfig::needs_id(&m.imap_host),
            },
            self.mail_password()?,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_model_overrides_config_and_never_touches_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(crate::config::ENV_BASE_URL, "http://127.0.0.1:9/v1");
        std::env::set_var(crate::config::ENV_MODEL, "test-model");
        std::env::set_var(crate::config::ENV_API_KEY, "sk-test-not-real-123456");
        let app = App::open(Some(dir.path())).unwrap();
        let router = app.router(Limits { max_calls: Some(2), max_tokens: Some(800), reasoning_effort: None }).unwrap();
        assert_eq!(router.chain(Workload::Triage)[0].name(), "env:test-model");
        std::env::remove_var(crate::config::ENV_BASE_URL);
        std::env::remove_var(crate::config::ENV_MODEL);
        std::env::remove_var(crate::config::ENV_API_KEY);
        assert!(app.router(app.default_limits()).is_err(), "no models without env or config");
        assert!(!dir.path().join("config.toml").exists());
        // Default sender is the outbox, never SMTP.
        app.sender(false).unwrap();
        assert!(app.outbox_dir().exists());
    }
}
