//! `hatch transport ...` — multi-channel transports (Phase 5).
//!
//! Only the `pipe` reference transport exists: inbox JSON files in,
//! agent turns per chat, outbox JSON files out. Real channels
//! (WeChat / Feishu / Telegram) are out of scope — `list` names them
//! with guidance instead of a transport.
//!
//! * `list` — registered transports + the known-but-unimplemented
//!   real channels. `poll <name>` — one pass over the inbox;
//!   `run <name>` — poll in a loop until Ctrl-C. `chats` — chats
//!   with state, their discretion rules and pending worker drafts.
//! * `discretion show/set <chat-id>` — the per-chat sharing rules.
//! * `device list/show/check/init` — the device capability manifest
//!   (a missing capability is answered honestly, with its enable
//!   path, never faked).
//!
//! A transport turn is a main-agent turn: the same gate, approval
//! queue and tool registry as `hatch chat` (general + artifact +
//! skill tools; browser and MCP tools are not offered on transport
//! turns in this phase), the same database (so main and side chats
//! share memory and goals), and one session per chat id (so
//! transcripts stay separate). The answer is relayed through
//! [`Relay`](hatch_transports::Relay): discretion check, widget text
//! fallback, artifact delivery via `store.deliver`.

use crate::app::App;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_agent::{Agent, AgentConfig};
use hatch_llm::Router;
use hatch_policy::Gatekeeper;
use hatch_tools::builtin::{add_general_tools, default_registry};
use hatch_tools::{ExecutorRegistry, Registry};
use hatch_transports::{
    executed_deliveries, labels_for_tool_calls, CapabilityManifest, CapabilityStore, ChatStore,
    InboundMessage, PipeTransport, Relay, Transport, TransportRegistry, TurnOutcome, TurnRunner,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Subcommand)]
