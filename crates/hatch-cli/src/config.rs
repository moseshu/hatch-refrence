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
/// Declared context window in tokens for the env model (§3.4).
/// Unset or unparsable = the model does not declare one.
pub const ENV_CONTEXT_WINDOW: &str = "HATCH_LLM_CONTEXT_WINDOW";
/// Accepted `provider` values. `openai_compatible` and `openai_chat` are the
/// same Chat Completions protocol.
pub const PROVIDERS: &[&str] = &[
    "openai_compatible",
    "openai_chat",
    "openai_responses",
    "anthropic",
    "gemini",
];
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
        UserConfig {
            name: "用户".into(),
            email: String::new(),
            timezone: "Asia/Shanghai".into(),
        }
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
    /// §3.4: this model's declared context window in tokens (static
    /// configuration, OMB #23). `None` = the model does not declare one:
    /// the absolute total budget is the only cap. Never a per-model
    /// hardcoded value — the user writes it here (or
    /// `HATCH_LLM_CONTEXT_WINDOW` for the env model).
    #[serde(default)]
    pub context_window: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub main: Vec<String>,
    pub triage: Vec<String>,
    pub draft: Vec<String>,
    pub compaction: Vec<String>,
    pub memory_flush: Vec<String>,
    pub brief: Vec<String>,
    pub feed: Vec<String>,
    /// Prompt-cache TTL per workload, in seconds. Only Anthropic honours
    /// it (300 = 5m, 3600 = 1h — the only values Anthropic accepts);
    /// OpenAI-compatible / Responses and Gemini ignore it (their cached
    /// tokens are still observed from usage and shown in cache stats).
    /// Example: `[routing.cache_ttl_secs] brief = 300`.
    pub cache_ttl_secs: BTreeMap<String, u64>,
    /// Fallback ladder overrides, one per workload + trigger.
    /// Example: `[[routing.fallback]] workload = "main"
    /// trigger = "server_error" action = "stop"`.
    /// Triggers: refused_params / timeout / server_error / empty_answer.
    /// Actions: retry_same / next_model / stop. Anything not listed
    /// keeps the conservative default (see hatch_llm::FallbackTable):
    /// refused_params → next_model, timeout / server_error → retry_same
    /// once then next model, empty_answer → stop.
    pub fallback: Vec<FallbackRuleConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FallbackRuleConfig {
    pub workload: String,
    pub trigger: String,
    pub action: String,
}

/// `[feed]` — the daily feed worker (hatch-goals). One prompt drives the
/// whole feed; empty = the built-in default.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedSection {
    pub prompt: String,
}

impl Default for FeedSection {
    fn default() -> Self {
        FeedSection { prompt: hatch_goals::DEFAULT_PROMPT.into() }
    }
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
        LimitsConfig {
            max_calls_per_run: Some(100),
            max_tokens: Some(4000),
            reasoning_effort: None,
            stream: false,
        }
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
        AttentionConfig {
            quiet_start: 22,
            quiet_end: 8,
            daily_budget: 5,
        }
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
        SandboxConfig {
            mode: "namespaces".into(),
            max_timeout_secs: 600,
            cpu_secs: 120,
            memory_mb: 2048,
            file_size_mb: 64,
            max_processes: 512,
            max_output_kb: 64,
        }
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
        WebConfig {
            fetch: true,
            fetch_max_kb: 512,
            search_url: String::new(),
        }
    }
}

