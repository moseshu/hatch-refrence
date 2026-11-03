//! `hatch memory ...`

use crate::app::App;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_memory::{AddOutcome, Importance, MemoryStore, NewClaim};
use std::path::Path;

#[derive(Subcommand)]
pub enum MemoryCmd {
    /// 记住一条事实或偏好
    Add {
        #[arg(long, default_value = "user")]
        subject: String,
        text: String,
        /// 逗号分隔，如 preference,style
        #[arg(long, default_value = "")]
        tags: String,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        k: usize,
    },
    /// 查看来源与修订历史
    Explain { id: String },
    /// 纠正：追加新版本并链接旧版本（历史保留）
    Correct { id: String, text: String },
    /// 遗忘：删除一条记忆的全部版本，或某个主题的全部记忆
    Forget {
        id: Option<String>,
        #[arg(long)]
        subject: Option<String>,
    },
    /// 联系人列表
    People,
    /// 新增/更新联系人，importance 为 vip|normal|low
    Person {
        email: String,
        name: String,
        #[arg(long, default_value = "normal")]
        importance: String,
        #[arg(long, default_value = "")]
        notes: String,
    },
}

pub fn run(home: Option<&Path>, cmd: MemoryCmd) -> Result<()> {
    let app = App::open(home)?;
    let m = MemoryStore::new(&app.db);
    let now = hatch_core::time::now();
    match cmd {
        MemoryCmd::Add { subject, text, tags } => {
            let tags: Vec<&str> = tags.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
            match m.add(&NewClaim::stated(&subject, &text, "user:cli").with_tags(&tags), &now)? {
                AddOutcome::Added(id) => println!("已记住 {id}"),
                AddOutcome::Duplicate(id) => println!("已存在相同记忆 {id}"),
            }
        }
        MemoryCmd::Search { query, k } => {
            let hits = m.search(&query, k, &now)?;
            if hits.is_empty() {
                println!("没有相关记忆。");
            }
            for (c, score) in hits {
                println!("{}  [{}] {:.2}  {}  （来源 {}，置信度 {:.2}）", c.id, c.subject, score, c.text, c.source, c.confidence);
            }
        }
        MemoryCmd::Explain { id } => {
            let ex = m.explain(&id)?;
            println!("{}  {}\n  主题 {}  来源 {}  信任 {}  置信度 {:.2}{}", ex.claim.id, ex.claim.text, ex.claim.subject, ex.claim.source, ex.claim.trust.as_str(), ex.claim.confidence, if ex.claim.inferred { "  推断" } else { "" });
            for h in ex.history {
                println!("  ← 替代了 {}  {}", h.id, h.text);
            }
            for s in ex.successors {
                println!("  → 已被替代为 {}  {}", s.id, s.text);
            }
        }
        MemoryCmd::Correct { id, text } => {
            let old = m.get(&id)?.ok_or_else(|| anyhow::anyhow!("没有记忆 {id}"))?;
            let mut c = NewClaim::stated(&old.subject, &text, "user:cli");
            c.tags = old.tags.clone();
            println!("已纠正：{id} → {}", m.correct(&id, &c, &now)?);
        }
        MemoryCmd::Forget { id, subject } => {
            let report = match (id, subject) {
                (Some(id), None) => m.forget_claim(&id)?,
                (None, Some(s)) => m.forget_subject(&s)?,
                _ => bail!("请指定一个记忆 id，或 --subject"),
            };
            println!("已删除 {} 条记忆{}。\n{}", report.claims_removed, if report.person_removed { "及联系人页" } else { "" }, report.caveat);
        }
        MemoryCmd::People => {
            for p in m.people().list()? {
                println!("{:<32} {:<10} {:<7} 往来 {:>3}  {}", p.email, p.name, p.importance.as_str(), p.interactions, p.notes);
            }
        }
        MemoryCmd::Person { email, name, importance, notes } => {
            m.people().upsert(&email, &name, Importance::parse(&importance)?, &notes)?;
            println!("已保存联系人 {email}");
        }
    }
    Ok(())
}
