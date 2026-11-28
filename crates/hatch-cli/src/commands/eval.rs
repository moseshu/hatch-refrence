//! `hatch eval`

use crate::app::{App, Limits};
use crate::commands::eval_compaction::run_compaction;
use crate::commands::eval_record::check_failure;
use anyhow::{bail, Result};
use hatch_eval::browser::{run_browser_eval, BrMode, BrowserWorld};
use hatch_eval::caution::{run_caution_eval, CautionWorld, CtMode};
use hatch_eval::compaction::CompactionWorld;
use hatch_eval::delegate::{run_delegate_eval, DelegateWorld, DgMode};
use hatch_eval::faults::{run_faults_eval, FaultsWorld, FtMode};
use hatch_agent::RecallMode;
use hatch_eval::memory::{
    git_state, independence_problems, run_memory_eval, target_warnings, MemMode, MemoryWorld,
    RunManifest,
};
use hatch_eval::scheduled::{run_scheduled_eval, ScheduledWorld, StMode};
use hatch_eval::scheduler::{run_scheduler_eval, SchedMode, SchedulerWorld};
use hatch_eval::weblive::{run_web_eval, WebWorld, WlMode};
use hatch_eval::workspace::{run_workspace_eval, WorkspaceWorld, WsMode};
use hatch_eval::{run_eval, run_eval_repeat, Mode, RepeatMode, World};
use std::path::{Path, PathBuf};

fn world_dir(arg: Option<PathBuf>) -> PathBuf {
    if let Some(p) = arg {
        return p;
    }
    let local = PathBuf::from("eval/worlds/zh-office");
    if local.exists() {
        local
    } else {
        hatch_eval::world::default_world_dir()
    }
}

/// Ten CLI arguments because every `hatch eval` flag is threaded through;
/// grouping them into a struct would churn all four world dispatchers.
#[allow(clippy::too_many_arguments)]
pub fn run(
    home: Option<&Path>,
    world: Option<PathBuf>,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    repeat: u32,
    recall: Option<String>,
    heldout: bool,
) -> Result<()> {
    let dir = world_dir(world);
    let execute = || run_world(
        home, &dir, live, max_calls, max_tokens, reasoning_effort.clone(),
        out.clone(), repeat, recall.clone(), heldout,
    );
    // Older held-out packs keep their original private output format.
    // Independent evaluators can attach metadata for a private scorecard.
    let recorded = !heldout || dir.join(hatch_eval::scorecard::METADATA_FILE).is_file();
    if let Some(output) = out.as_deref().filter(|_| recorded) {
        let fixture_clock = if WebWorld::is_web_world(&dir) {
            Some(WebWorld::load(&dir)?.spec.now)
        } else {
            None
        };
        let settings = serde_json::json!({
            "max_calls": max_calls,
            "max_tokens": max_tokens,
            "reasoning_effort": reasoning_effort.as_deref(),
            "recall": recall.as_deref().unwrap_or("auto"),
            "fixture_clock": fixture_clock,
            "eval_today": std::env::var(hatch_eval::dates::TODAY_ENV).ok(),
        });
        super::eval_record::run_recorded(&dir, output, live, heldout, settings, execute)
    } else {
        execute()
    }
}