/// `[browser]` — the browser tools. Off unless enabled.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserSection {
    pub enabled: bool,
    /// Chromium / Chrome executable; empty = search the usual places (or `HATCH_CHROMIUM`).
    pub chromium_path: String,
    pub headless: bool,
    /// Parent directory of the per-profile data; empty = `$HATCH_HOME/browser/profiles`.
    pub profiles_dir: String,
    pub max_tabs: usize,
    pub idle_timeout_secs: u64,
    /// Host patterns that are never loaded (`example.com`, `*.example.com`).
    pub blocklist: Vec<String>,
    /// PEM bundle imported into each profile's certificate store; empty = `HATCH_CA_BUNDLE` / `SSL_CERT_FILE`.
    pub ca_bundle: String,
    /// Test setups only: allow 127.0.0.1 / localhost.
    pub allow_loopback: bool,
    /// Test setups only: ignore TLS certificate errors.
    pub ignore_cert_errors: bool,
}

impl Default for BrowserSection {
    fn default() -> Self {
        BrowserSection {
            enabled: false,
            chromium_path: String::new(),
            headless: true,
            profiles_dir: String::new(),
            max_tabs: 4,
            idle_timeout_secs: 600,
            blocklist: Vec::new(),
            ca_bundle: String::new(),
            allow_loopback: false,
            ignore_cert_errors: false,
        }
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
    /// Wall-clock timeout per sub-agent process run, in seconds.
    pub subagent_timeout_secs: u64,
    pub max_depth: usize,
    pub max_parallel: usize,
    /// Plan mode: the agent first produces a structured plan and pauses for
    /// approval instead of executing directly (also `--plan` on chat/ask).
    pub plan_mode: bool,
    /// D2: re-anchor every N steps while a turn is bound to a plan
    /// (also `--plan-id` on chat/ask). Default 2: the model drifts within
    /// ~3 rounds, so the anchor must return before the third round.
    pub reanchor_every: usize,
    /// P2: half-life in days for the recall decay on `last_used_at`
    /// (search score = BM25 × salience × decay). Default 90.
    pub salience_half_life_days: f64,
    /// §3.4: total context budget in (estimated) tokens — a quality /
    /// cost choice, not a window. Default 64k: a starting proposal to
    /// be validated by the 32k / 64k / 128k experiment, not a settled
    /// number.
    pub total_budget: usize,
    /// §3.4: fraction of the declared context window that also caps
    /// the total (`effective = min(total_budget, window × ratio)`).
    /// Default 0.25.
    pub window_ratio: f64,
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            max_steps: 24,
            subagents: true,
            subagent_timeout_secs: 600,
            max_depth: 2,
            max_parallel: 3,
            plan_mode: false,
            reanchor_every: 2,
            salience_half_life_days: 90.0,
            total_budget: 64_000,
            window_ratio: 0.25,
        }
    }
}

/// `[scheduler]` — the daemon and its jobs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerSection {
    /// Seconds between ticks of `hatch daemon run`.
    pub tick_secs: u64,
    /// A firing at most this many minutes late still counts as on time.
    pub grace_minutes: i64,
    /// Model calls allowed per job run (each run gets a fresh budget).
    pub max_calls_per_job: u64,
}

impl Default for SchedulerSection {
    fn default() -> Self {
        SchedulerSection {
            tick_secs: 60,
            grace_minutes: 10,
            max_calls_per_job: 60,
        }
    }
}

/// `[tasks]` — the unified task ledger's live cadence (P1 D6/D7).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksSection {
    /// Seconds between heartbeat writes of a running task. 60s: matches
    /// the daemon tick, so a job running between ticks heartbeats about
    /// once per tick; 5x under the watchdog's staleness window and 30x
    /// under the startup safety net, so a live task can never look stale
    /// from scheduling jitter. One UPDATE per minute per running task.
    pub heartbeat_secs: u64,
    /// Seconds between liveness probes of the task watchdog thread.
    pub watchdog_probe_secs: u64,
    /// A `running` task whose heartbeat is older than this is presumed
    /// dead by the live watchdog (revived to `waiting`, or `failed` when
    /// retries are out). 300s: 5x the heartbeat interval, so a dead
    /// owner is found within ~5 minutes while a live task — heartbeat at
    /// most ~60s old — can never trip it, even with missed beats.
    pub watchdog_stale_after_secs: u64,
}

