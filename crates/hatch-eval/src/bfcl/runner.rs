//! BFCL agent 级 Phase A runner：每题建一个 agent 跑一轮，写出结果文件。
//!
//! 流程（每题）：
//! 1. 全新内存 DB + 全新 session（题目间隔离）；
//! 2. Registry 只注册该题的函数桩；策略把桩的类压到 Draft，调用先进审批队列；
//! 3. `run_turn` 跑一轮，decide() 自动批准并执行全部桩调用，再把审批结果
//!    喂回一轮（最多一轮跟进）；
//! 4. 从 transcript 取第一个含 tool_calls 的 assistant 行——那就是单轮要的
//!    一次模型响应；无调用时结果记为回答文本。

use super::dataset::{
    is_supported_category, load_category, load_ids_file, BfclEntry, NameMap, SUPPORTED_CATEGORIES,
};
use super::results::{write_category_results, BfclRow};
use super::stub::{BfclStub, BfclStubExecutor};
use hatch_agent::{Agent, AgentConfig, SessionStore, TranscriptRow};
use hatch_core::{Error, Result};
use hatch_llm::{Router, ToolCall, Workload};
use hatch_policy::{
    ActionClass, Approval, ApprovalQueue, ApprovalStatus, Autonomy, Gatekeeper, PolicyConfig,
};
use hatch_store::Db;
use hatch_tools::{execute_approved, ExecOutcome, ExecutorRegistry, Registry};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// `hatch eval --bfcl-*` 的选项（由 CLI 透传；模型与上游全走配置，不写死）。
#[derive(Clone, Debug, Default)]
pub struct BfclOpts {
    /// 数据目录（包含 `BFCL_v4_<类别>.json`）；None 表示不跑 BFCL。
    pub data_dir: Option<PathBuf>,
    /// 类别；空 = 默认三个 Phase A 类别。
    pub categories: Vec<String>,
    /// 每类别最多题数；None = 全量。
    pub limit: Option<usize>,
    /// test_case_ids_to_generate.json（`{类别: [id, ...]}`）；None = 不过滤。
    pub ids_file: Option<PathBuf>,
    /// 结果输出目录；None = `result/hatch-agent-<模型名>`。
    pub out_dir: Option<PathBuf>,
}

/// 单个类别的汇总。
#[derive(Clone, Debug, Default)]
pub struct CategorySummary {
    pub category: String,
    pub total: usize,
    pub with_calls: usize,
    pub no_calls: usize,
    pub errors: usize,
    pub skipped: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub seconds: f64,
    pub out_path: Option<PathBuf>,
}

/// 整次运行的汇总（系统名 = agent + 模型，模型名取自 router 配置）。
#[derive(Clone, Debug, Default)]
pub struct BfclSummary {
    pub system_name: String,
    pub categories: Vec<CategorySummary>,
}

fn sanitize_model_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn model_name(router: &Router) -> String {
    router
        .chain(Workload::Main)
        .first()
        .map(|m| m.name())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn run_bfcl_eval(opts: &BfclOpts, router: &Router) -> Result<BfclSummary> {
    let data_dir = opts
        .data_dir
        .clone()
        .ok_or_else(|| Error::Invalid("BFCL 数据目录未指定（--bfcl-data）".to_string()))?;
    let categories: Vec<String> = if opts.categories.is_empty() {
        SUPPORTED_CATEGORIES.iter().map(|s| s.to_string()).collect()
    } else {
        for c in &opts.categories {
            if !is_supported_category(c) {
                return Err(Error::Invalid(format!(
                    "不支持的 BFCL 类别：{c}（Phase A 只支持 {}）",
                    SUPPORTED_CATEGORIES.join("、")
                )));
            }
        }
        opts.categories.clone()
    };
    let ids: Option<HashMap<String, Vec<String>>> = opts
        .ids_file
        .as_ref()
        .map(|p| load_ids_file(p))
        .transpose()?;

    let system_name = format!("hatch-agent-{}", sanitize_model_name(&model_name(router)));
    let out_dir = opts
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("result").join(&system_name));

    let mut summary = BfclSummary {
        system_name: system_name.clone(),
        categories: vec![],
    };
    for category in &categories {
        let wanted: Option<HashSet<String>> = ids
            .as_ref()
            .and_then(|m| m.get(category).map(|v| v.iter().cloned().collect()));
        if ids.is_some() && wanted.is_none() {
            println!("[{category}] ids 文件里没有该类别，跳过。");
            continue;
        }
        let entries = load_category(&data_dir, category, wanted.as_ref(), opts.limit)?;
        println!("[{category}] 读到 {} 题。", entries.len());
        let cat = run_category(category, &entries, router, &out_dir)?;
        println!(
            "[{category}] 完成：{} 题（调工具 {}，无调用 {}，出错 {}，跳过 {}），{} prompt + {} completion tokens，{:.1}s → {}",
            cat.total,
            cat.with_calls,
            cat.no_calls,
            cat.errors,
            cat.skipped,
            cat.prompt_tokens,
            cat.completion_tokens,
            cat.seconds,
            cat.out_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        );
        summary.categories.push(cat);
    }
    println!(
        "BFCL agent 级 Phase A 完成，系统名 {system_name}，结果在 {}。",
        out_dir.display()
    );
    Ok(summary)
}

