//! `hatch release ...` — task 9: versioned releases, canary
//! rollouts, feature flags, pre-release verification, eval
//! samples.
//!
//! A release pins the prompts (hash of the compiled-in
//! templates), the skill versions and the execution policy
//! version. Activating a release never touches the ledger: tasks,
//! receipts and approvals are version-independent, so a rollback
//! is an activation of the previous release, and what was
//! recorded under the rolled-back release stays queryable.

use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_agent::prompt::BEHAVIOR_TEMPLATE;
use hatch_policy::POLICY_VERSION;
use hatch_release::{
    all_passed, assign, prompt_sha256, EvalSample, FlagStore, ReleaseStore, RolloutStore,
    SampleStore, VerifyCheck,
};
use hatch_snapshots::SnapshotStore;
use hatch_store::SCHEMA_VERSION;
use hatch_triage::{classify::CLASSIFY_TEMPLATE, draft::DRAFT_TEMPLATE};
use hatch_workers::brief::compose::TEMPLATE as BRIEF_TEMPLATE;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Subcommand)]
pub enum ReleaseCmd {
    /// Create a release pinning the current prompts, skills and policy.
    Create {
        /// Human note for this release.
        #[arg(long)]
        note: Option<String>,
    },
    /// List releases, newest first (active marked).
    List,
    /// Activate a release (exactly one stays active).
    Activate { id: String },
    /// Roll back to the previous release.
    Rollback,
    /// Run the pre-release checks for a release.
    Verify { id: String },
    /// Set a feature flag on or off.
    Flag {
        name: String,
        /// on | off
        value: String,
    },
    /// List feature flags that were ever set.
    Flags,
    /// Canary rollout for a release.
    Rollout {
        #[command(subcommand)]
        cmd: RolloutCmd,
    },
    /// List eval samples, optionally filtered by attribution class.
    Samples {
        /// runtime | model | tool | context | permission | delivery
        #[arg(long)]
        class: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RolloutCmd {
    /// Create (or replace) a rollout with staged percentages.
    Create {
        release: String,
        /// Comma-separated stages, e.g. 10,50,100.
        #[arg(long, value_delimiter = ',')]
        stages: Vec<u8>,
    },
    /// Move to the next stage.
    Advance { release: String },
    /// Kill switch: stop new canary assignments.
    Kill { release: String },
    /// Show a rollout's stage, percentage and kill state.
    Status { release: String },
}

fn prompt_parts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("behavior_en", BEHAVIOR_TEMPLATE),
        ("triage_classify_zh", CLASSIFY_TEMPLATE),
        ("triage_draft_zh", DRAFT_TEMPLATE),
        ("brief_zh", BRIEF_TEMPLATE),
    ]
}

fn print_check(c: &VerifyCheck) {
    println!(
        "  {} {:<16} {}",
        if c.passed { "✔" } else { "✗" },
        c.name,
        c.detail
    );
}

fn print_sample(s: &EvalSample) {
    println!(
        "{}  {}  {} / {}  task {} step {}  {}",
        s.id,
        s.world,
        s.class.label(),
        s.stage.label(),
        s.task_id,
        s.step_id,
        s.summary
    );
}