pub enum TransportCmd {
    /// 列出已注册的传输与已知但未实现的真实通道
    List,
    /// 持续运行一个传输（轮询收件箱），Ctrl-C 停止
    Run {
        name: String,
        /// 两次轮询之间的秒数（默认 5）
        #[arg(long)]
        interval_secs: Option<u64>,
        /// 允许文件与命令工具访问的目录（覆盖配置 [workspace] root）
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 对一个传输轮询一次
    Poll {
        name: String,
        /// 允许文件与命令工具访问的目录（覆盖配置 [workspace] root）
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 列出有状态的聊天（会话、分享规则、待转述草稿）
    Chats,
    /// 每聊天的分享规则（DISCRETION.md）
    Discretion {
        #[command(subcommand)]
        cmd: DiscretionCmd,
    },
    /// 设备能力清单（声明式；缺失的能力如实告知开通路径）
    Device {
        #[command(subcommand)]
        cmd: DeviceCmd,
    },
}

#[derive(Subcommand)]
pub enum DiscretionCmd {
    /// 查看一个聊天的分享规则
    Show { chat: String },
    /// 写入一个聊天的分享规则（--rule 可重复，或 --file 读文件）
    Set {
        chat: String,
        /// 一条规则，如 "forbid: mail"（可重复）
        #[arg(long)]
        rule: Vec<String>,
        /// 从文件读规则
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum DeviceCmd {
    /// 列出有能力清单的设备
    List,
    /// 查看一个设备的能力清单
    Show { device: String },
    /// 检查一个设备是否支持某能力（不支持时给出开通路径）
    Check { device: String, capability: String },
    /// 为一个设备创建能力清单（从本机默认清单复制，再手动编辑）
    Init { device: String },
}

/// The agent-turn callback for a transport: owns the router, gate,
/// executors and registry; builds a main-agent turn per message.
struct TransportRunner {
    db: Arc<hatch_store::Db>,
    router: Router,
    gate: Gatekeeper,
    executors: ExecutorRegistry,
    tools: Registry,
    delegation: Option<hatch_agent::Delegation>,
    user_name: String,
    user_email: String,
    tz: chrono_tz::Tz,
    max_tokens: Option<u32>,
    max_steps: usize,
    skills: Vec<String>,
    skill_tools: Option<Vec<String>>,
}

impl TransportRunner {
    fn build(app: &App, workspace: Option<&Path>) -> Result<Self> {
        let router = app.router(app.default_limits())?;
        let gate = app.policy()?;
        let executors = app.executors(false)?;
        let general = app.general_tools(workspace)?;
        let deleg = crate::commands::chat::delegation(app, general.workspace.as_ref());
        let mut tools = add_general_tools(default_registry(), &general);
        for t in app.artifact_tools() {
            tools = tools.with(t);
        }
        let skill_reg = Arc::new(app.skill_registry());
        tools = hatch_tools::builtin::skills::add_skill_tools(tools, skill_reg, &app.home);
        let active = app.active_skills();
        let mut skill_tools: Option<Vec<String>> = None;
        if !active.is_empty() {
            let mut allowed: Vec<String> = Vec::new();
            for s in &active {
                for t in &s.tools {
                    if !allowed.contains(t) {
                        allowed.push(t.clone());
                    }
                }
            }
            if !allowed.is_empty() {
                skill_tools = Some(allowed.clone());
                let mut keep: Vec<&str> = allowed.iter().map(String::as_str).collect();
                keep.extend(["list_skills", "load_skill"]);
                tools = tools.filtered(&keep);
            }
        }
        let skill_bodies: Vec<String> = active
            .iter()
            .map(|s| format!("技能 {}：\n{}", s.name, s.body))
            .collect();
        Ok(TransportRunner {
            db: app.db.clone(),
            router,
            gate,
            executors,
            tools,
            delegation: deleg,
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            max_tokens: app.config.limits.max_tokens,
            max_steps: app.config.agent.max_steps,
            skills: skill_bodies,
            skill_tools,
        })
    }
}

impl TurnRunner for TransportRunner {
    fn run_turn(
        &self,
        _msg: &InboundMessage,
        session: &str,
        input: &str,
    ) -> hatch_core::Result<TurnOutcome> {
        let agent = Agent {
            db: &self.db,
            router: &self.router,
            tools: &self.tools,
            executors: &self.executors,
            gate: &self.gate,
            config: AgentConfig {
                user_name: self.user_name.clone(),
                user_email: self.user_email.clone(),
                tz: self.tz,
                max_tokens: self.max_tokens,
                max_steps: self.max_steps,
                delegation: self.delegation.clone(),
                skills: self.skills.clone(),
                skill_tools: self.skill_tools.clone(),
                origin: Some("transport:pipe".into()),
                system_note: Some(
                    "你正在通过外部聊天通道回复对方；你的回答会原样发给对方。不要在回答里写内部推理、秘密或你没有把握的事；不确定的事直接说不确定。".into(),
                ),
                ..AgentConfig::default()
            },
        };
        let now = hatch_core::time::now();
        let report = agent.run_turn(session, input, &now)?;
        let artifact_id = executed_deliveries(&self.db, session)?.last().cloned();
        Ok(TurnOutcome {
            text: report.answer.clone().unwrap_or_else(|| report.receipt()),
            // Widgets surface as their text fallback inside the answer
            // (same as `hatch chat`): the turn has no structured widget
            // side-channel, and the pipe transport has no UI.
            widget: None,
            artifact_id,
            labels: labels_for_tool_calls(&report.tool_calls),
            pending_approvals: report.approvals.clone(),
        })
    }
}

fn pipe_transport(app: &App) -> Result<(Arc<PipeTransport>, ChatStore, CapabilityStore)> {
    let chats = app.chat_store();
    let pipe = Arc::new(PipeTransport::new(
        app.pipe_dirs(),
        chats.clone(),
        app.artifacts_dir(),
        app.db.clone(),
    ));
    Ok((pipe, chats, app.capability_store()))
}

fn relay_for(
    app: &App,
    pipe: &Arc<PipeTransport>,
    chats: &ChatStore,
    caps: &CapabilityStore,
) -> Relay {
    let device = caps.local().ok();
    Relay::new(pipe.clone(), chats.clone(), app.artifacts_dir(), device)
}

fn print_report(name: &str, report: &hatch_transports::PollReport) {
    println!(
        "{name}：已处理 {} 条：已发送 {}，拦截 {}，失败 {}",
        report.processed, report.sent, report.blocked, report.failed
    );
    for e in &report.errors {
        println!("   失败：{e}");
    }
}

pub fn run(home: Option<&Path>, cmd: TransportCmd) -> Result<()> {
    let app = App::open(home)?;
    match cmd {
        TransportCmd::List => {
            let reg = TransportRegistry::builtin(&app.home, app.db.clone());
            for s in reg.statuses() {
                println!(
                    "{} {}  {}",
                    if s.configured { "✅" } else { "❌" },
                    s.name,
                    s.detail
                );
                println!("   指引：{}", s.guidance);
            }
            for s in TransportRegistry::not_implemented() {
                println!("➖ {}  {}", s.name, s.detail);
                println!("   指引：{}", s.guidance);
            }
        }
        TransportCmd::Poll { name, workspace } => {
            let (pipe, chats, caps) = pipe_transport(&app)?;
            if name != pipe.name() {
                return unknown_transport(&name);
            }
            let runner = TransportRunner::build(&app, workspace.as_deref())?;
            let relay = relay_for(&app, &pipe, &chats, &caps);
            let report =
                pipe.poll_once(&runner, &|msg, outcome| relay.deliver_main(msg, outcome))?;
            print_report(&name, &report);
        }
        TransportCmd::Run {
            name,
            interval_secs,
            workspace,
        } => {
            let (pipe, chats, caps) = pipe_transport(&app)?;
            if name != pipe.name() {
                return unknown_transport(&name);
            }
            let runner = TransportRunner::build(&app, workspace.as_deref())?;
            let relay = relay_for(&app, &pipe, &chats, &caps);
            let interval = std::time::Duration::from_secs(interval_secs.unwrap_or(5).max(1));
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let _ = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst));
            println!(
                "hatch transport run {name}：每 {} 秒轮询一次，Ctrl-C 停止。",
                interval.as_secs()
            );
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let report =
                    pipe.poll_once(&runner, &|msg, outcome| relay.deliver_main(msg, outcome))?;
                if report.processed > 0 || !report.errors.is_empty() {
                    print_report(&name, &report);
                }
                std::thread::sleep(interval);
            }
            println!("hatch transport run {name} 已停止。");
        }
        TransportCmd::Chats => {
            let (pipe, chats, caps) = pipe_transport(&app)?;
            let relay = relay_for(&app, &pipe, &chats, &caps);
            let list = chats.list_chats()?;
            if list.is_empty() {
                println!("还没有聊天（往 pipe 收件箱放一条消息再 `hatch transport poll pipe`）。");
                return Ok(());
            }
            for id in list {
                let rules = chats.read_discretion(&id).unwrap_or_default();
                let forbidden: Vec<&str> = rules.forbidden.iter().map(|k| k.display()).collect();
                let drafts = relay.pending_drafts(&id).unwrap_or_default();
                println!(
                    "{}  禁止分享：{}  待转述草稿：{} 条",
                    id,
                    if forbidden.is_empty() {
                        "无（秘密始终禁止）".to_string()
                    } else {
                        forbidden.join("、")
                    },
                    drafts.len()
                );
            }
        }
        TransportCmd::Discretion { cmd } => match cmd {
            DiscretionCmd::Show { chat } => {
                let chats = app.chat_store();
                let path = chats.discretion_path(&chat)?;
                match std::fs::read_to_string(&path) {
                    Ok(text) => println!("{}：\n{text}", path.display()),
                    Err(_) => println!(
                        "聊天 {chat} 没有 DISCRETION.md（默认开放，秘密始终禁止）。\n文件位置：{}",
                        path.display()
                    ),
                }
            }
            DiscretionCmd::Set { chat, rule, file } => {
                let text = match (file, rule.is_empty()) {
                    (Some(p), _) => std::fs::read_to_string(&p)?,
                    (None, false) => rule.join("\n"),
                    (None, true) => bail!("需要 --rule \"forbid: mail\"（可重复）或 --file <文件>"),
                };
                let rules = app.chat_store().write_discretion(&chat, &text)?;
                let forbidden: Vec<&str> = rules.forbidden.iter().map(|k| k.display()).collect();
                println!(
                    "聊天 {chat} 的分享规则已写入：{}",
                    if forbidden.is_empty() {
                        "无额外限制（秘密始终禁止）".to_string()
                    } else {
                        format!("禁止分享 {}", forbidden.join("、"))
                    }
                );
            }
        },
        TransportCmd::Device { cmd } => match cmd {
            DeviceCmd::List => {
                let store = app.capability_store();
                let devices = store.list_devices()?;
                if devices.is_empty() {
                    println!(
                        "没有设备能力清单（`hatch transport device init local` 创建本机的）。"
                    );
                    return Ok(());
                }
                for id in devices {
                    let m = store.load(&id)?;
                    let supported: Vec<&str> = m
                        .capabilities
                        .iter()
                        .filter(|c| c.supported)
                        .map(|c| c.name.as_str())
                        .collect();
                    println!("{}（{}）：支持 {}", id, m.device_name, supported.join(", "));
                }
            }
            DeviceCmd::Show { device } => {
                let m = app.capability_store().load(&device)?;
                println!("{}", serde_json::to_string_pretty(&m)?);
            }
            DeviceCmd::Check { device, capability } => {
                let m = app.capability_store().load(&device)?;
                if m.supports(&capability) {
                    println!("✅ 设备 {} 支持 {capability}", m.device_name);
                } else {
                    println!("{}", hatch_transports::missing_reply(&m, &capability));
                }
            }
            DeviceCmd::Init { device } => {
                let mut m = CapabilityManifest::local_default();
                m.device_id = device.clone();
                m.device_name = device.clone();
                app.capability_store().save(&m)?;
                println!(
                    "已为设备 {device} 创建能力清单：{}（按实际能力编辑该文件，supported 改为 true 即声明支持）",
                    app.capability_store().manifest_path(&device).display()
                );
            }
        },
    }
    Ok(())
}

fn unknown_transport(name: &str) -> Result<()> {
    if ["wechat", "feishu", "telegram"].contains(&name) {
        bail!("{name} 是已知但本阶段未实现的真实通道（OUT OF SCOPE），没有真实接入；本阶段请用 pipe 参考传输。见 `hatch transport list`。")
    }
    bail!("没有名为 {name} 的传输（`hatch transport list` 查看）。")
}

/// `hatch doctor` 的多通道一节（`doctor.rs` 调用）。
pub fn doctor_section(app: &App) -> Result<()> {
    let reg = TransportRegistry::builtin(&app.home, app.db.clone());
    for s in reg.statuses() {
        println!(
            "   {} {}  {}",
            if s.configured { "✅" } else { "❌" },
            s.name,
            s.detail
        );
    }
    for s in TransportRegistry::not_implemented() {
        println!("   ➖ {}  {}", s.name, s.detail);
    }
    let chats = app.chat_store().list_chats().unwrap_or_default();
    println!("   聊天：{} 个（{}）", chats.len(), chats.join(", "));
    let devices = app.capability_store().list_devices().unwrap_or_default();
    println!(
        "   设备能力清单：{} 个（{}）",
        devices.len(),
        devices.join(", ")
    );
    Ok(())
}
