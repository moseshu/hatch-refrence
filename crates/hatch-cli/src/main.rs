//! `hatch` — a personal agent assistant for mail and calendar.

mod app;
mod commands;
mod config;
mod interrupt;
mod sched;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "hatch",
    version,
    about = "hatch — 个人 Agent 助手（邮件与日程、记忆、工具、浏览器、子代理、定时任务、技能、MCP、审批）"
)]
struct Cli {
    /// 数据目录（默认 $HATCH_HOME 或 ~/.hatch）
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 创建数据目录、配置文件与数据库
    Init {
        #[arg(long)]
        force: bool,
    },
    /// 邮件 + 日程分诊
    Triage {
        #[command(subcommand)]
        cmd: commands::triage::TriageCmd,
    },
    /// 审批队列
    Approvals {
        #[command(subcommand)]
        cmd: commands::approvals::ApprovalsCmd,
    },
    /// 常设授权
    Grants {
        #[command(subcommand)]
        cmd: commands::grants::GrantsCmd,
    },
    /// 长期记忆与联系人
    Memory {
        #[command(subcommand)]
        cmd: commands::memory::MemoryCmd,
    },
    /// 日历
    Calendar {
        #[command(subcommand)]
        cmd: commands::calendar::CalendarCmd,
    },
    /// 交互式对话（Ctrl-C 停止当前任务，连按两次退出）
    Chat {
        /// 继续指定会话（同 --resume）
        #[arg(long, alias = "resume")]
        session: Option<String>,
        /// 允许文件与命令工具访问的目录（覆盖配置 [workspace] root）
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// 开启新会话（默认继续最近一次）
        #[arg(long)]
        new: bool,
        /// 流式输出回答（也可在配置 [limits] stream = true）
        #[arg(long)]
        stream: bool,
        /// 计划模式：先制定结构化计划、等批准后再执行（也可在配置 [agent] plan_mode = true）
        #[arg(long)]
        plan: bool,
    },
    /// 单次提问
    Ask {
        text: String,
        #[arg(long, alias = "resume")]
        session: Option<String>,
        /// 允许文件与命令工具访问的目录（覆盖配置 [workspace] root）
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// 流式输出回答（也可在配置 [limits] stream = true）
        #[arg(long)]
        stream: bool,
        /// 计划模式：先制定结构化计划、等批准后再执行（也可在配置 [agent] plan_mode = true）
        #[arg(long)]
        plan: bool,
    },
    /// 查看未读通知摘要
    Digest {
        /// 包含静默通知
        #[arg(long)]
        all: bool,
        /// 标记为已读
        #[arg(long)]
        mark_read: bool,
    },
    /// 运行沙盒评估
    Eval {
        /// 评估世界目录（默认 eval/worlds/zh-office；另有 workspace-basic、delegate-basic、scheduler-basic、browser-basic）
        #[arg(long)]
        world: Option<PathBuf>,
        /// 使用真实模型（会产生费用）
        #[arg(long)]
        live: bool,
        /// live 模式的模型调用上限（默认不限制）
        #[arg(long)]
        max_calls: Option<u64>,
        /// live 模式每次调用的 max_tokens（推理模型的思考 token 也计入）
        #[arg(long, default_value_t = 4000)]
        max_tokens: u32,
        /// 推理模型的推理强度：low / medium / high（默认取配置或环境变量）
        #[arg(long)]
        reasoning_effort: Option<String>,
        /// 报告输出目录（写入 report.json 与 report.md）
        #[arg(long)]
        out: Option<PathBuf>,
        /// 同一世界重复运行 N 次，报告取均值（默认 1；仅邮件分诊世界支持）
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
    /// 文件检查点：查看与回退 hatch 做过的文件修改
    Checkpoint {
        #[command(subcommand)]
        cmd: commands::checkpoint::CheckpointCmd,
    },
    /// 子代理：类型与运行记录
    Agents {
        #[command(subcommand)]
        cmd: commands::agents::AgentsCmd,
    },
    /// 会话：列表、查看、复制（fork）、回退（rewind）
    Session {
        #[command(subcommand)]
        cmd: commands::session::SessionCmd,
    },
    /// 定时任务：新建、列表、启停、立即运行、运行记录
    Jobs {
        #[command(subcommand)]
        cmd: commands::jobs::JobsCmd,
    },
    /// 每日简报
    Brief {
        #[command(subcommand)]
        cmd: commands::brief::BriefCmd,
    },
    /// 目标：创建、列表、查看、完成/暂停/恢复/放弃
    Goal {
        #[command(subcommand)]
        cmd: commands::goal::GoalCmd,
    },
    /// 点子库：记录、列表、忽略（已忽略永不出现）、采纳
    Ideas {
        #[command(subcommand)]
        cmd: commands::ideas::IdeasCmd,
    },
    /// 每日动态（feed）：列表、查看
    Feed {
        #[command(subcommand)]
        cmd: commands::feed::FeedCmd,
    },
    /// 常驻进程：按计划运行任务
    Daemon {
        #[command(subcommand)]
        cmd: commands::daemon::DaemonCmd,
    },
    /// 浏览器：进程状态、会话、接管与恢复
    Browser {
        #[command(subcommand)]
        cmd: commands::browser::BrowserCmd,
    },
    /// 技能：列表、查看、激活与取消
    Skills {
        #[command(subcommand)]
        cmd: commands::skills::SkillsCmd,
    },
    /// MCP 服务器：列表、测试与调用（stdio/HTTP），以及作为服务端运行（stdio）
    Mcp {
        #[command(subcommand)]
        cmd: commands::mcp::McpCmd,
    },
    /// 凭据保管（vault helper 进程 + 0600 文件）：存入、列表（只显示名字）、删除、状态、停止、OAuth
    Vault {
        #[command(subcommand)]
        cmd: commands::vault::VaultCmd,
    },
    /// 连接器：列表、状态检查与调用（清单在 $HATCH_HOME/connectors/*.toml）
    Connectors {
        #[command(subcommand)]
        cmd: commands::connectors::ConnectorsCmd,
    },
    /// 计划模式：先制定结构化计划、等批准后再执行
    Plan {
        #[command(subcommand)]
        cmd: commands::plan::PlanCmd,
    },
    /// 交付物：带版本的文档/HTML 卡片（创建、列表、查看、分享、交付）
    Artifact {
        #[command(subcommand)]
        cmd: commands::artifact::ArtifactCmd,
    },
    /// 多通道传输：pipe 参考传输、每聊天分享规则、设备能力清单
    Transport {
        #[command(subcommand)]
        cmd: commands::transport::TransportCmd,
    },
    /// 自检：配置、数据库、模型、邮箱、沙箱、浏览器与定时任务
    Doctor,
}

fn main() {
    // The same binary doubles as the sandbox helper (spawned by `shell_run`).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(app::HELPER_SUBCOMMAND) {
        hatch_sandbox::helper_main(&args[2..]);
    }
    let cli = Cli::parse();
    let home = cli.home.as_deref();
    let result = match cli.cmd {
        Cmd::Init { force } => commands::init::run(home, force),
        Cmd::Triage { cmd } => commands::triage::run(home, cmd),
        Cmd::Approvals { cmd } => commands::approvals::run(home, cmd),
        Cmd::Grants { cmd } => commands::grants::run(home, cmd),
        Cmd::Memory { cmd } => commands::memory::run(home, cmd),
        Cmd::Calendar { cmd } => commands::calendar::run(home, cmd),
        Cmd::Chat {
            session,
            workspace,
            new,
            stream,
            plan,
        } => commands::chat::repl(home, session, new, stream, workspace.as_deref(), plan),
        Cmd::Ask {
            text,
            session,
            workspace,
            stream,
            plan,
        } => commands::chat::ask(home, &text, session, stream, workspace.as_deref(), plan),
        Cmd::Checkpoint { cmd } => commands::checkpoint::run(home, cmd),
        Cmd::Digest { all, mark_read } => commands::digest::run(home, all, mark_read),
        Cmd::Eval {
            world,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
            repeat,
        } => commands::eval::run(
            home,
            world,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
            repeat,
        ),
        Cmd::Agents { cmd } => commands::agents::run(home, cmd),
        Cmd::Session { cmd } => commands::session::run(home, cmd),
        Cmd::Jobs { cmd } => commands::jobs::run(home, cmd),
        Cmd::Brief { cmd } => commands::brief::run(home, cmd),
        Cmd::Goal { cmd } => commands::goal::run(home, cmd),
        Cmd::Ideas { cmd } => commands::ideas::run(home, cmd),
        Cmd::Feed { cmd } => commands::feed::run(home, cmd),
        Cmd::Daemon { cmd } => commands::daemon::run(home, cmd),
        Cmd::Browser { cmd } => commands::browser::run(home, cmd),
        Cmd::Skills { cmd } => commands::skills::run(home, cmd),
        Cmd::Mcp { cmd } => commands::mcp::run(home, cmd),
        Cmd::Vault { cmd } => commands::vault::run(home, cmd),
        Cmd::Connectors { cmd } => commands::connectors::run(home, cmd),
        Cmd::Plan { cmd } => commands::plan::run(home, cmd),
        Cmd::Artifact { cmd } => commands::artifact::run(home, cmd),
        Cmd::Transport { cmd } => commands::transport::run(home, cmd),
        Cmd::Doctor => commands::doctor::run(home),
    };
    if let Err(e) = result {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}
