//! `hatch questions ...` — answer `ask_user` questions from the CLI.
//!
//! A question asked during a chat turn blocks that turn until it is
//! answered, rejected, or expires. From a second terminal (or while
//! the API serves the turn) this is how the user answers.

use crate::app::App;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_tools::builtin::ask_user::{answer_question, AskUserPayload, Choice, ASK_KIND};
use std::path::Path;

#[derive(Subcommand)]
pub enum QuestionsCmd {
    /// 列出等你回答的问题
    List,
    /// 回答一个问题（choice 为选项序号 1..n 或选项文字；--text 为自由输入）
    Answer {
        id: String,
        choice: Option<String>,
        #[arg(long)]
        text: Option<String>,
    },
}

pub fn run(home: Option<&Path>, cmd: QuestionsCmd) -> Result<()> {
    let app = App::open(home)?;
    let q = ApprovalQueue::new(&app.db);
    let now = hatch_core::time::now();
    q.expire_due(&now)?;
    match cmd {
        QuestionsCmd::List => {
            let list: Vec<_> = q
                .list(Some(ApprovalStatus::Pending))?
                .into_iter()
                .filter(|a| a.kind == ASK_KIND)
                .collect();
            if list.is_empty() {
                println!("没有等你回答的问题。");
            }
            for a in list {
                println!(
                    "{}  {}  （{} 到期）",
                    a.id,
                    a.title,
                    fmt_local(&a.expires_at, &app.tz)
                );
                if let Ok(p) = serde_json::from_value::<AskUserPayload>(a.payload.clone()) {
                    for (i, o) in p.options.iter().enumerate() {
                        println!("  {}. {}", i + 1, o.label);
                    }
                }
            }
        }
        QuestionsCmd::Answer { id, choice, text } => {
            let choice = match choice {
                Some(c) => match c.parse::<usize>() {
                    Ok(n) if n >= 1 => Some(Choice::Index(n - 1)),
                    _ => Some(Choice::Label(c)),
                },
                None => None,
            };
            if choice.is_none() && text.is_none() {
                bail!("需要给出 choice（序号或选项文字）或 --text 自由输入");
            }
            let answer = answer_question(&app.db, &id, choice, text, &now)?;
            println!("已回答 {id}：{}", answer["label"].as_str().unwrap_or(""));
            if let Some(t) = answer["text"].as_str() {
                println!("附言：{t}");
            }
        }
    }
    Ok(())
}