#[allow(clippy::too_many_arguments)]
fn run_world(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    repeat: u32,
    recall: Option<String>,
    heldout: bool,
) -> Result<()> {
    if live && dir.join(hatch_eval::variant::PROVENANCE_FILE).is_file() {
        bail!("生成的当前模板变体是离线夹具，请使用 pipeline 模式");
    }
    if repeat == 0 {
        bail!("--repeat 至少为 1");
    }
    if heldout {
        if !MemoryWorld::is_memory_world(dir) {
            bail!("--heldout 目前只适用于记忆世界格式（memory.json + project/）");
        }
        if !live {
            bail!("--heldout 需要 --live：脚本模型只认识仓库里的世界");
        }
        if out.is_none() {
            bail!("--heldout 需要 --out <私有目录>：完整报告和诊断轨迹只写到那里，终端只显示汇总");
        }
    }
    let recall = match recall.as_deref() {
        None => None,
        Some(_) if !MemoryWorld::is_memory_world(dir) => bail!("--recall 只适用于记忆世界（memory-crossday、memory-pressure）"),
        Some("auto") => Some(RecallMode::Auto),
        Some("off") => Some(RecallMode::Off),
        Some("legacy") => Some(RecallMode::LegacyExcludeAllTagged),
        Some(other) => bail!("--recall 取值为 auto / off / legacy，收到 {other}"),
    };
    if repeat > 1
        && (BrowserWorld::is_browser_world(dir)
            || WorkspaceWorld::is_workspace_world(dir)
            || DelegateWorld::is_delegate_world(dir)
            || MemoryWorld::is_memory_world(dir)
            || ScheduledWorld::is_scheduled_world(dir)
            || CautionWorld::is_caution_world(dir)
            || FaultsWorld::is_faults_world(dir)
            || WebWorld::is_web_world(dir)
            || SchedulerWorld::is_scheduler_world(dir)
            || CompactionWorld::is_compaction_world(dir))
    {
        bail!("--repeat 目前只支持邮件分诊世界（zh-office、en-office、thread-attachment 等）；workspace / delegate / memory / scheduled / act-or-ask / tool-faults / web-live / scheduler / browser / compaction 世界请单次运行");
    }
    if CompactionWorld::is_compaction_world(dir) {
        return run_compaction(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if BrowserWorld::is_browser_world(dir) {
        return run_browser(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if WebWorld::is_web_world(dir) {
        return run_web(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if FaultsWorld::is_faults_world(dir) {
        return run_faults(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if CautionWorld::is_caution_world(dir) {
        return run_caution(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if ScheduledWorld::is_scheduled_world(dir) {
        return run_scheduled(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if MemoryWorld::is_memory_world(dir) {
        return run_memory(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
            MemoryRun {
                recall: recall.unwrap_or_default(),
                heldout,
            },
        );
    }
    if WorkspaceWorld::is_workspace_world(dir) {
        return run_workspace(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if DelegateWorld::is_delegate_world(dir) {
        return run_delegate(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    if SchedulerWorld::is_scheduler_world(dir) {
        return run_scheduler(
            home,
            dir,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
        );
    }
    let w = World::load(dir)?;
    if repeat > 1 {
        return run_repeated(
            home,
            &w,
            live,
            max_calls,
            max_tokens,
            reasoning_effort,
            out,
            repeat,
        );
    }
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort: reasoning_effort.clone(),
        }).map(hatch_eval::recording::attach)?;
        eprintln!(
            "live 模式：调用上限 {}，每次 max_tokens={max_tokens}，reasoning_effort={}",
            max_calls
                .map(|n| n.to_string())
                .unwrap_or_else(|| "不限".into()),
            reasoning_effort
                .as_deref()
                .unwrap_or("（未设置，用模型默认）")
        );
        run_eval(
            &w,
            Mode::Live {
                router: &router,
                max_calls,
            },
        )?
    } else {
        run_eval(&w, Mode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(dir.join("report.md"), &md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}

/// Same eight-argument threading as [`run`]; see its note.
#[allow(clippy::too_many_arguments)]
fn run_repeated(
    home: Option<&Path>,
    w: &World,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    repeat: u32,
) -> Result<()> {
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort: reasoning_effort.clone(),
        }).map(hatch_eval::recording::attach)?;
        eprintln!(
            "live 模式重复运行 {} 次：调用上限 {}，每次 max_tokens={max_tokens}",
            repeat,
            max_calls
                .map(|n| n.to_string())
                .unwrap_or_else(|| "不限".into()),
        );
        run_eval_repeat(
            w,
            RepeatMode::Live {
                router: &router,
                max_calls,
            },
            repeat as usize,
        )?
    } else {
        run_eval_repeat(w, RepeatMode::Pipeline, repeat as usize)?
    };
    let md = report.render_markdown();
    println!("{md}");
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        std::fs::write(dir.join("report.md"), &md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    if !report.all_passed() {
        let failed_runs = report
            .reports
            .iter()
            .enumerate()
            .filter(|(_, r)| r.passed() != r.checks.len())
            .map(|(i, r)| format!("第 {} 次：{}/{} 通过", i + 1, r.passed(), r.checks.len()))
            .collect::<Vec<_>>()
            .join("；");
        return Err(check_failure(format!("重复运行中有未通过的检查（{failed_runs}）")));
    }
    Ok(())
}

fn write_report(out: Option<PathBuf>, json: String, md: &str) -> Result<()> {
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("report.json"), json)?;
        std::fs::write(dir.join("report.md"), md)?;
        eprintln!("报告已写入 {}", dir.display());
    }
    Ok(())
}

fn run_workspace(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = WorkspaceWorld::load(dir)?;
    let app = App::open(home)?;
    let sandbox = app.sandbox()?;
    let report = if live {
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        run_workspace_eval(&w, WsMode::Live { router: &router }, sandbox)?
    } else {
        run_workspace_eval(&w, WsMode::Pipeline, sandbox)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}

/// Memory-world options beyond the common `hatch eval` flags.
struct MemoryRun {
    recall: RecallMode,
    /// The world is a private held-out pack: check its independence
    /// first, print only the aggregate summary.
    heldout: bool,
}

/// The repository's `eval/worlds`: the working directory's when run from
/// a checkout, else the one this binary was built from.
fn repo_worlds_dir() -> PathBuf {
    let local = PathBuf::from("eval/worlds");
    if local.is_dir() {
        local
    } else {
        hatch_eval::world::default_world_dir()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(local)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_memory(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
    opts: MemoryRun,
) -> Result<()> {
    let MemoryRun { recall, heldout } = opts;
    let w = MemoryWorld::load(dir)?;
    let fingerprint = hatch_eval::markers::fingerprint(dir);
    // Targets named by position and category only: safe for held-out packs.
    for warning in target_warnings(&w) {
        eprintln!("目标校验：{warning}");
    }
    if heldout {
        let problems = independence_problems(&w, &repo_worlds_dir());
        if !problems.is_empty() {
            bail!(
                "这个目录不能当留出集用（还没有调用模型）：\n- {}",
                problems.join("\n- ")
            );
        }
    }
    let (commit, dirty) = git_state(Path::new("."));
    let started_at = hatch_core::time::to_rfc3339(&hatch_core::time::now());
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        // The nightly extraction builds its own router from the same
        // config, exactly as the daemon's memory-extract job does.
        let extract = crate::sched::router_factory(&app).map(hatch_eval::recording::factory);
        run_memory_eval(
            &w,
            MemMode::Live {
                router: &router,
                extract,
            },
            recall,
        )?
    } else {
        run_memory_eval(&w, MemMode::Pipeline, recall)?
    };
    let md = report.render_markdown();
    if heldout {
        println!("{}", report.render_heldout_summary(&fingerprint));
    } else {
        println!("{md}");
    }
    if let Some(dir) = &out {
        let manifest = RunManifest {
            world: w.spec.name.clone(),
            fixture_fingerprint: fingerprint.clone(),
            heldout,
            mode: report.mode.clone(),
            model: report.model.clone(),
            recall: report.recall.clone(),
            commit,
            dirty,
            hatch_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at,
            finished_at: hatch_core::time::to_rfc3339(&hatch_core::time::now()),
            checks_passed: report.passed(),
            checks_total: report.checks.len(),
            usage: report.usage.clone(),
            extract_usage: report.extract_usage.clone(),
        };
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(&manifest)?)?;
        report.write_diagnostics(dir)?;
        if heldout {
            std::fs::write(dir.join("summary.md"), report.render_heldout_summary(&fingerprint))?;
        }
    }
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}

fn run_delegate(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = DelegateWorld::load(dir)?;
    let app = App::open(home)?;
    let sandbox = app.sandbox()?;
    let report = if live {
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        run_delegate_eval(&w, DgMode::Live { router: &router }, sandbox)?
    } else {
        run_delegate_eval(&w, DgMode::Pipeline, sandbox)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}

fn run_faults(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = FaultsWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        run_faults_eval(&w, FtMode::Live { router: &router })?
    } else {
        run_faults_eval(&w, FtMode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    require_checks(&report.checks)
}

fn run_web(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = WebWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        // The same web stack the agent has in real use: `[web]` fetch
        // limits and, when configured, the SearXNG search backend.
        let search_url = app.config.web.search_url.trim().to_string();
        let search: Option<std::sync::Arc<dyn hatch_tools::web::SearchBackend>> = (!search_url.is_empty())
            .then(|| {
                std::sync::Arc::new(hatch_tools::web::SearxBackend { base_url: search_url })
                    as std::sync::Arc<dyn hatch_tools::web::SearchBackend>
            });
        eprintln!(
            "live 模式：真实网络；搜索 {}",
            search.as_ref().map(|s| s.name()).unwrap_or_else(|| "未配置（只有 web_fetch）".into())
        );
        run_web_eval(
            &w,
            WlMode::Live {
                router: &router,
                search,
                fetch: app.fetch_limits(),
            },
        )?
    } else {
        run_web_eval(&w, WlMode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    require_checks(&report.checks)
}

fn run_caution(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = CautionWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        run_caution_eval(&w, CtMode::Live { router: &router })?
    } else {
        run_caution_eval(&w, CtMode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    require_checks(&report.checks)
}

fn run_scheduled(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = ScheduledWorld::load(dir)?;
    let mode = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        // `--max-calls` caps each job run: every run builds a fresh router,
        // as the daemon does.
        let limits = Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        };
        // Fail early (and readably) if no model is configured.
        let label = app
            .router(limits.clone())?
            .chain(hatch_llm::Workload::Main)
            .iter()
            .map(|m| m.name())
            .collect::<Vec<_>>()
            .join(" → ");
        let (config, home) = (app.config.clone(), app.home.clone());
        let factory: hatch_workers::RouterFactory = std::sync::Arc::new(move || {
            crate::app::build_router(&config, &home, limits.clone())
                .map_err(|e| hatch_core::Error::Config(format!("{e:#}")))
        });
        StMode::Live { factory: hatch_eval::recording::factory(factory), label }
    } else {
        StMode::Pipeline
    };
    let report = run_scheduled_eval(&w, mode)?;
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    require_checks(&report.checks)
}

fn require_checks(checks: &[hatch_eval::Check]) -> Result<()> {
    let unknown = checks.iter().filter(|check| check.insufficient_evidence).count();
    let failed = checks.iter().filter(|check| !check.passed && !check.insufficient_evidence).count();
    match (failed, unknown) {
        (0, 0) => Ok(()),
        (failed, 0) => Err(check_failure(format!("{failed} 项检查未通过"))),
        (0, unknown) => Err(check_failure(format!("{unknown} 项检查证据不足"))),
        (failed, unknown) => Err(check_failure(format!("{failed} 项检查未通过，{unknown} 项证据不足"))),
    }
}

fn run_scheduler(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = SchedulerWorld::load(dir)?;
    let mode = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let limits = Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        };
        // Fail early (and readably) if no model is configured.
        let label = app
            .router(limits.clone())?
            .chain(hatch_llm::Workload::Brief)
            .iter()
            .map(|m| m.name())
            .collect::<Vec<_>>()
            .join(" → ");
        let (config, home) = (app.config.clone(), app.home.clone());
        let factory: hatch_workers::RouterFactory = std::sync::Arc::new(move || {
            crate::app::build_router(&config, &home, limits.clone())
                .map_err(|e| hatch_core::Error::Config(format!("{e:#}")))
        });
        SchedMode::Live { factory: hatch_eval::recording::factory(factory), label }
    } else {
        SchedMode::Pipeline
    };
    let report = run_scheduler_eval(&w, mode)?;
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}

fn run_browser(
    home: Option<&Path>,
    dir: &Path,
    live: bool,
    max_calls: Option<u64>,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let w = BrowserWorld::load(dir)?;
    let report = if live {
        let app = App::open(home)?;
        let reasoning_effort =
            reasoning_effort.or_else(|| app.config.limits.effective_reasoning_effort());
        let router = app.router(Limits {
            max_calls,
            max_tokens: Some(max_tokens),
            reasoning_effort,
        }).map(hatch_eval::recording::attach)?;
        run_browser_eval(&w, BrMode::Live { router: &router })?
    } else {
        run_browser_eval(&w, BrMode::Pipeline)?
    };
    let md = report.render_markdown();
    println!("{md}");
    write_report(out, serde_json::to_string_pretty(&report)?, &md)?;
    let failed = report.checks.len() - report.passed();
    if failed > 0 {
        return Err(check_failure(format!("{failed} 项检查未通过")));
    }
    Ok(())
}