pub fn run(home: Option<&Path>, cmd: ReleaseCmd) -> Result<()> {
    let app = App::open(home)?;
    let now = hatch_core::time::now();
    match cmd {
        ReleaseCmd::Create { note } => {
            let hash = prompt_sha256(&prompt_parts());
            let skills: BTreeMap<String, String> = app
                .skill_registry()
                .scan()?
                .into_iter()
                .map(|m| (m.name, m.version))
                .collect();
            let rel = ReleaseStore::new(&app.db).create(
                &hash,
                &skills,
                POLICY_VERSION,
                note.as_deref().unwrap_or(""),
                &now,
            )?;
            println!("release {} 已创建（未激活）", rel.id);
            println!("  prompts: {}", rel.prompt_hash);
            println!("  skills: {} 个", rel.skill_versions().len());
            println!("  policy: {}", rel.policy_version);
            println!("用 `hatch release activate {}` 激活。", rel.id);
            Ok(())
        }
        ReleaseCmd::List => {
            let list = ReleaseStore::new(&app.db).list()?;
            if list.is_empty() {
                println!("还没有任何 release。用 `hatch release create` 创建一个。");
                return Ok(());
            }
            for r in list {
                println!(
                    "{}  {}{}  prompts {}  policy {}  {}",
                    r.id,
                    r.created_at,
                    if r.active { "  ● 激活" } else { "" },
                    &r.prompt_hash[..12],
                    r.policy_version,
                    r.note
                );
            }
            Ok(())
        }
        ReleaseCmd::Activate { id } => {
            let rel = ReleaseStore::new(&app.db).activate(&id)?;
            println!("release {} 已激活。", rel.id);
            println!("  prompts: {}", rel.prompt_hash);
            println!("  policy: {}", rel.policy_version);
            Ok(())
        }
        ReleaseCmd::Rollback => {
            let store = ReleaseStore::new(&app.db);
            let prev = store
                .previous()?
                .ok_or_else(|| anyhow!("没有可回滚到的上一个版本"))?;
            let rel = store.activate(&prev.id)?;
            println!("已回滚到 release {}。", rel.id);
            println!("  账本不受影响：在途任务、回执与审批仍可查询。");
            Ok(())
        }
        ReleaseCmd::Verify { id } => {
            let store = SnapshotStore::new(&app.home.join("snapshots"));
            let checkpoints: Vec<(String, Option<i64>, Option<String>)> = store
                .list()?
                .into_iter()
                .map(|m| (m.id, m.schema_version, m.policy_version))
                .collect();
            let checks = hatch_release::verify_release(
                &app.db,
                &id,
                &checkpoints,
                SCHEMA_VERSION,
                POLICY_VERSION,
            )?;
            println!("发布前检查 release {id}：");
            for c in &checks {
                print_check(c);
            }
            if all_passed(&checks) {
                println!("全部通过，可以发布。");
                Ok(())
            } else {
                Err(anyhow!("发布前检查未通过：先修掉 ✗ 的项再发布"))
            }
        }
        ReleaseCmd::Flag { name, value } => {
            let enabled = match value.as_str() {
                "on" => true,
                "off" => false,
                _ => return Err(anyhow!("flag 值须是 on 或 off，收到 {value}")),
            };
            FlagStore::new(&app.db).set(&name, enabled, &now)?;
            println!("开关 {name} = {}。", if enabled { "on" } else { "off" });
            if name == hatch_release::NEW_TASK_ENTRY && !enabled {
                println!("  新任务入口已关闭；在途任务仍可查询，已发动作的回执仍可查询。");
            }
            Ok(())
        }
        ReleaseCmd::Flags => {
            let list = FlagStore::new(&app.db).list()?;
            if list.is_empty() {
                println!("没有设置过任何开关（未设置的开关默认开启）。");
                return Ok(());
            }
            for (name, enabled) in list {
                println!("{} = {}", name, if enabled { "on" } else { "off" });
            }
            Ok(())
        }
        ReleaseCmd::Rollout { cmd } => run_rollout(&app, cmd),
        ReleaseCmd::Samples { class } => {
            let store = SampleStore::new(&app.db);
            match class {
                Some(name) => {
                    let c = hatch_release::FailureClass::parse(&name)
                        .ok_or_else(|| anyhow!("未知的归因类 {name}"))?;
                    let list = store.list_by_class(c)?;
                    if list.is_empty() {
                        println!("归因类 {}（{}）暂无样本。", c.label(), c.as_str());
                    }
                    for s in list {
                        print_sample(&s);
                    }
                }
                None => {
                    let list = store.list_all(20)?;
                    if list.is_empty() {
                        println!("暂无评估样本。");
                    }
                    for s in list {
                        print_sample(&s);
                    }
                    let counts = store.count_by_class()?;
                    let summary: Vec<String> = counts
                        .iter()
                        .map(|(c, n)| format!("{} {}", c.label(), n))
                        .collect();
                    println!("按类统计：{}", summary.join("、"));
                }
            }
            Ok(())
        }
    }
}

fn run_rollout(app: &App, cmd: RolloutCmd) -> Result<()> {
    let now = hatch_core::time::now();
    let store = RolloutStore::new(&app.db);
    match cmd {
        RolloutCmd::Create { release, stages } => {
            let r = store.create(&release, &stages, &now)?;
            println!(
                "release {0} 的灰度已创建：阶段 {1:?}，当前 {2}%。",
                r.release_id,
                r.stages,
                r.current_percent()
            );
            println!(
                "  用 `hatch release rollout advance {0}` 放量，`kill` 随时停。",
                release
            );
            Ok(())
        }
        RolloutCmd::Advance { release } => {
            let r = store.advance(&release)?;
            println!(
                "release {0} 灰度进入第 {1} 阶段：{2}%。",
                r.release_id,
                r.stage_idx + 1,
                r.current_percent()
            );
            Ok(())
        }
        RolloutCmd::Kill { release } => {
            store.kill(&release)?;
            println!("release {release} 的灰度已停：不再分配新任务到灰度组，在途任务继续跑完。");
            Ok(())
        }
        RolloutCmd::Status { release } => {
            let r = store
                .get(&release)?
                .ok_or_else(|| anyhow!("release {release} 没有灰度"))?;
            println!("release {} 的灰度：", r.release_id);
            println!("  阶段：{:?}（当前第 {} 阶段）", r.stages, r.stage_idx + 1);
            println!("  当前放量：{}%", r.current_percent());
            println!("  kill switch：{}", if r.killed { "已停" } else { "未停" });
            // A deterministic example: the same task id always
            // lands in the same bucket, so the assignment is sticky.
            let probe = ["tsk_probe_a", "tsk_probe_b", "tsk_probe_c"];
            let inside: Vec<&str> = probe.iter().copied().filter(|id| assign(id, &r)).collect();
            println!(
                "  探测任务 {} 在灰度组（同一任务 id 每次判定相同）",
                if inside.is_empty() {
                    "无".to_string()
                } else {
                    inside.join("、")
                }
            );
            Ok(())
        }
    }
}