impl Default for TasksSection {
    fn default() -> Self {
        TasksSection {
            heartbeat_secs: 60,
            watchdog_probe_secs: 60,
            watchdog_stale_after_secs: 300,
        }
    }
}

/// `[daemon]` — the daemon's embedded product API (`/v1/...`, served by
/// `hatch daemon run --api`). The API shares one backend with the web and
/// desktop clients; it never does TLS itself (use a reverse proxy), and a
/// non-loopback bind without a bearer token is refused at startup.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonSection {
    /// Serve the product API from `hatch daemon run` without `--api`.
    pub http_enabled: bool,
    /// Bind address for the API (default `127.0.0.1:8787`).
    pub http_bind: String,
}

impl Default for DaemonSection {
    fn default() -> Self {
        DaemonSection {
            http_enabled: false,
            http_bind: "127.0.0.1:8787".into(),
        }
    }
}

/// `[skills]` — skills loaded into every chat turn.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsSection {
    /// Skill names loaded at turn start (also see `hatch skills load`).
    pub load: Vec<String>,
}

/// `[caldav]` — CalDAV bidirectional sync (Phase 6). The server URL and
/// username are plain config; the credential is only a vault token
/// *name* (`token_ref`) — never a token value. `allow_http` is only
/// for the local mock server (loopback hosts); real servers must use
/// https://.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CaldavConfig {
    pub server_url: String,
    pub username: String,
    pub token_ref: String,
    /// Explicit calendar collection URL; empty = discover via PROPFIND.
    pub calendar_url: String,
    /// Allow plain http:// — loopback hosts only (local mock server).
    pub allow_http: bool,
}

impl Default for CaldavConfig {
    fn default() -> Self {
        CaldavConfig {
            server_url: String::new(),
            username: String::new(),
            token_ref: "caldav".into(),
            calendar_url: String::new(),
            allow_http: false,
        }
    }
}

/// `[artifacts]` — quotas for versioned artifacts (single-user v1:
/// the quota counts the whole home). Every field is optional; an
/// unset field uses the default, and `0` means unlimited for that
/// dimension. Environment variables override the file:
/// `HATCH_ARTIFACT_MAX_COUNT`, `HATCH_ARTIFACT_MAX_TOTAL_BYTES`,
/// `HATCH_ARTIFACT_MAX_FILE_BYTES` (an unparseable value is ignored
/// and the file/default value stands).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactsSection {
    /// Max artifacts (default 200).
    pub max_artifacts: Option<usize>,
    /// Max total bytes across all versions (default 100 MiB).
    pub max_total_bytes: Option<u64>,
    /// Max bytes of one version's content (default 5 MiB).
    pub max_file_bytes: Option<u64>,
}

/// `[inference]` — inference proxy: route model calls through a
/// local reverse proxy for usage accounting, centralized retry and
/// rate limiting. Everything is off unless `proxy_url` is set.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceSection {
    /// Proxy URL the agent routes model calls through, e.g.
    /// `http://127.0.0.1:8199` (no `/v1`: the provider client
    /// appends `/responses` itself, and the proxy forwards the
    /// request path as-is onto the upstream base URL).
    /// Empty = direct to the provider.
    pub proxy_url: String,
    /// Upstream base URL the proxy forwards to, e.g.
    /// `https://api.deepseek.com/v1` (used by `hatch inference proxy`).
    pub upstream_base_url: String,
    /// Env var holding the upstream API key for the proxy
    /// (default `HATCH_LLM_API_KEY`).
    pub api_key_env: String,
    /// Bind address for `hatch inference proxy`
    /// (default `127.0.0.1:8199`).
    pub bind: String,
    /// Retries after the first attempt (default 3).
    pub max_retries: Option<u32>,
    /// Requests allowed per minute (default 60; 0 = unlimited).
    pub rate_limit_per_min: Option<u32>,
    /// Price per million input tokens, for cost estimates.
    pub price_input_per_million: Option<f64>,
    /// Price per million output tokens, for cost estimates.
    pub price_output_per_million: Option<f64>,
    /// Currency of the prices (default `CNY`).
    pub price_currency: String,
}

