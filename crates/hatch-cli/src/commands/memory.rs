//! `hatch memory ...`

use crate::app::App;
use anyhow::{bail, Result};
use clap::Subcommand;
use hatch_core::CancelToken;
use hatch_memory::{load_synthesis, AddOutcome, Importance, MemoryStore, NewClaim};
use hatch_workers::{memory_extract, MemoryExtractWorker, MemorySynthesisWorker};
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
    /// 手动调一条记忆的显著度（0–1，只改排序权重，不产生新版本）
    SetSalience { id: String, salience: f64 },
    /// 遗忘：删除一条记忆的全部版本，或某个主题的全部记忆
    Forget {
        id: Option<String>,
        #[arg(long)]
        subject: Option<String>,
    },
    /// 强制全量重建检索索引（bank 不动；索引损坏或漂移时用）
    Reindex,
    /// 从会话记录提炼持久记忆（默认扫描全部有新消息的会话）
    Extract {
        /// 只提炼这个会话
        #[arg(long)]
        session: Option<String>,
    },
    /// 手动综合一次“当前理解”文档（默认 12 小时内已生成过则跳过）
    Synthesize {
        /// 忽略 12 小时内的生成记录，强制重新综合
        #[arg(long)]
        force: bool,
    },
    /// 每夜综合：查看“当前理解”文档
    Synthesis {
        #[command(subcommand)]
        cmd: SynthesisCmd,
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

#[derive(Subcommand)]
pub enum SynthesisCmd {
    /// 查看最新综合文档（未生成过时提示）
    Show,
}

pub fn run(home: Option<&Path>, cmd: MemoryCmd) -> Result<()> {
    let app = App::open(home)?;
    let m = MemoryStore::new(&app.db);
    let now = hatch_core::time::now();
    match cmd {
        MemoryCmd::Add {
            subject,
            text,
            tags,
        } => {
            let tags: Vec<&str> = tags
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            match m.add(
                &NewClaim::stated(&subject, &text, "user:cli").with_tags(&tags),
                &now,
            )? {
                AddOutcome::Added(id) => println!("已记住 {id}"),
                AddOutcome::Duplicate(id) => println!("已存在相同记忆 {id}"),
            }
        }
        MemoryCmd::Search { query, k } => {
            let m = MemoryStore::with_ranking(&app.db, app.ranking_config());
            let hits = m.search(&query, k, &now)?;
            if hits.is_empty() {
                println!("没有相关记忆。");
            }
            for (c, score) in hits {
                println!(
                    "{}  [{}] {:.2}  {}  （来源 {}，置信度 {:.2}，显著度 {:.2}）",
                    c.id, c.subject, score, c.text, c.source, c.confidence, c.salience
                );
            }
        }
        MemoryCmd::Explain { id } => {
            let ex = m.explain(&id)?;
            println!(
                "{}  {}\n  主题 {}  来源 {}  信任 {}  置信度 {:.2}  显著度 {:.2}  最后使用 {}{}",
                ex.claim.id,
                ex.claim.text,
                ex.claim.subject,
                ex.claim.source,
                ex.claim.trust.as_str(),
                ex.claim.confidence,
                ex.claim.salience,
                ex.claim
                    .last_used_at
                    .map(|t| hatch_core::time::to_rfc3339(&t))
                    .unwrap_or_else(|| "从未".into()),
                if ex.claim.inferred { "  推断" } else { "" }
            );
            for h in ex.history {
                println!("  ← 替代了 {}  {}", h.id, h.text);
            }
            for s in ex.successors {
                println!("  → 已被替代为 {}  {}", s.id, s.text);
            }
        }
        MemoryCmd::Correct { id, text } => {
            let old = m
                .get(&id)?
                .ok_or_else(|| anyhow::anyhow!("没有记忆 {id}"))?;
            let mut c = NewClaim::stated(&old.subject, &text, "user:cli");
            c.tags = old.tags.clone();
            println!("已纠正：{id} → {}", m.correct(&id, &c, &now)?);
        }
        MemoryCmd::SetSalience { id, salience } => {
            if !(0.0..=1.0).contains(&salience) {
                bail!("显著度必须在 0–1 之间");
            }
            m.set_salience(&id, salience)?;
            println!("已设置 {id} 的显著度为 {salience:.2}");
        }
        MemoryCmd::Forget { id, subject } => {
            let report = match (id, subject) {
                (Some(id), None) => m.forget_claim(&id)?,
                (None, Some(s)) => m.forget_subject(&s)?,
                _ => bail!("请指定一个记忆 id，或 --subject"),
            };
            println!(
                "已删除 {} 条记忆{}。\n{}",
                report.claims_removed,
                if report.person_removed {
                    "及联系人页"
                } else {
                    ""
                },
                report.caveat
            );
        }
        MemoryCmd::Reindex => {
            let r = m.reindex()?;
            println!(
                "索引已重建：bank {} 条，索引 {} 条。",
                r.bank_docs, r.indexed_docs
            );
        }
        MemoryCmd::Extract { session } => {
            let w = MemoryExtractWorker {
                router: crate::sched::router_factory(&app),
                attention: app.attention(),
            };
            let r = match &session {
                Some(id) => w.extract_session(&app.db, id, &now, &CancelToken::new())?,
                None => w.extract_all(&app.db, &now, &CancelToken::new())?,
            };
            println!(
                "扫描 {} 个会话，新增 {} 条，待决定 {} 组，跳过凭据类 {} 条、过长 {} 条。{}",
                r.sessions,
                r.added.len(),
                r.proposals.len(),
                r.skipped_secrets,
                r.skipped_long,
                r.note.as_deref().unwrap_or("")
            );
            if !r.added.is_empty() || !r.proposals.is_empty() {
                println!("\n{}", memory_extract::render(&r));
            }
        }
        MemoryCmd::Synthesize { force } => {
            let w = MemorySynthesisWorker {
                router: crate::sched::router_factory(&app),
            };
            let r = w.synthesize(&app.db, &now, &CancelToken::new(), force)?;
            if r.generated {
                println!("已综合 {} 条记忆。", r.source_claims);
            } else {
                println!("未生成。{}", r.note.as_deref().unwrap_or(""));
            }
        }
        MemoryCmd::Synthesis { cmd } => match cmd {
            SynthesisCmd::Show => match load_synthesis(&app.db)? {
                None => println!("尚未生成过综合文档（每夜 02:30 的 memory-synthesis 任务，或 `hatch memory synthesize` 手动生成）。"),
                Some(doc) => println!(
                    "# 当前理解（每夜综合）\n\n生成时间：{}；来源：{} 条记忆。\n\n{}",
                    hatch_core::time::to_rfc3339(&doc.generated_at),
                    doc.source_claims,
                    doc.content
                ),
            },
        },
        MemoryCmd::People => {
            for p in m.people().list()? {
                println!(
                    "{:<32} {:<10} {:<7} 往来 {:>3}  {}",
                    p.email,
                    p.name,
                    p.importance.as_str(),
                    p.interactions,
                    p.notes
                );
            }
        }
        MemoryCmd::Person {
            email,
            name,
            importance,
            notes,
        } => {
            m.people()
                .upsert(&email, &name, Importance::parse(&importance)?, &notes)?;
            println!("已保存联系人 {email}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time::now;
    use hatch_memory::{save_synthesis, SynthesisDoc};

    #[test]
    fn set_salience_command_updates_weight() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let m = MemoryStore::new(&app.db);
        let t = now();
        let id = m
            .add(&NewClaim::stated("user", "回复邮件要简洁", "user:chat"), &t)
            .unwrap()
            .id()
            .to_string();
        assert!(run(
            Some(dir.path()),
            MemoryCmd::SetSalience { id: id.clone(), salience: 0.9 }
        )
        .is_ok());
        assert_eq!(m.get(&id).unwrap().unwrap().salience, 0.9);
        assert!(run(
            Some(dir.path()),
            MemoryCmd::SetSalience { id: id.clone(), salience: 1.5 }
        )
        .is_err());
        assert_eq!(m.get(&id).unwrap().unwrap().salience, 0.9, "rejected write changes nothing");
    }

    #[test]
    fn synthesis_show_reports_missing_then_prints_stored_doc() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        assert!(run(Some(dir.path()), MemoryCmd::Synthesis { cmd: SynthesisCmd::Show }).is_ok());
        assert!(load_synthesis(&app.db).unwrap().is_none());
        let t = now();
        save_synthesis(
            &app.db,
            &SynthesisDoc { content: "## 用户画像\n喜欢简洁。".into(), generated_at: t, source_claims: 2 },
        )
        .unwrap();
        assert!(run(Some(dir.path()), MemoryCmd::Synthesis { cmd: SynthesisCmd::Show }).is_ok());
        assert_eq!(load_synthesis(&app.db).unwrap().unwrap().source_claims, 2);
    }

    #[test]
    fn synthesize_without_model_skips_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let m = MemoryStore::new(&app.db);
        m.add(&NewClaim::stated("user", "回复邮件要简洁", "user:chat"), &now()).unwrap();
        assert!(run(Some(dir.path()), MemoryCmd::Synthesize { force: false }).is_ok());
        assert!(load_synthesis(&app.db).unwrap().is_none(), "skipped run writes nothing");
    }

    #[test]
    fn reindex_command_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open(Some(dir.path())).unwrap();
        let m = MemoryStore::new(&app.db);
        let t = now();
        m.add(&NewClaim::stated("user", "回复邮件要简洁", "user:chat"), &t)
            .unwrap();
        app.db
            .conn()
            .execute("DELETE FROM memory_index_docs", [])
            .unwrap();
        assert!(run(Some(dir.path()), MemoryCmd::Reindex).is_ok());
        assert_eq!(m.verify_index().unwrap().indexed_docs, 1);
    }
}