fn run_category(
    category: &str,
    entries: &[BfclEntry],
    router: &Router,
    out_dir: &Path,
) -> Result<CategorySummary> {
    let mut summary = CategorySummary {
        category: category.to_string(),
        ..Default::default()
    };
    let mut rows: Vec<BfclRow> = Vec::new();
    let started = Instant::now();
    for entry in entries {
        summary.total += 1;
        match run_task(entry, router) {
            Ok(Some(row)) => {
                if row.result.is_array() {
                    summary.with_calls += 1;
                } else {
                    summary.no_calls += 1;
                }
                summary.prompt_tokens += row.input_tokens;
                summary.completion_tokens += row.output_tokens;
                rows.push(row);
            }
            Ok(None) => summary.skipped += 1,
            Err(e) => {
                summary.errors += 1;
                eprintln!("[{category}] {} 出错：{e}", entry.id);
                rows.push(BfclRow {
                    id: entry.id.clone(),
                    result: Value::String(format!("ERROR: {e}")),
                    input_tokens: 0,
                    output_tokens: 0,
                    latency_secs: 0.0,
                });
            }
        }
    }
    summary.seconds = started.elapsed().as_secs_f64();
    summary.out_path = Some(write_category_results(out_dir, category, &rows)?);
    Ok(summary)
}

/// 跑一道题。Ok(None) = 跳过（函数名下划线化后撞名）；Err = 本题出错。
fn run_task(entry: &BfclEntry, router: &Router) -> Result<Option<BfclRow>> {
    let name_map = match NameMap::build(&entry.functions) {
        Ok(m) => m,
        Err(e) => {
            println!("{}：跳过（{}）", entry.id, e);
            return Ok(None);
        }
    };
    let db = Db::open_in_memory()?;
    let mut reg = Registry::new();
    for f in &entry.functions {
        reg.register(Arc::new(BfclStub::new(f)));
    }
    let exec = ExecutorRegistry::new().with(Arc::new(BfclStubExecutor));
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::ReadPersonal, Autonomy::Draft);
    let gate = Gatekeeper::new(policy);
    let agent = Agent {
        db: &db,
        router,
        tools: &reg,
        executors: &exec,
        gate: &gate,
        config: AgentConfig {
            user_name: "bfcl-eval".to_string(),
            max_steps: 3,
            ..Default::default()
        },
    };
    let sessions = SessionStore::new(&db);
    let now = hatch_core::time::now();
    let sid = sessions.create(&format!("bfcl {}", entry.id), &now)?;
    let origin = format!("chat:{sid}");

    let started = Instant::now();
    let mut input = entry.question.clone();
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut answer = String::new();
    for _round in 0..=1 {
        let r = agent.run_turn(&sid, &input, &hatch_core::time::now())?;
        prompt_tokens += r.usage.prompt_tokens;
        completion_tokens += r.usage.completion_tokens;
        if answer.is_empty() {
            answer = r.answer.clone().unwrap_or_default();
        }
        match decide(&db, &exec, &origin)? {
            Some(msg) => input = msg,
            None => break,
        }
    }
    let latency = started.elapsed().as_secs_f64();

    let calls = first_tool_calls(&sessions.transcript(&sid)?);
    let (result, calls_desc) = match &calls {
        Some(calls) => {
            let list: Vec<Value> = calls
                .iter()
                .map(|c| {
                    let dotted = name_map.restore(&c.name);
                    let args =
                        serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".to_string());
                    json!({ dotted: args })
                })
                .collect();
            let desc = calls
                .iter()
                .map(|c| name_map.restore(&c.name).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            (Value::Array(list), desc)
        }
        None => (Value::String(answer.clone()), "无调用".to_string()),
    };
    println!(
        "{}：{}（{} prompt + {} completion tokens，{:.1}s）",
        entry.id, calls_desc, prompt_tokens, completion_tokens, latency
    );
    Ok(Some(BfclRow {
        id: entry.id.clone(),
        result,
        input_tokens: prompt_tokens,
        output_tokens: completion_tokens,
        latency_secs: latency,
    }))
}

/// transcript 里第一个含 tool_calls 的 assistant 行。
/// TurnReport.tool_calls 只有名字，参数必须从这里取。
fn first_tool_calls(rows: &[TranscriptRow]) -> Option<Vec<ToolCall>> {
    rows.iter()
        .find(|r| r.role == "assistant" && !r.tool_calls.is_empty())
        .map(|r| r.tool_calls.clone())
}

/// 批准该 origin 下全部待审批（桩调用无副作用），执行后把结果喂回 agent。
/// 与 workspace harness 的 decide() 同模式，只是这里没有 allow-list：
/// 桩调用全部放行。
fn decide(db: &Db, exec: &ExecutorRegistry, origin: &str) -> Result<Option<String>> {
    let q = ApprovalQueue::new(db);
    let pending: Vec<Approval> = q
        .list(Some(ApprovalStatus::Pending))?
        .into_iter()
        .filter(|a| a.origin == origin)
        .collect();
    if pending.is_empty() {
        return Ok(None);
    }
    let now = hatch_core::time::now();
    let mut lines = Vec::new();
    for a in pending {
        q.decide(&a.id, true, "评估代理：无副作用的桩调用，自动放行", &now)?;
        let outcome = execute_approved(db, exec, &a.id, &now)?;
        let text = match outcome {
            ExecOutcome::Executed(r) => format!("已执行：{r}"),
            ExecOutcome::AlreadyDone(r) => format!("此前已执行：{r}"),
            ExecOutcome::Ambiguous(e) => format!("执行结果不明：{e}"),
            ExecOutcome::Failed(e) => format!("执行失败：{e}"),
        };
        lines.push(format!("- {}：{}", a.title, text));
    }
    Ok(Some(format!("审批结果：\n{}\n请继续。", lines.join("\n"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_model_name_replaces_unsafe_chars() {
        assert_eq!(sanitize_model_name("my-model-v2"), "my-model-v2");
        assert_eq!(sanitize_model_name("a/b:c d"), "a_b_c_d");
    }
}
