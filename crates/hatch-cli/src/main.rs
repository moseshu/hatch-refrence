//! `hatch` — a personal agent assistant for mail and calendar.

mod app;
mod commands;
mod config;
mod interrupt;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hatch", version, about = "hatch — 个人 Agent 助手（邮件 + 日程分诊、记忆、审批）")]
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
        /// 评估世界目录（默认 eval/worlds/zh-office；另有 workspace-basic、delegate-basic）
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
    /// 自检：配置、数据库、模型、邮箱与沙箱
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
        Cmd::Chat { session, workspace, new, stream } => commands::chat::repl(home, session, new, stream, workspace.as_deref()),
        Cmd::Ask { text, session, workspace, stream } => commands::chat::ask(home, &text, session, stream, workspace.as_deref()),
        Cmd::Checkpoint { cmd } => commands::checkpoint::run(home, cmd),
        Cmd::Digest { all, mark_read } => commands::digest::run(home, all, mark_read),
        Cmd::Eval { world, live, max_calls, max_tokens, reasoning_effort, out } => {
            commands::eval::run(home, world, live, max_calls, max_tokens, reasoning_effort, out)
        }
        Cmd::Agents { cmd } => commands::agents::run(home, cmd),
        Cmd::Session { cmd } => commands::session::run(home, cmd),
        Cmd::Doctor => commands::doctor::run(home),
    };
    if let Err(e) = result {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}
