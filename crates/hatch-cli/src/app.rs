//! Wiring: config → database, router, connectors, executors, policy.

use crate::config::{home_dir, AppConfig, ResolvedModel};
use anyhow::{anyhow, bail, Context, Result};
use chrono_tz::Tz;
use hatch_agent::ChildRegistry;
use hatch_browser::{Broker, BrowserConfig};
use hatch_connectors::mail::fixture::{FileOutbox, FixtureMailbox};
use hatch_connectors::mail::imap::{ImapConfig, ImapSource};
use hatch_connectors::mail::smtp::{SmtpConfig, SmtpSender};
use hatch_connectors::{MailSender, MailSource};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_llm::provider::{AnthropicMessages, Gemini, OpenAiCompatible, OpenAiResponses};
use hatch_llm::{ChatModel, Router, Workload};
use hatch_mcp::{AnyClient, McpCallExecutor, ToolInfo};
use hatch_policy::{ActionClass, AttentionPolicy, Autonomy, Gatekeeper, PolicyConfig};
use hatch_sandbox::{HelperCmd, Limits as SandboxLimits, Sandbox, SandboxMode};
use hatch_skills::{registry::SkillRegistry, Skill};
use hatch_store::{params, Db};
use hatch_tools::builtin::GeneralTools;
use hatch_tools::web::{FetchLimits, SearxBackend};
use hatch_tools::{
    BrowserClickExecutor, BrowserOpenExecutor, ConnectorCallExecutor, CreateEventExecutor,
    ExecCommandExecutor, ExecutorRegistry, FetchUrlExecutor, SendEmailExecutor, WebSearchExecutor,
    Workspace, WriteFileExecutor,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Hidden subcommand under which the `hatch` binary acts as the sandbox helper.
pub const HELPER_SUBCOMMAND: &str = "__sandbox-helper";

pub struct App {
    pub home: PathBuf,
    pub config: AppConfig,
    /// Shared by threads: the MCP server runs each `tools/call` on a
    /// worker thread, so it needs an owned handle to the same database.
    pub db: Arc<Db>,
    pub tz: Tz,
    /// One broker per process, shared by the browser tools and executors.
    browser: OnceLock<Option<Arc<Broker>>>,
    /// MCP servers, connected lazily on first use (chat only).
    mcp: OnceLock<McpSetup>,
    /// Shared executor for approved `mcp_call` payloads.
    pub mcp_executor: Arc<McpCallExecutor>,
    /// Shared executor for approved `connector_call` payloads.
    pub connector_executor: Arc<ConnectorCallExecutor>,
    /// Sub-agents running in this process (async delegation): run id →
    /// cancel token + owner. Chat, the daemon and the transport runner
    /// share this one registry; child processes get their own (empty)
    /// registry over their scratch database.
    pub subagent_registry: Arc<ChildRegistry>,
}

/// Connected MCP servers and the tools they offer.
pub struct McpSetup {
    pub clients: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<AnyClient>>>>>,
    pub tools: Vec<(String, ToolInfo)>,
    pub warnings: Vec<String>,
}

/// Per-command model limits.
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub max_calls: Option<u64>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
}

/// Build one model client. When `proxy_url` is set, every model
/// talks to the inference proxy instead of its own `base_url`, and
/// no API key is needed from the agent side: the client sends a
/// placeholder bearer token that the proxy replaces with the real
/// upstream key (or passes through when the proxy has none).
pub fn build_model(m: &ResolvedModel, proxy_url: Option<&str>) -> Result<Arc<dyn ChatModel>> {
    let (base_url, key) = match proxy_url {
        Some(url) => (url.to_string(), "proxy".to_string()),
        None => {
            let key = m.api_key.clone().ok_or_else(|| {
                anyhow!(
                    "model `{}`: environment variable {} is not set",
                    m.cfg.name,
                    m.cfg.api_key_env
                )
            })?;
            (m.cfg.base_url.clone(), key)
        }
    };
    Ok(match m.cfg.provider.as_str() {
        "openai_compatible" | "openai_chat" => Arc::new(OpenAiCompatible::new(
            &m.cfg.name,
            &base_url,
            &m.cfg.model,
            key,
        )),
        "openai_responses" => Arc::new(OpenAiResponses::new(
            &m.cfg.name,
            &base_url,
            &m.cfg.model,
            key,
        )),
        "anthropic" => Arc::new(AnthropicMessages::new(
            &m.cfg.name,
            &base_url,
            &m.cfg.model,
            key,
        )),
        "gemini" => Arc::new(Gemini::new(&m.cfg.name, &base_url, &m.cfg.model, key)),
        p => bail!("unknown provider {p}"),
    })
}

