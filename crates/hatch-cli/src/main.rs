//! `hatch` — a personal agent assistant for mail and calendar.

mod app;
mod commands;
mod config;

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
    /// 交互式对话
    Chat {
        /// 继续指定会话
        #[arg(long)]
        session: Option<String>,
        /// 开启新会话（默认继续最近一次）
        #[arg(long)]
        new: bool,
    },
    /// 单次提问
    Ask {
        text: String,
        #[arg(long)]
        session: Option<String>,
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
        /// 评估世界目录（默认 eval/worlds/zh-office）
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
    /// 自检：配置、数据库、模型与邮箱设置
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    let home = cli.home.as_deref();
    let result = match cli.cmd {
        Cmd::Init { force } => commands::init::run(home, force),
        Cmd::Triage { cmd } => commands::triage::run(home, cmd),
        Cmd::Approvals { cmd } => commands::approvals::run(home, cmd),
        Cmd::Grants { cmd } => commands::grants::run(home, cmd),
        Cmd::Memory { cmd } => commands::memory::run(home, cmd),
        Cmd::Calendar { cmd } => commands::calendar::run(home, cmd),
        Cmd::Chat { session, new } => commands::chat::repl(home, session, new),
        Cmd::Ask { text, session } => commands::chat::ask(home, &text, session),
        Cmd::Digest { all, mark_read } => commands::digest::run(home, all, mark_read),
        Cmd::Eval { world, live, max_calls, max_tokens, reasoning_effort, out } => {
            commands::eval::run(home, world, live, max_calls, max_tokens, reasoning_effort, out)
        }
        Cmd::Doctor => commands::doctor::run(home),
    };
    if let Err(e) = result {
        eprintln!("错误：{e:#}");
        std::process::exit(1);
    }
}
