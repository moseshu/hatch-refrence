//! `hatch plan` — plan mode from the command line.
//!
//! `new` asks the model for a structured plan and leaves it pending
//! approval; `approve` / `reject` / `cancel` record the user's decision
//! (with timestamp) on the plan; `edit` changes a step and sends the plan
//! back to pending approval; `run` executes an approved plan step by step
//! through the normal dispatch gate, stopping on any deviation.
//!
//! The planner sees the same full registry as the turn loop (built-ins,
//! browser, MCP, connectors, artifacts, skills — see `commands::tools`),
//! so a plan step can name any tool the executor can resolve. Approval
//! goes through the approval queue: creating (or editing) a plan submits
//! a `plan_approve` entry, and `approve` decides that entry and executes
//! the decision; the plan flips to `approved` only through that path.
//! Approving a plan approves the PLAN, not each step's future side
//! effects — every step still goes through the dispatch gate (and its own
//! approval queue) at execution time.

use crate::app::App;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use hatch_core::time::now;
use hatch_plan::{
    pending_plan_approval, request_plan, run_plan, PlanStatus, PlanStore, RiskLevel, RunDeps,
    StepEdit,
};
use hatch_policy::ApprovalQueue;
use hatch_tools::{execute_approved, ExecOutcome};
use std::path::Path;

