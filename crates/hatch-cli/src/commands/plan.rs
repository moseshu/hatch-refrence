//! `hatch plan` — plan mode from the command line.
//!
//! `new` asks the model for a structured plan and leaves it pending
//! approval; `approve` / `reject` / `cancel` record the user's decision
//! (with timestamp) on the plan; `edit` changes a step and sends the plan
//! back to pending approval; `run` executes an approved plan step by step
//! through the normal dispatch gate, stopping on any deviation.
//!
//! The registry here covers the built-in tools plus file / shell / web /
//! browser tools. MCP and skill tools are not offered to the planner yet
//! (see docs/ARCHITECTURE.md).

use crate::app::App;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use hatch_core::time::now;
use hatch_plan::{request_plan, run_plan, PlanStatus, PlanStore, RiskLevel, RunDeps, StepEdit};
use hatch_tools::builtin::browser::add_browser_tools;
use hatch_tools::builtin::{add_general_tools, default_registry};
use hatch_tools::{Registry, Workspace};
use std::path::Path;

#[derive(Subcommand)]
pub enum PlanCmd {
    /// 让模型为一项任务制定结构化计划（只计划，不执行，等待批准）
    New {
        /// 任务描述（计划会记录这句原话）
        task: String,
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
}

fn registry(app: &App) -> Result<(Registry, Option<Workspace>)> {
    let general = app.general_tools(None)?;
    let mut reg = add_general_tools(default_registry(), &general);
    if let Some(b) = app.browser() {
        reg = add_browser_tools(reg, b);
    }
    // Connector tools are offered to the planner too (Phase 2): a plan step
    // may name `connector_<name>_<op>`; execution still goes through the
    // approval queue. MCP / skill tools are still not offered here.
    for t in app.connector_tools() {
        reg = reg.with(t);
    }
    // Artifact/widget tools: a plan step may create or share an artifact;
    // both still go through the approval queue at execution time.
    for t in app.artifact_tools() {
        reg = reg.with(t);
    }
    Ok((reg, general.workspace))
}

pub(crate) fn print_plan(plan: &hatch_plan::Plan) {
    println!("计划 {}  [{}]", plan.id, plan.status.as_str());
    println!("任务：{}", plan.task);
    if !plan.model.is_empty() {
        println!("模型：{}", plan.model);
    }
    println!("创建：{}  更新：{}", plan.created_at, plan.updated_at);
    if let Some(d) = &plan.decided_at {
        println!(
            "决定：{} 由 {} 决定（{}）",
            d, plan.decided_by, plan.decision_note
        );
    }
    if !plan.deviation.is_empty() {
        println!("偏差：{}", plan.deviation);
    }
    for (i, s) in plan.steps.iter().enumerate() {
        println!(
            "  {}. [{}] {} — {} {}（风险 {}；预期：{}）",
            i + 1,
            s.status.as_str(),
            s.id,
            s.title,
            s.tool,
            s.risk.as_str(),
            s.expected
        );
        println!("     参数：{}", s.arguments);
        if !s.files.is_empty() {
            println!("     文件：{}", s.files.join(", "));
        }
        if !s.output.is_empty() {
            println!("     结果：{}", s.output);
        }
        if let Some(a) = &s.approval_id {
            println!("     等待审批：{a}");
        }
    }
}

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
        PlanCmd::New { task, draft } => {
            let router = app.router(app.default_limits())?;
            let (reg, ws) = registry(&app)?;
            let (steps, model) =
                request_plan(&router, &reg, ws.as_ref(), &task).context("制定计划失败")?;
            app.record_usage(&router, &now)?;
            let plan = store.create(&task, &model, steps, draft, &now)?;
            print_plan(&plan);
            if draft {
                println!(
                    "已保存为草稿（`hatch plan approve {}` 前需先编辑确认）。",
                    plan.id
                );
            } else {
                println!(
                    "计划待批准：`hatch plan approve {}`，或 `hatch plan reject {}`。",
                    plan.id, plan.id
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
        }
        PlanCmd::Approve { id, note } => {
            let plan = store.approve(&id, &who, &note, &now)?;
            println!(
                "计划 {} 已批准（{}）。执行：`hatch plan run {}`。",
                plan.id,
                plan.decided_at.as_deref().unwrap_or(""),
                plan.id
            );
        }
        PlanCmd::Reject { id, note } => {
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
            let (reg, ws) = registry(&app)?;
            hatch_plan::validate_step(&edited, &reg, ws.as_ref())
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
            let (reg, _ws) = registry(&app)?;
            let executors = app.executors(false)?;
            let gate = app.policy()?;
            let deps = RunDeps {
                db: &app.db,
                registry: &reg,
                executors: &executors,
                gate: &gate,
                tz: app.tz,
                user_email: app.user_email(),
            };
            let report = run_plan(&deps, &id, &now)?;
            for s in &report.steps {
                println!("  [{}] {}：{}", s.status.as_str(), s.step_id, s.output);
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
    }
    Ok(())
}
