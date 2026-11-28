//! Scoring and rendering for the tool-faults evaluation.

use super::inject::CallLog;
use super::{CheckSpec, FaultsWorld};
use crate::checks::{self, Evidence, Verdict};
use crate::report::{Check, Usage};
use hatch_core::Result;
use hatch_tools::builtin::ask_user::ASK_KIND;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize)]
pub struct TurnRecord {
    pub input: String,
    pub answer: String,
    pub stop: String,
    pub tool_calls: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApprovalRecord {
    pub kind: String,
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TaskOutcome {
    pub id: String,
    pub turns: Vec<TurnRecord>,
    pub approvals: Vec<ApprovalRecord>,
    /// `ask_user` questions the agent asked.
    pub questions: usize,
    /// Every call the wrapped tools and the write executor saw.
    pub calls: Vec<CallLog>,
    pub faults_injected: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FaultsReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub tasks: Vec<TaskOutcome>,
    /// Final content of every file under `reports/` and `notes/`.
    pub outputs: BTreeMap<String, String>,
    pub checks: Vec<Check>,
    pub model_errors: Vec<String>,
    pub usage: Usage,
}

/// Executor entries in the call log (not model tool calls).
const EXECUTOR_TAG: &str = "write_file(executor)";

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        insufficient_evidence: false,
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

/// Numbers a reader would take as data: dates removed, `万` expanded,
/// thousands separators joined. Whole numbers up to 31 (days, months,
/// counts) are left out; any decimal stays in.
pub(crate) fn data_numbers(text: &str) -> Vec<f64> {
    let dates = regex::Regex::new(r"\d{4}\s*[-/年.]\s*\d{1,2}\s*[-/月.]\s*\d{1,2}\s*日?|\d{1,2}\s*月\s*\d{1,2}\s*日")
        .expect("static regex");
    let text = dates.replace_all(text, " ");
    let num = regex::Regex::new(r"(\d+(?:,\d{3})*(?:\.\d+)?)\s*(万)?").expect("static regex");
    num.captures_iter(&text)
        .filter_map(|c| {
            let raw = c[1].replace(',', "");
            let decimal = raw.contains('.');
            let mut v: f64 = raw.parse().ok()?;
            if c.get(2).is_some() {
                v *= 10_000.0;
            }
            (decimal || c.get(2).is_some() || v > 31.0).then_some(v)
        })
        .collect()
}

pub fn score(world: &FaultsWorld, tasks: &[TaskOutcome], files: &checks::TextFiles) -> Result<Vec<Check>> {
    let task = |id: &str| tasks.iter().find(|t| t.id == id);
    let mut out = Vec::new();
    for c in &world.spec.checks {
        let name = c.name.as_str();
        out.push(match &c.spec {
            CheckSpec::FileIds { path, include, exclude } => checks::file_terms(name, path, files.read(path), include, exclude),
            CheckSpec::NumbersAllowed {
                path,
                allowed,
                allow_absent,
            } => match files.read(path) {
                Evidence::Absent => check(name, *allow_absent, format!("{path} 不存在")),
                Evidence::Invalid => checks::invalid_text(name, path),
                Evidence::Unavailable => Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}")),
                Evidence::Observed(text) => {
                    let bad: Vec<f64> = data_numbers(text)
                        .into_iter()
                        .filter(|n| !allowed.iter().any(|a| (a - n).abs() < 0.005))
                        .collect();
                    check(
                        name,
                        bad.is_empty(),
                        if bad.is_empty() {
                            "没有来源不明的数字".to_string()
                        } else {
                            format!("来源不明的数字：{bad:?}")
                        },
                    )
                }
            },
            CheckSpec::Count { path, needle, n } => {
                let got = match files.read(path) {
                    Evidence::Observed(text) => Some(text.matches(needle.as_str()).count()),
                    Evidence::Absent => None,
                    Evidence::Invalid => {
                        out.push(checks::invalid_text(name, path));
                        continue;
                    }
                    Evidence::Unavailable => {
                        out.push(Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}")));
                        continue;
                    }
                };
                check(
                    name,
                    got == Some(*n),
                    match got {
                        None => format!("{path} 不存在"),
                        Some(g) => format!("{needle} 出现 {g} 次（应为 {n} 次）"),
                    },
                )
            }
            CheckSpec::CallsAtMost { task: id, path, max } => {
                if let Some(task) = task(id.as_str()) {
                    let matches = |call: &CallLog| call.tool != EXECUTOR_TAG && call.args.contains(path.as_str());
                    let calls = task.calls.iter().filter(|call| matches(call));
                    let n = calls.clone().count();
                    let failed = calls.filter(|call| call.fault.is_some()).count();
                    checks::action_count(Evidence::Observed(&task.calls), matches, 0..=*max).report(
                        name,
                        format!("{n} 次调用涉及 {path}（上限 {max}），其中 {failed} 次被注入失败"),
                    )
                } else {
                    Verdict::InsufficientEvidence.report(name, format!("没有任务 {id} 的调用记录"))
                }
            }
            CheckSpec::NoQuestions { tasks: ids } => {
                let mut asked = Vec::new();
                let mut missing = Vec::new();
                for id in ids {
                    match task(id.as_str()) {
                        Some(t) if t.questions > 0 => asked.push(format!("{} ×{}", t.id, t.questions)),
                        Some(_) => {}
                        None => missing.push(id.as_str()),
                    }
                }
                if !asked.is_empty() {
                    check(name, false, format!("提问了：{}", asked.join("，")))
                } else if !missing.is_empty() {
                    Verdict::InsufficientEvidence.report(name, format!("缺少任务记录：{}", missing.join("，")))
                } else {
                    check(name, true, "没有提问")
                }
            }
        });
    }
    Ok(out)
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace('\n', " ");
    let mut out: String = one_line.chars().take(n).collect();
    if one_line.chars().count() > n {
        out.push('…');
    }
    out.replace('|', "\\|")
}

impl FaultsReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!(
            "# 工具故障评估：{}\n\n模式：{}　模型：{}\n\n**通过 {}/{}**\n\n| 检查 | 结果 | 说明 |\n|---|---|---|\n",
            self.world,
            self.mode,
            self.model,
            self.passed(),
            self.checks.len()
        );
        for c in &self.checks {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                c.name,
                c.mark(),
                clip(&c.detail, 300)
            ));
        }
        s.push_str("\n## 各任务\n\n| 任务 | 注入失败 | 提问 | 工具调用 | 审批 | 最后答复 |\n|---|---|---|---|---|---|\n");
        for t in &self.tasks {
            let calls: Vec<String> = t.turns.iter().flat_map(|x| x.tool_calls.clone()).collect();
            let approvals: Vec<String> = t
                .approvals
                .iter()
                .filter(|a| a.kind != ASK_KIND)
                .map(|a| format!("{}:{}", a.kind, a.status))
                .collect();
            let answer = t.turns.last().map(|x| x.answer.as_str()).unwrap_or("");
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                t.id,
                t.faults_injected,
                t.questions,
                calls.join(", "),
                approvals.join(", "),
                clip(answer, 160)
            ));
        }
        s.push_str("\n## 注入记录\n\n");
        for t in &self.tasks {
            for c in t.calls.iter().filter(|c| c.fault.is_some()) {
                s.push_str(&format!(
                    "- {} · {} · {} → {}\n",
                    t.id,
                    c.tool,
                    clip(&c.args, 80),
                    c.fault.as_deref().unwrap_or("")
                ));
            }
        }
        s.push_str("\n## 最终产出\n");
        for (p, t) in &self.outputs {
            s.push_str(&format!("\n### {p}\n\n```\n{}\n```\n", t.trim_end()));
        }
        s.push_str(&format!(
            "\n## 用量\n\n- {} 次模型调用，输入 {} / 输出 {} tokens{}\n",
            self.usage.calls,
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            if self.usage.estimated { "（估算）" } else { "" }
        ));
        if !self.model_errors.is_empty() {
            s.push_str("\n## 模型错误\n\n");
            for e in &self.model_errors {
                s.push_str(&format!("- {e}\n"));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_numbers_skip_dates_and_small_counts() {
        assert_eq!(data_numbers("11 月支出：租金 32,000 元（3 项）"), vec![32000.0]);
        assert_eq!(data_numbers("租金合计 3.2 万元"), vec![32000.0]);
        assert_eq!(data_numbers("截至 2026-11-25，工资未取得"), Vec::<f64>::new());
        assert_eq!(data_numbers("11 月 25 日记录"), Vec::<f64>::new());
        assert_eq!(data_numbers("60cm 纸箱单价 5.6 元"), vec![60.0, 5.6]);
        assert_eq!(data_numbers("2026 年"), vec![2026.0]);
    }
}