impl InferenceSection {
    pub fn api_key_env(&self) -> &str {
        if self.api_key_env.is_empty() {
            "HATCH_LLM_API_KEY"
        } else {
            &self.api_key_env
        }
    }

    pub fn bind(&self) -> &str {
        if self.bind.is_empty() {
            "127.0.0.1:8199"
        } else {
            &self.bind
        }
    }

    pub fn currency(&self) -> &str {
        if self.price_currency.is_empty() {
            "CNY"
        } else {
            &self.price_currency
        }
    }

    pub fn pricing(&self) -> hatch_inference::Pricing {
        hatch_inference::Pricing {
            input_per_million: self.price_input_per_million,
            output_per_million: self.price_output_per_million,
            currency: self.currency().to_string(),
        }
    }
}

/// `[transports]` — multi-channel transports (Phase 5: only the pipe
/// reference transport exists; real channels are out of scope).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransportsSection {
    /// Override for the pipe transport's inbox directory (empty =
    /// `$HATCH_HOME/transports/pipe/inbox`).
    pub pipe_inbox_dir: String,
    /// Override for the pipe transport's outbox directory (empty =
    /// `$HATCH_HOME/transports/pipe/outbox`).
    pub pipe_outbox_dir: String,
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
    pub browser: BrowserSection,
    pub agent: AgentSection,
    pub scheduler: SchedulerSection,
    pub daemon: DaemonSection,
    pub tasks: TasksSection,
    pub skills: SkillsSection,
    pub transports: TransportsSection,
    pub artifacts: ArtifactsSection,
    pub inference: InferenceSection,
    pub caldav: CaldavConfig,
    pub feed: FeedSection,
    /// MCP servers (stdio or HTTP transport). `env` / `headers` hold
    /// variable *names*; `token_ref` is a vault token name — never a
    /// secret value.
    pub mcp_servers: Vec<hatch_mcp::McpServerConfig>,
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
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Env-defined model, if all three variables are set and non-empty.
    pub fn env_model() -> Option<ResolvedModel> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let (base, model, key) = (get(ENV_BASE_URL)?, get(ENV_MODEL)?, get(ENV_API_KEY)?);
        let provider = get(ENV_PROVIDER).unwrap_or_else(|| "openai_compatible".into());
        let context_window = get(ENV_CONTEXT_WINDOW).and_then(|v| v.parse::<usize>().ok());
        Some(ResolvedModel {
            cfg: ModelConfig {
                name: "env".into(),
                provider,
                base_url: base,
                model,
                api_key_env: ENV_API_KEY.into(),
                context_window,
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
                api_key: if m.api_key_env.is_empty() {
                    None
                } else {
                    std::env::var(&m.api_key_env).ok().filter(|v| !v.is_empty())
                },
            })
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        for m in &self.models {
            if !PROVIDERS.contains(&m.provider.as_str()) {
                bail!(
                    "model {}: provider must be one of {}",
                    m.name,
                    PROVIDERS.join(" / ")
                );
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
        hatch_core::time::parse_tz(&self.user.timezone)
            .map_err(|e| anyhow::anyhow!("[user] {e}"))?;
        hatch_sandbox::SandboxMode::parse(&self.sandbox.mode)
            .map_err(|e| anyhow::anyhow!("[sandbox] {e}"))?;
        if self.agent.max_steps == 0 {
            bail!("[agent] max_steps must be at least 1");
        }
        if self.agent.reanchor_every == 0 {
            bail!("[agent] reanchor_every must be at least 1");
        }
        if self.agent.total_budget == 0 {
            bail!("[agent] total_budget must be at least 1");
        }
        if !(self.agent.window_ratio > 0.0 && self.agent.window_ratio <= 1.0) {
            bail!("[agent] window_ratio must be in (0, 1]");
        }
        for m in &self.models {
            if m.context_window == Some(0) {
                bail!("model {}: context_window must be at least 1", m.name);
            }
        }
        let mut mcp_names = std::collections::HashSet::new();
        for s in &self.mcp_servers {
            if s.name.trim().is_empty() {
                bail!("[mcp_servers] every server needs a name");
            }
            match s.transport.as_str() {
                "stdio" | "" => {
                    if s.command.trim().is_empty() {
                        bail!("[mcp_servers] server `{}` needs a command", s.name);
                    }
                }
                "http" => {
                    if s.url.trim().is_empty() {
                        bail!(
                            "[mcp_servers] server `{}`: http transport needs a url",
                            s.name
                        );
                    }
                    if !(s.url.starts_with("http://") || s.url.starts_with("https://")) {
                        bail!(
                            "[mcp_servers] server `{}`: url must start with http:// or https://",
                            s.name
                        );
                    }
                }
                t => bail!(
                    "[mcp_servers] server `{}`: unknown transport `{t}` (stdio|http)",
                    s.name
                ),
            }
            for h in &s.headers {
                if h.trim().is_empty() {
                    bail!("[mcp_servers] server `{}`: empty header name", s.name);
                }
            }
            if !mcp_names.insert(s.name.clone()) {
                bail!("[mcp_servers] duplicate server name `{}`", s.name);
            }
            if let Some(t) = &s.token_ref {
                if !hatch_vault::is_valid_name(t) {
                    bail!("[mcp_servers] server `{}`: invalid token_ref `{t}`", s.name);
                }
            }
        }
        for name in &self.skills.load {
            if !hatch_skills::parse::valid_skill_name(name) {
                bail!("[skills] invalid skill name `{name}`");
            }
        }
        if !self.caldav.server_url.is_empty() {
            hatch_connectors::caldav::client::validate_server_url(
                &self.caldav.server_url,
                self.caldav.allow_http,
            )
            .map_err(|e| anyhow::anyhow!("[caldav] {e}"))?;
            if !self.caldav.token_ref.is_empty()
                && !hatch_vault::is_valid_name(&self.caldav.token_ref)
            {
                bail!("[caldav] invalid token_ref `{}`", self.caldav.token_ref);
            }
        }
        for (w, ttl) in &self.routing.cache_ttl_secs {
            if hatch_llm::Workload::parse(w).is_none() {
                bail!("[routing.cache_ttl_secs] unknown workload `{w}`");
            }
            if *ttl != 300 && *ttl != 3600 {
                bail!(
                    "[routing.cache_ttl_secs] workload `{w}`: ttl {ttl} is not supported (Anthropic accepts only 300 or 3600)"
                );
            }
        }
        for f in &self.routing.fallback {
            if hatch_llm::Workload::parse(&f.workload).is_none() {
                bail!("[routing.fallback] unknown workload `{}`", f.workload);
            }
            if hatch_llm::LadderTrigger::parse(&f.trigger).is_none() {
                bail!("[routing.fallback] unknown trigger `{}` (refused_params / timeout / server_error / empty_answer)", f.trigger);
            }
            if hatch_llm::LadderAction::parse(&f.action).is_none() {
                bail!("[routing.fallback] unknown action `{}` (retry_same / next_model / stop)", f.action);
            }
        }
        Ok(())
    }

    /// Effective artifact quota: environment overrides the file,
    /// the file overrides the defaults (`Quota::default`). An
    /// unparseable env value is ignored; `0` means unlimited.
    pub fn artifact_quota(&self) -> hatch_artifacts::Quota {
        fn env_u64(name: &str) -> Option<u64> {
            std::env::var(name).ok()?.trim().parse().ok()
        }
        let d = hatch_artifacts::Quota::default();
        hatch_artifacts::Quota {
            max_artifacts: env_u64("HATCH_ARTIFACT_MAX_COUNT")
                .map(|v| v as usize)
                .or(self.artifacts.max_artifacts)
                .unwrap_or(d.max_artifacts),
            max_total_bytes: env_u64("HATCH_ARTIFACT_MAX_TOTAL_BYTES")
                .or(self.artifacts.max_total_bytes)
                .unwrap_or(d.max_total_bytes),
            max_file_bytes: env_u64("HATCH_ARTIFACT_MAX_FILE_BYTES")
                .or(self.artifacts.max_file_bytes)
                .unwrap_or(d.max_file_bytes),
        }
    }

    /// CalDAV sync settings, or `None` when `[caldav] server_url` is unset.
    pub fn caldav_settings(&self) -> Option<hatch_connectors::caldav::SyncSettings> {
        if self.caldav.server_url.trim().is_empty() {
            return None;
        }
        let mut s = hatch_connectors::caldav::SyncSettings::new(
            &self.caldav.server_url,
            &self.caldav.username,
            &self.caldav.token_ref,
        );
        if !self.caldav.calendar_url.trim().is_empty() {
            s.calendar_url = Some(self.caldav.calendar_url.clone());
        }
        Some(s)
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
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
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
    fn context_engineering_config_parses_and_validates() {
        // §3.4: [agent] total_budget / window_ratio, [[models]] context_window.
        let c = AppConfig::parse(
            "[[models]]\nname = \"m\"\nprovider = \"openai_compatible\"\nbase_url = \"https://e\"\nmodel = \"m\"\ncontext_window = 128000\n[agent]\ntotal_budget = 32000\nwindow_ratio = 0.5\n",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.models[0].context_window, Some(128_000));
        assert_eq!(c.agent.total_budget, 32_000);
        assert_eq!(c.agent.window_ratio, 0.5);
        // Omitted = unknown window + 64k / 0.25 defaults.
        let c = AppConfig::parse("").unwrap();
        assert_eq!(c.models.len(), 0);
        assert_eq!(c.agent.total_budget, 64_000);
        assert_eq!(c.agent.window_ratio, 0.25);
    }

    #[test]
    fn context_engineering_config_rejects_degenerate_values() {
        let bad = AppConfig::parse("[agent]\ntotal_budget = 0\n").unwrap();
        assert!(bad.validate().is_err());
        for ratio in ["0", "1.5", "-0.25"] {
            let bad = AppConfig::parse(&format!("[agent]\nwindow_ratio = {ratio}\n")).unwrap();
            assert!(bad.validate().is_err(), "ratio {ratio} must be rejected");
        }
        let bad = AppConfig::parse(
            "[[models]]\nname = \"m\"\nprovider = \"openai_compatible\"\nbase_url = \"https://e\"\nmodel = \"m\"\ncontext_window = 0\n",
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn artifact_quota_defaults_file_and_env() {
        let c = AppConfig::parse("").unwrap();
        let q = c.artifact_quota();
        assert_eq!(q.max_artifacts, 200);
        assert_eq!(q.max_total_bytes, 100 * 1024 * 1024);
        assert_eq!(q.max_file_bytes, 5 * 1024 * 1024);
        let c = AppConfig::parse("[artifacts]\nmax_artifacts = 10\nmax_file_bytes = 0\n").unwrap();
        let q = c.artifact_quota();
        assert_eq!(q.max_artifacts, 10);
        assert_eq!(q.max_file_bytes, 0, "0 means unlimited");
        assert_eq!(
            q.max_total_bytes,
            100 * 1024 * 1024,
            "unset field keeps its default"
        );
        // Env wins over the file; an unparseable value is ignored.
        // (Names are unique to this feature; no other test reads them.)
        let prev = std::env::var("HATCH_ARTIFACT_MAX_COUNT").ok();
        std::env::set_var("HATCH_ARTIFACT_MAX_COUNT", "3");
        std::env::set_var("HATCH_ARTIFACT_MAX_TOTAL_BYTES", "bogus");
        let q = c.artifact_quota();
        assert_eq!(q.max_artifacts, 3);
        assert_eq!(q.max_total_bytes, 100 * 1024 * 1024);
        match prev {
            Some(v) => std::env::set_var("HATCH_ARTIFACT_MAX_COUNT", v),
            None => std::env::remove_var("HATCH_ARTIFACT_MAX_COUNT"),
        }
        std::env::remove_var("HATCH_ARTIFACT_MAX_TOTAL_BYTES");
    }

    #[test]
    fn defaults_and_bad_values() {
        let c = AppConfig::parse("").unwrap();
        assert_eq!(c.user.timezone, "Asia/Shanghai");
        assert_eq!(
            c.mail.sender, "outbox",
            "default sender never sends real mail"
        );
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

    #[test]
    fn routing_cache_ttl_and_fallback_validation() {
        // Valid: Anthropic TTLs only, known workloads/triggers/actions.
        let ok = "[routing.cache_ttl_secs]\nbrief = 300\nfeed = 3600\n[[routing.fallback]]\nworkload = \"main\"\ntrigger = \"server_error\"\naction = \"stop\"\n";
        AppConfig::parse(ok).unwrap().validate().unwrap();
        let c = AppConfig::parse("").unwrap();
        assert!(!c.feed.prompt.is_empty(), "feed prompt defaults to the built-in prompt");
        assert!(c.routing.cache_ttl_secs.is_empty());
        // TTLs other than 300/3600 are rejected (Anthropic accepts nothing else).
        let bad_ttl = "[routing.cache_ttl_secs]\nbrief = 600\n";
        assert!(AppConfig::parse(bad_ttl).unwrap().validate().is_err());
        let bad_w = "[routing.cache_ttl_secs]\nnope = 300\n";
        assert!(AppConfig::parse(bad_w).unwrap().validate().is_err());
        let bad_t = "[[routing.fallback]]\nworkload = \"main\"\ntrigger = \"explode\"\naction = \"stop\"\n";
        assert!(AppConfig::parse(bad_t).unwrap().validate().is_err());
        let bad_a = "[[routing.fallback]]\nworkload = \"main\"\ntrigger = \"timeout\"\naction = \"panic\"\n";
        assert!(AppConfig::parse(bad_a).unwrap().validate().is_err());
    }

    #[test]
    fn caldav_config_validation() {
        // Unset = off, no error.
        assert!(AppConfig::parse("").unwrap().caldav_settings().is_none());
        // https is fine; http needs allow_http + loopback.
        let ok = "[caldav]\nserver_url = \"https://cal.example.com/\"\nusername = \"u\"\ntoken_ref = \"caldav\"\n";
        let c = AppConfig::parse(ok).unwrap();
        c.validate().unwrap();
        assert!(c.caldav_settings().is_some());
        let bad = "[caldav]\nserver_url = \"http://cal.example.com/\"\n";
        assert!(AppConfig::parse(bad).unwrap().validate().is_err());
        let mock = "[caldav]\nserver_url = \"http://127.0.0.1:8090/\"\nallow_http = true\n";
        AppConfig::parse(mock).unwrap().validate().unwrap();
        // http + allow_http but a non-loopback host is still rejected.
        let bad2 = "[caldav]\nserver_url = \"http://cal.example.com/\"\nallow_http = true\n";
        assert!(AppConfig::parse(bad2).unwrap().validate().is_err());
        // Invalid token_ref name is rejected (never a value in config).
        let bad3 =
            "[caldav]\nserver_url = \"https://cal.example.com/\"\ntoken_ref = \"has space\"\n";
        assert!(AppConfig::parse(bad3).unwrap().validate().is_err());
    }
}