/// Build a router from configuration (also used per job run by the daemon).
pub fn build_router(config: &AppConfig, home: &Path, limits: Limits) -> Result<Router> {
    let resolved = config.resolved_models();
    if resolved.is_empty() {
        bail!(
            "没有配置模型。请在 {} 中配置 [[models]]，或设置环境变量 {}、{}、{}",
            home.join("config.toml").display(),
            crate::config::ENV_BASE_URL,
            crate::config::ENV_MODEL,
            crate::config::ENV_API_KEY
        );
    }
    let env_mode = resolved.len() == 1 && resolved[0].cfg.name == "env";
    let mut by_name: HashMap<String, Arc<dyn ChatModel>> = HashMap::new();
    let mut order = Vec::new();
    let proxy_url = {
        let p = config.inference.proxy_url.trim();
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
    };
    for m in &resolved {
        by_name.insert(m.cfg.name.clone(), build_model(m, proxy_url.as_deref())?);
        order.push(m.cfg.name.clone());
    }
    let chain = |names: &[String]| -> Result<Vec<Arc<dyn ChatModel>>> {
        let names: Vec<String> = if names.is_empty() || env_mode {
            order.clone()
        } else {
            names.to_vec()
        };
        names
            .iter()
            .map(|n| {
                by_name
                    .get(n)
                    .cloned()
                    .ok_or_else(|| anyhow!("[routing] refers to unknown model `{n}`"))
            })
            .collect()
    };
    let r = &config.routing;
    let mut router = Router::new(chain(&[])?)
        .route(Workload::Main, chain(&r.main)?)
        .route(Workload::Triage, chain(&r.triage)?)
        .route(Workload::Draft, chain(&r.draft)?)
        .route(Workload::Compaction, chain(&r.compaction)?)
        .route(Workload::MemoryFlush, chain(&r.memory_flush)?)
        .route(Workload::Brief, chain(&r.brief)?)
        .route(Workload::Feed, chain(&r.feed)?);
    for (w, ttl) in &r.cache_ttl_secs {
        if let Some(w) = Workload::parse(w) {
            router = router.with_cache_ttl(w, *ttl);
        }
    }
    if !r.fallback.is_empty() {
        let mut rules: std::collections::HashMap<Workload, Vec<hatch_llm::FallbackRule>> =
            std::collections::HashMap::new();
        for f in &r.fallback {
            // Config validation already checked these parse; skip (never
            // panic) if a caller built the config by hand.
            if let (Some(w), Some(t), Some(a)) = (
                Workload::parse(&f.workload),
                hatch_llm::LadderTrigger::parse(&f.trigger),
                hatch_llm::LadderAction::parse(&f.action),
            ) {
                rules.entry(w).or_default().push(hatch_llm::FallbackRule { trigger: t, action: a });
            }
        }
        // Overrides layer onto the default ladder: a workload/trigger
        // not mentioned keeps its default action.
        let mut table = hatch_llm::FallbackTable::default();
        for (w, rs) in rules {
            table = table.with_rules(w, rs);
        }
        router = router.with_fallback_table(table);
    }
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

impl App {
    pub fn open(home_override: Option<&Path>) -> Result<App> {
        let home = home_dir(home_override);
        let db = Db::open(&home.join("hatch.db")).context("opening database")?;
        Self::open_with_db(Some(&home), Arc::new(db))
    }

    /// Like [`App::open`], but with a caller-provided database. A sub-agent
    /// child process opens this way with a scratch database, so it can
    /// reuse the config, router and tool wiring without ever writing to
    /// the shared database.
    pub fn open_with_db(home_override: Option<&Path>, db: Arc<Db>) -> Result<App> {
        let home = home_dir(home_override);
        let config = AppConfig::load(&home.join("config.toml"))?;
        config.validate()?;
        let tz = hatch_core::time::parse_tz(&config.user.timezone)?;
        Ok(App {
            home: home.clone(),
            config,
            db: Arc::clone(&db),
            tz,
            browser: OnceLock::new(),
            mcp: OnceLock::new(),
            mcp_executor: Arc::new(McpCallExecutor::new()),
            connector_executor: Arc::new(ConnectorCallExecutor::new(&home)),
            subagent_registry: Arc::new(ChildRegistry::new(db)),
        })
    }

    pub fn user_email(&self) -> String {
        if !self.config.user.email.is_empty() {
            self.config.user.email.clone()
        } else {
            self.config.mail.username.clone()
        }
    }

    pub fn router(&self, limits: Limits) -> Result<Router> {
        build_router(&self.config, &self.home, limits)
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
        AttentionPolicy {
            quiet_start: a.quiet_start,
            quiet_end: a.quiet_end,
            daily_budget: a.daily_budget,
            tz: self.tz,
        }
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
            return Ok(Arc::new(FileOutbox::new(
                &self.outbox_dir(),
                &self.user_email(),
            )?));
        }
        Ok(Arc::new(SmtpSender::new(
            SmtpConfig {
                host: m.smtp_host.clone(),
                port: m.smtp_port,
                username: m.username.clone(),
                from: self.user_email(),
            },
            self.mail_password()?,
        )))
    }

    /// P2 ranking knobs from `[agent] salience_half_life_days`.
    pub fn ranking_config(&self) -> hatch_memory::RankingConfig {
        hatch_memory::RankingConfig::from_days(self.config.agent.salience_half_life_days)
    }

    /// Browser settings from `[browser]`.
    pub fn browser_config(&self) -> BrowserConfig {
        let b = &self.config.browser;
        let mut c = BrowserConfig::new(&self.home);
        if !b.chromium_path.trim().is_empty() {
            c.chromium_path = Some(PathBuf::from(b.chromium_path.trim()));
        }
        if !b.profiles_dir.trim().is_empty() {
            c.profiles_dir = PathBuf::from(b.profiles_dir.trim());
        }
        if !b.ca_bundle.trim().is_empty() {
            c.ca_bundle = Some(PathBuf::from(b.ca_bundle.trim()));
        }
        c.headless = b.headless;
        c.max_tabs = b.max_tabs.max(1);
        c.idle_timeout = std::time::Duration::from_secs(b.idle_timeout_secs.max(30));
        c.blocklist = b.blocklist.clone();
        c.allow_loopback = b.allow_loopback;
        c.ignore_cert_errors = b.ignore_cert_errors;
        c
    }

    /// The process-wide browser broker, if `[browser] enabled = true`.
    pub fn browser(&self) -> Option<Arc<Broker>> {
        self.browser
            .get_or_init(|| {
                self.config
                    .browser
                    .enabled
                    .then(|| Arc::new(Broker::new(self.browser_config())))
            })
            .clone()
    }

    /// A broker regardless of `enabled` (for `hatch browser` management commands).
    pub fn browser_for_management(&self) -> Arc<Broker> {
        self.browser()
            .unwrap_or_else(|| Arc::new(Broker::new(self.browser_config())))
    }

    /// Skill registry over `$HATCH_HOME/skills` and `./skills`.
    pub fn skill_registry(&self) -> SkillRegistry {
        SkillRegistry::from_home(&self.home)
    }

    /// Skills active for this turn: `[skills] load` from config plus
    /// names activated with `hatch skills load` / the `load_skill` tool.
    /// A skill that fails to load is reported and skipped, never fatal.
    pub fn active_skills(&self) -> Vec<Skill> {
        let reg = self.skill_registry();
        let mut names = self.config.skills.load.clone();
        for n in hatch_skills::registry::active_skills(&self.home) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        let mut out = Vec::new();
        for n in names {
            match reg.load(&n) {
                Ok(s) => out.push(s),
                Err(e) => eprintln!("（技能 {n} 加载失败：{e}，已跳过）"),
            }
        }
        out
    }

    /// Connect enabled MCP servers once per process (stdio or HTTP).
    /// A server that fails to connect is reported in `warnings` and
    /// skipped; the agent keeps running without its tools.
    ///
    /// Tokens are resolved through the vault helper (`reveal`): the value
    /// lives in this process only between reveal and child spawn / header
    /// construction, and is dropped right after. When the helper cannot
    /// start, the legacy file read is used and a warning is recorded
    /// (honest fallback, not a silent one). Header values for HTTP
    /// servers are resolved from the environment by *name* (`cfg.headers`).
    pub fn mcp(&self) -> &McpSetup {
        self.mcp.get_or_init(|| {
            let mut clients: HashMap<String, Arc<std::sync::Mutex<AnyClient>>> = HashMap::new();
            let mut tools: Vec<(String, ToolInfo)> = Vec::new();
            let mut warnings: Vec<String> = Vec::new();
            let vault = hatch_vault::open(&self.home).ok();
            let vault_client = hatch_vault::VaultClient::start(&self.home).ok();
            let mut helper_warned = false;
            for cfg in self.config.mcp_servers.iter().filter(|s| s.enabled) {
                let mut env_map: HashMap<String, String> = HashMap::new();
                for name in &cfg.env {
                    match std::env::var(name) {
                        Ok(v) if !v.is_empty() => {
                            env_map.insert(name.clone(), v);
                        }
                        _ => warnings
                            .push(format!("MCP 服务器 {}：环境变量 {name} 未设置", cfg.name)),
                    }
                }
                // HTTP headers are env-var names too; only the resolved
                // values travel, and they are never logged.
                let mut headers_map: HashMap<String, String> = HashMap::new();
                for name in &cfg.headers {
                    match std::env::var(name) {
                        Ok(v) if !v.is_empty() => {
                            headers_map.insert(name.clone(), v);
                        }
                        _ => {
                            warnings.push(format!("MCP 服务器 {}：请求头 {name} 未设置", cfg.name))
                        }
                    }
                }
                let token = cfg.token_ref.as_ref().and_then(|t| {
                    // Preferred: reveal from the helper (spawn-instant only).
                    if let Some(vc) = &vault_client {
                        if let Ok(v) = vc.reveal(t) {
                            return Some(v);
                        }
                    }
                    // Fallback: legacy file read (warn once, honestly).
                    if !helper_warned {
                        helper_warned = true;
                        warnings.push(
                            "vault helper 未启动：MCP token 将从文件读取（短暂经过本进程内存）"
                                .into(),
                        );
                    }
                    vault.as_ref().and_then(|v| v.get(t).ok()).flatten()
                });
                if cfg.token_ref.is_some() && token.is_none() {
                    warnings.push(format!(
                        "MCP 服务器 {}：vault 中没有 token {}",
                        cfg.name,
                        cfg.token_ref.as_deref().unwrap_or("")
                    ));
                }
                match AnyClient::connect(cfg, &env_map, &headers_map, token.as_deref()) {
                    Ok(mut client) => match client.list_tools() {
                        Ok(listed) => {
                            for t in listed {
                                tools.push((cfg.name.clone(), t));
                            }
                            let arc = Arc::new(std::sync::Mutex::new(client));
                            self.mcp_executor.register(&cfg.name, arc.clone());
                            clients.insert(cfg.name.clone(), arc);
                        }
                        Err(e) => {
                            warnings.push(format!("MCP 服务器 {}：tools/list 失败：{e}", cfg.name))
                        }
                    },
                    Err(e) => warnings.push(format!("MCP 服务器 {}：连接失败：{e}", cfg.name)),
                }
            }
            McpSetup {
                clients: Arc::new(std::sync::Mutex::new(clients)),
                tools,
                warnings,
            }
        })
    }

    pub fn executors(&self, force_outbox: bool) -> Result<ExecutorRegistry> {
        let mut reg = ExecutorRegistry::new()
            .with(Arc::new(SendEmailExecutor::new(
                self.sender(force_outbox)?,
                &self.user_email(),
            )))
            .with(Arc::new(CreateEventExecutor))
            .with(Arc::new(WriteFileExecutor))
            .with(Arc::new(ExecCommandExecutor {
                sandbox: Arc::new(self.sandbox()?),
            }))
            .with(Arc::new(FetchUrlExecutor {
                limits: self.fetch_limits(),
            }))
            .with(Arc::new(
                hatch_artifacts::executor::CreateArtifactExecutor::new(self.artifacts_dir())
                    .with_quota(self.config.artifact_quota()),
            ))
            .with(Arc::new(hatch_artifacts::executor::ShareArtifactExecutor {
                root: self.artifacts_dir(),
            }))
            .with(Arc::new(
                hatch_artifacts::executor::DeliverArtifactExecutor {
                    root: self.artifacts_dir(),
                },
            ));
        let mcp: Arc<dyn hatch_tools::ActionExecutor> = self.mcp_executor.clone();
        reg = reg.with(mcp);
        let connectors: Arc<dyn hatch_tools::ActionExecutor> = self.connector_executor.clone();
        reg = reg.with(connectors);
        if let Some(b) = self.browser() {
            reg = reg
                .with(Arc::new(BrowserOpenExecutor { broker: b.clone() }))
                .with(Arc::new(BrowserClickExecutor { broker: b }));
        }
        if !self.config.web.search_url.is_empty() {
            reg = reg.with(Arc::new(WebSearchExecutor {
                backend: Arc::new(SearxBackend {
                    base_url: self.config.web.search_url.clone(),
                }),
            }));
        }
        Ok(reg)
    }

    /// Connector manifests directory: `$HATCH_HOME/connectors`, or
    /// `$HATCH_CONNECTORS_DIR` when set.
    pub fn connectors_dir(&self) -> PathBuf {
        hatch_connectors::framework::default_dir(&self.home)
    }

    /// Versioned artifacts live under `$HATCH_HOME/artifacts/<id>/v<n>/`.
    pub fn artifacts_dir(&self) -> PathBuf {
        self.home.join("artifacts")
    }

    /// The pipe transport's directories (`[transports]` overrides, else
    /// `$HATCH_HOME/transports/pipe/{inbox,outbox,processed,failed}`).
    pub fn pipe_dirs(&self) -> hatch_transports::PipeDirs {
        let mut dirs = hatch_transports::PipeDirs::default_under(&self.home);
        let t = &self.config.transports;
        if !t.pipe_inbox_dir.trim().is_empty() {
            dirs.inbox = PathBuf::from(t.pipe_inbox_dir.trim());
        }
        if !t.pipe_outbox_dir.trim().is_empty() {
            dirs.outbox = PathBuf::from(t.pipe_outbox_dir.trim());
        }
        dirs
    }

    /// Per-chat state (`$HATCH_HOME/chats/<chat-id>/`).
    pub fn chat_store(&self) -> hatch_transports::ChatStore {
        hatch_transports::ChatStore::new(&self.home)
    }

    /// Device capability manifests (`$HATCH_HOME/devices/`).
    pub fn capability_store(&self) -> hatch_transports::CapabilityStore {
        hatch_transports::CapabilityStore::new(&self.home)
    }

    /// Artifact/widget tools for the chat agent and the MCP server
    /// (`show_options`, `create_artifact`, `share_artifact`). They go
    /// through the same gate → approval queue → executor path as every
    /// other tool; the CLI's own `hatch artifact` commands use the store
    /// directly instead.
    pub fn artifact_tools(&self) -> Vec<Arc<dyn hatch_tools::Tool>> {
        hatch_artifacts::tools::artifact_tools(&self.artifacts_dir())
    }

    /// Tools offered by installed connectors (`connector_<name>_<op>`).
    /// A manifest that fails to parse is skipped with a warning; the agent
    /// keeps running without its tools.
    pub fn connector_tools(&self) -> Vec<Arc<dyn hatch_tools::Tool>> {
        hatch_tools::connector_tools(&self.connectors_dir())
    }

    pub fn fetch_limits(&self) -> FetchLimits {
        FetchLimits {
            max_bytes: self.config.web.fetch_max_kb.max(1) * 1024,
            ..FetchLimits::default()
        }
    }

    /// The command sandbox. The helper is this very binary, re-executed with
    /// the hidden `__sandbox-helper` subcommand.
    pub fn sandbox(&self) -> Result<Sandbox> {
        let s = &self.config.sandbox;
        let mode = SandboxMode::parse(&s.mode)?;
        let helper = match mode {
            SandboxMode::Namespaces => Some(HelperCmd {
                program: std::env::current_exe()?,
                prefix_args: vec![HELPER_SUBCOMMAND.into()],
            }),
            SandboxMode::None => None,
        };
        Ok(Sandbox {
            mode,
            helper,
            limits: SandboxLimits {
                cpu_secs: s.cpu_secs,
                memory_mb: s.memory_mb,
                file_size_mb: s.file_size_mb,
                max_processes: s.max_processes,
                ..SandboxLimits::default()
            },
            max_output: s.max_output_kb.max(1) * 1024,
        })
    }

    /// The workspace: `--workspace` overrides `[workspace] root`; none = file/shell tools off.
    pub fn workspace(&self, override_dir: Option<&Path>) -> Result<Option<Workspace>> {
        let root = match override_dir {
            Some(p) => p.to_path_buf(),
            None if self.config.workspace.root.trim().is_empty() => return Ok(None),
            None => PathBuf::from(self.config.workspace.root.trim()),
        };
        Ok(Some(Workspace::new(&root)?))
    }

    pub fn general_tools(&self, override_dir: Option<&Path>) -> Result<GeneralTools> {
        Ok(GeneralTools {
            workspace: self.workspace(override_dir)?,
            web_fetch: self.config.web.fetch,
            search_backend: (!self.config.web.search_url.is_empty())
                .then(|| format!("searxng:{}", self.config.web.search_url)),
            max_command_timeout_secs: self.config.sandbox.max_timeout_secs,
        })
    }

    pub fn source(&self, fixture: Option<&Path>) -> Result<Box<dyn MailSource>> {
        let m = &self.config.mail;
        let fixture = fixture.map(Path::to_path_buf).or_else(|| {
            (m.source == "fixture" && !m.fixture_path.is_empty())
                .then(|| PathBuf::from(&m.fixture_path))
        });
        if let Some(p) = fixture {
            let account = if self.user_email().is_empty() {
                "fixture".to_string()
            } else {
                self.user_email()
            };
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
        let router = app
            .router(Limits {
                max_calls: Some(2),
                max_tokens: Some(800),
                reasoning_effort: None,
            })
            .unwrap();
        assert_eq!(router.chain(Workload::Triage)[0].name(), "env:test-model");
        std::env::remove_var(crate::config::ENV_BASE_URL);
        std::env::remove_var(crate::config::ENV_MODEL);
        std::env::remove_var(crate::config::ENV_API_KEY);
        assert!(
            app.router(app.default_limits()).is_err(),
            "no models without env or config"
        );
        assert!(!dir.path().join("config.toml").exists());
        // Default sender is the outbox, never SMTP.
        app.sender(false).unwrap();
        assert!(app.outbox_dir().exists());
    }
}