#[derive(Subcommand)]
pub enum PlanCmd {
    /// 让模型为一项任务制定结构化计划（只计划，不执行，等待批准）
    New {
        /// 任务描述（计划会记录这句原话）
        task: String,
        /// 计划服务的持久目标（与任务描述分开存，可空）
        #[arg(long, default_value = "")]
        goal: String,
        /// 只保存为草稿，不进入待审批
        #[arg(long)]
        draft: bool,
    },
    /// 列出计划
    List {
        /// 只列出该状态的计划
        #[arg(long)]
        status: Option<String>,
    },
    /// 查看一个计划的步骤、状态与执行结果
    Show { id: String },
    /// 批准一个计划（记录批准人与时间；批准时快照全部步骤）
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 拒绝一个计划
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 取消一个计划（永不执行）
    Cancel {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 编辑一个步骤（改后计划回到待审批，需要重新批准）
    Edit {
        id: String,
        /// 步骤 id（如 s1）
        #[arg(long)]
        step: String,
        #[arg(long)]
        tool: Option<String>,
        /// 新参数，JSON 对象
        #[arg(long)]
        args: Option<String>,
        /// 新文件列表，逗号分隔
        #[arg(long)]
        files: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        expected: Option<String>,
        /// low / medium / high
        #[arg(long)]
        risk: Option<String>,
    },
    /// 执行一个已批准的计划（按批准的步骤逐一执行，遇偏差即停）
    Run { id: String },
    /// 暂停一个执行中的计划（等外部东西；东西一到用 unblock 恢复）
    Block {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 恢复一个暂停中的计划，回到执行中
    Unblock { id: String },
    /// 把一个执行中或暂停中的计划标记为失败（终态）
    Fail {
        id: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 设置计划服务的持久目标（与任务描述分开存）
    Goal { id: String, goal: String },
    /// 加一个里程碑，或把已有的标为完成/未完成（不加 --done 即未完成）
    Milestone {
        id: String,
        title: String,
        #[arg(long)]
        done: bool,
    },
}

pub(crate) use super::plan_show::print_plan;

pub fn run(home: Option<&Path>, cmd: PlanCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = PlanStore::new(&app.db);
    let now = now();
    let who = if app.config.user.name.trim().is_empty() {
        "cli".into()
    } else {
        app.config.user.name.clone()
    };
    match cmd {
        PlanCmd::New { task, goal, draft } => {
            let router = app.router(app.default_limits())?;
            // Same full registry as the turn loop (see commands::tools):
            // the planner may name MCP / skill tools, and validation
            // agrees with what the executor can resolve.
            let set = super::tools::tool_registry(&app, None)?;
            let (steps, model) = request_plan(&router, &set.tools, set.workspace.as_ref(), &task)
                .context("制定计划失败")?;
            app.record_usage(&router, &now)?;
            let plan = store.create(&task, &goal, &model, steps, draft, &now)?;
            print_plan(&plan);
            if draft {
                println!(
                    "已保存为草稿（`hatch plan approve {}` 前需先编辑确认）。",
                    plan.id
                );
            } else {
                let entry = pending_plan_approval(&app.db, &plan.id)?;
                println!(
                    "计划待批准：`hatch plan approve {}`，或 `hatch plan reject {}`。审批：{}。",
                    plan.id,
                    plan.id,
                    entry.map(|e| e.id).unwrap_or_else(|| "—".into())
                );
            }
        }
        PlanCmd::List { status } => {
            let st = status
                .as_deref()
                .map(PlanStatus::parse)
                .transpose()
                .map_err(|e| anyhow!(e.to_string()))?;
            let plans = store.list(st, 20)?;
            if plans.is_empty() {
                println!("没有计划。");
                return Ok(());
            }
            for p in plans {
                println!(
                    "{}  [{}]  {} 步  {}",
                    p.id,
                    p.status.as_str(),
                    p.steps,
                    p.task
                );
                println!("    创建 {}", p.created_at);
            }
        }
        PlanCmd::Show { id } => {
            let plan = store.get(&id)?.ok_or_else(|| anyhow!("plan {id} 不存在"))?;
            print_plan(&plan);
            if let Some(entry) = pending_plan_approval(&app.db, &id)? {
                println!(
                    "待处理审批：{}（`hatch approvals show {}`）",
                    entry.id, entry.id
                );
            }
        }
        PlanCmd::Approve { id, note } => {
            // The plan flips to `approved` only through the approval
            // queue: decide this plan's pending `plan_approve` entry,
            // then execute the decision (the executor applies it). No
            // pending entry → refuse, naming the plan state.
            let plan = store.get(&id)?.ok_or_else(|| anyhow!("plan {id} 不存在"))?;
            let entry = pending_plan_approval(&app.db, &id)?.ok_or_else(|| {
                anyhow!(
                    "plan {id} 没有待处理的审批（当前状态：{}）。{}",
                    plan.status.as_str(),
                    match plan.status {
                        PlanStatus::Approved => "已批准，直接 `hatch plan run` 执行。",
                        PlanStatus::Draft => "草稿不能直接批准：先编辑，编辑后进入待审批。",
                        _ => "请先 `hatch plan show` 查看状态。",
                    }
                )
            })?;
            ApprovalQueue::new(&app.db)
                .decide(&entry.id, true, &note, &now)
                .map_err(|e| anyhow!(e.to_string()))?;
            let executors = app.executors(false)?;
            match execute_approved(&app.db, &executors, &entry.id, &now)? {
                ExecOutcome::Executed(r) => {
                    println!("{r}。执行：`hatch plan run {id}`。")
                }
                ExecOutcome::AlreadyDone(r) => println!("此前已批准过，未重复：{r}"),
                ExecOutcome::Ambiguous(r) => println!("未执行：{r}"),
                ExecOutcome::Failed(r) => println!("批准执行失败：{r}"),
            }
        }
        PlanCmd::Reject { id, note } => {
            // Decide the pending queue entry first, so a later decision
            // cannot approve a plan the user just rejected.
            if let Some(entry) = pending_plan_approval(&app.db, &id)? {
                ApprovalQueue::new(&app.db)
                    .decide(&entry.id, false, &note, &now)
                    .map_err(|e| anyhow!(e.to_string()))?;
            }
            let plan = store.reject(&id, &who, &note, &now)?;
            println!("计划 {} 已拒绝。", plan.id);
        }
        PlanCmd::Cancel { id, note } => {
            let plan = store.cancel(&id, &who, &note, &now)?;
            println!("计划 {} 已取消。", plan.id);
        }
        PlanCmd::Edit {
            id,
            step,
            tool,
            args,
            files,
            title,
            expected,
            risk,
        } => {
            let plan = store.get(&id)?.ok_or_else(|| anyhow!("plan {id} 不存在"))?;
            let current = plan
                .steps
                .iter()
                .find(|s| s.id == step)
                .ok_or_else(|| anyhow!("plan {id} 中没有步骤 {step}"))?;
            let mut edited = current.clone();
            if let Some(t) = &tool {
                edited.tool = t.clone();
            }
            if let Some(a) = &args {
                edited.arguments = serde_json::from_str(a).context("`--args` 必须是 JSON 对象")?;
            }
            if let Some(f) = &files {
                edited.files = f
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect();
            }
            if let Some(t) = &title {
                edited.title = t.clone();
            }
            if let Some(e) = &expected {
                edited.expected = e.clone();
            }
            if let Some(r) = &risk {
                edited.risk = RiskLevel::parse(r).map_err(|e| anyhow!(e.to_string()))?;
            }
            let set = super::tools::tool_registry(&app, None)?;
            hatch_plan::validate_step(&edited, &set.tools, set.workspace.as_ref())
                .map_err(|e| anyhow!(e.to_string()))?;
            let edit = StepEdit {
                tool,
                arguments: args
                    .map(|a| serde_json::from_str(&a))
                    .transpose()
                    .context("`--args` 必须是 JSON 对象")?,
                files: files.map(|f| {
                    f.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                }),
                title,
                expected,
                risk: risk
                    .map(|r| RiskLevel::parse(&r))
                    .transpose()
                    .map_err(|e| anyhow!(e.to_string()))?,
            };
            let plan = store.edit_step(&id, &step, edit, &now)?;
            println!(
                "步骤 {step} 已修改。计划 {} 回到待审批（改动需要重新批准）。",
                plan.id
            );
        }
        PlanCmd::Run { id } => {
            let set = super::tools::tool_registry(&app, None)?;
            let executors = app.executors(false)?;
            let gate = app.policy()?;
            let deps = RunDeps {
                db: &app.db,
                registry: &set.tools,
                executors: &executors,
                gate: &gate,
                tz: app.tz,
                user_email: app.user_email(),
                // D11: high-risk steps snapshot their files first; the
                // workspace is the same root the file tools use.
                workspace_root: set.workspace.as_ref().map(|w| w.root().to_path_buf()),
                snapshot_root: Some(app.home.join("snapshots")),
                verify: None,
                output_map: None,
                user_name: String::new(),
            };
            let report = run_plan(&deps, &id, &now)?;
            for s in &report.steps {
                println!("  [{}] {}：{}", s.status.as_str(), s.step_id, s.output);
            }
            if !report.snapshots.is_empty() {
                println!("  快照 {} 张（高风险步骤前）：{}", report.snapshots.len(), report.snapshots.join("、"));
            }
            match report.outcome {
                hatch_plan::RunOutcome::Completed => println!("计划 {id} 全部完成。"),
                hatch_plan::RunOutcome::PausedAwaitingApproval => {
                    println!(
                        "计划 {id} 已暂停：有步骤在等审批。批准后用 `hatch plan run {id}` 继续。"
                    )
                }
                hatch_plan::RunOutcome::StoppedOnFailure => {
                    println!(
                        "计划 {id} 在某步失败后停下，之后可用 `hatch plan run {id}` 从断点继续。"
                    )
                }
                hatch_plan::RunOutcome::StoppedOnDeviation => {
                    println!(
                        "计划 {id} 因偏离批准内容而停下：{}。检查后重新批准。",
                        report.deviation.unwrap_or_default()
                    )
                }
            }
        }
        PlanCmd::Block { id, note } => {
            let plan = store.block(&id, &who, &note, &now)?;
            println!("计划 {} 已暂停（{}）。", plan.id, plan.decision_note);
        }
        PlanCmd::Unblock { id } => {
            let plan = store.unblock(&id, &now)?;
            println!("计划 {} 已恢复执行。", plan.id);
        }
        PlanCmd::Fail { id, note } => {
            let plan = store.fail(&id, &who, &note, &now)?;
            println!("计划 {} 已标记失败（{}）。", plan.id, plan.decision_note);
        }
        PlanCmd::Goal { id, goal } => {
            let plan = store.set_goal(&id, &goal, &now)?;
            println!("计划 {} 的目标已设为：{}。", plan.id, plan.goal);
        }
        PlanCmd::Milestone { id, title, done } => {
            let plan = store.upsert_milestone(&id, &title, done, &now)?;
            let m = plan.milestones.iter().find(|m| m.title == title).unwrap();
            println!(
                "里程碑“{}”已{}。",
                m.title,
                if m.done { "完成" } else { "记下" }
            );
        }
    }
    Ok(())
}
