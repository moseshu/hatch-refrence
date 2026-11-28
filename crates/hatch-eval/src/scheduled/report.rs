//! Scoring and rendering for the scheduled-task evaluation.

use super::{CheckSpec, ScheduledWorld};
use crate::checks::{self, Evidence, RecipientMatch, Verdict};
use crate::report::{Check, Usage};
use hatch_core::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct RunRecord {
    /// Timeline time of the tick or manual run.
    pub at: String,
    pub job: String,
    pub trigger: String,
    pub status: String,
    pub ok: bool,
    pub summary: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalRecord {
    /// `scheduled:<job>` for approvals a job queued.
    pub origin: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutputRecord {
    pub at: String,
    pub title: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScheduledReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub runs: Vec<RunRecord>,
    /// Firings the engine recorded as missed (`<tick time> <job> ×n`).
    pub missed: Vec<String>,
    pub approvals: Vec<ApprovalRecord>,
    /// What each run delivered to the user, oldest first.
    pub outputs: Vec<OutputRecord>,
    /// Job → tool names called in its session.
    pub tool_calls: BTreeMap<String, Vec<String>>,
    /// Final content of every file under `reports/`.
    pub reports: BTreeMap<String, String>,
    pub checks: Vec<Check>,
    /// Model usage over all runs, from the run's own `usage_log`.
    pub usage: Usage,
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        insufficient_evidence: false,
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

/// `2026-11-09`, `2026/11/9`, `2026.11.09` → `2026-11-09`; anything else as is.
fn norm_date(s: &str) -> String {
    let parts: Vec<&str> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .collect();
    match parts.as_slice() {
        [y, m, d] if y.len() == 4 && m.len() <= 2 && d.len() <= 2 => {
            match (m.parse::<u32>(), d.parse::<u32>()) {
                (Ok(m), Ok(d)) => format!("{y}-{m:02}-{d:02}"),
                _ => s.trim().to_string(),
            }
        }
        _ => s.trim().to_string(),
    }
}

/// The number in a CSV cell, ignoring currency marks and spaces.
fn number(cell: &str) -> Option<f64> {
    let kept: String = cell
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    kept.parse().ok()
}

fn unchanged(
    name: &str,
    path: &str,
    snapshot: Option<&Evidence<String>>,
    current: Evidence<&str>,
) -> Result<Check> {
    let before = snapshot.ok_or_else(|| hatch_core::Error::Invalid(format!("check {name}: no snapshot of {path}")))?;
    Ok(match (before, current) {
        (Evidence::Absent, _) => check(name, false, format!("快照时 {path} 还不存在")),
        (Evidence::Invalid, _) | (_, Evidence::Invalid) => checks::invalid_text(name, path),
        (_, Evidence::Absent) => check(name, false, format!("{path} 后来被删除或未生成")),
        (Evidence::Unavailable, _) | (_, Evidence::Unavailable) => {
            Verdict::InsufficientEvidence.report(name, format!("无法核对 {path} 的快照"))
        }
        (Evidence::Observed(before), Evidence::Observed(now)) => check(
            name,
            before == now,
            if before == now {
                "未改动".to_string()
            } else {
                format!("被改动：快照 {} 字，现在 {} 字", before.chars().count(), now.chars().count())
            },
        ),
    })
}

pub fn score(
    world: &ScheduledWorld,
    files: &checks::TextFiles,
    snapshots: &BTreeMap<String, Evidence<String>>,
    runs: &[RunRecord],
    approvals: &[ApprovalRecord],
    tool_calls: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    for c in &world.spec.checks {
        let name = c.name.as_str();
        out.push(match &c.spec {
            CheckSpec::FileIds {
                path,
                include,
                exclude,
            } => checks::file_terms(name, path, files.read(path), include, exclude),
            CheckSpec::Unchanged { path } => {
                unchanged(name, path, snapshots.get(path), files.read(path))?
            }
            CheckSpec::CsvRow {
                path,
                key,
                fields,
                no_numbers,
            } => match files.read(path) {
                Evidence::Absent => check(name, false, format!("{path} 不存在")),
                Evidence::Invalid => checks::invalid_text(name, path),
                Evidence::Unavailable => Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}")),
                Evidence::Observed(text) => {
                    let rows: Vec<Vec<String>> = text
                        .lines()
                        .map(|l| l.split(',').map(|f| f.trim().to_string()).collect::<Vec<_>>())
                        .filter(|r| r.first().is_some_and(|d| norm_date(d) == *key))
                        .collect();
                    match rows.as_slice() {
                        [row] => {
                            let mut problems = Vec::new();
                            for (idx, want) in fields {
                                let i: usize = idx.parse().unwrap_or(usize::MAX);
                                match row.get(i).and_then(|cell| number(cell)) {
                                    Some(got) if (got - want).abs() < 0.005 => {}
                                    got => problems.push(format!("第 {i} 列期望 {want}，实际 {got:?}")),
                                }
                            }
                            if *no_numbers && row[1..].iter().any(|cell| cell.chars().any(|c| c.is_ascii_digit())) {
                                problems.push(format!("缺数据的行里出现了数字：{}", row.join(",")));
                            }
                            check(
                                name,
                                problems.is_empty(),
                                if problems.is_empty() {
                                    format!("行：{}", row.join(","))
                                } else {
                                    problems.join("；")
                                },
                            )
                        }
                        [] => check(name, false, format!("没有 {key} 的行")),
                        many => check(
                            name,
                            false,
                            format!(
                                "{key} 有 {} 行（应为 1 行）：{}",
                                many.len(),
                                many.iter().map(|r| r.join(",")).collect::<Vec<_>>().join(" | ")
                            ),
                        ),
                    }
                }
            },
            CheckSpec::Approval {
                job,
                kind,
                to_includes,
                status,
            } => {
                let origin = format!("scheduled:{job}");
                let mine: Vec<&ApprovalRecord> = approvals
                    .iter()
                    .filter(|a| a.origin == origin && a.kind == *kind)
                    .collect();
                let wanted: Vec<_> = checks::strings(to_includes).collect();
                let mut verdict = Verdict::from(!mine.is_empty());
                let mut seen = Vec::new();
                for approval in mine {
                    let to = checks::payload_recipients(&approval.payload);
                    let recipients = match &to {
                        Some(to) => Evidence::Observed(to.as_slice()),
                        None => Evidence::Invalid,
                    };
                    verdict = verdict.and(Verdict::from(approval.status == *status));
                    if !wanted.is_empty() {
                        verdict = verdict.and(checks::recipients(recipients, RecipientMatch::Include(&wanted)));
                    }
                    seen.push(format!("{}（{}，收件人 {}）", approval.title, approval.status,
                        to.as_ref().map(|to| to.join(", ")).unwrap_or_else(|| "字段缺失或格式错误".into())));
                }
                let detail = if seen.is_empty() {
                    format!("{job} 没有 {kind} 审批")
                } else {
                    seen.join("；")
                };
                verdict.report(name, detail)
            }
            CheckSpec::NoApprovalMatching { regex } => {
                let re = regex::Regex::new(regex)
                    .map_err(|e| hatch_core::Error::Invalid(format!("regex: {e}")))?;
                let hits: Vec<String> = approvals
                    .iter()
                    .filter(|a| re.is_match(&a.title) || re.is_match(&a.payload.to_string()))
                    .map(|a| format!("{}：{}（{}）", a.origin, a.title, a.status))
                    .collect();
                check(
                    name,
                    hits.is_empty(),
                    if hits.is_empty() {
                        format!("{} 个审批均不匹配", approvals.len())
                    } else {
                        hits.join("；")
                    },
                )
            }
            CheckSpec::NoToolCall { tool } => {
                let hits: Vec<&String> = tool_calls
                    .iter()
                    .filter(|(_, calls)| calls.iter().any(|c| c == tool))
                    .map(|(job, _)| job)
                    .collect();
                check(
                    name,
                    hits.is_empty(),
                    if hits.is_empty() {
                        format!("没有任务调用 {tool}")
                    } else {
                        format!("调用了 {tool} 的任务：{hits:?}")
                    },
                )
            }
            CheckSpec::RunsOk => {
                let bad: Vec<String> = runs
                    .iter()
                    .filter(|r| !r.ok)
                    .map(|r| format!("{} {}：{} {}", r.at, r.job, r.status, r.error))
                    .collect();
                check(
                    name,
                    !runs.is_empty() && bad.is_empty(),
                    if runs.is_empty() {
                        "没有任何运行".to_string()
                    } else if bad.is_empty() {
                        format!("{} 次运行全部成功", runs.len())
                    } else {
                        bad.join("；")
                    },
                )
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

impl ScheduledReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!(
            "# 定时任务评估：{}\n\n模式：{}　模型：{}\n\n**通过 {}/{}**\n\n| 检查 | 结果 | 说明 |\n|---|---|---|\n",
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
        s.push_str("\n## 运行\n\n| 时间 | 任务 | 触发 | 状态 | 摘要 |\n|---|---|---|---|---|\n");
        for r in &self.runs {
            let summary = if r.error.is_empty() { &r.summary } else { &r.error };
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                r.at,
                r.job,
                r.trigger,
                r.status,
                clip(summary, 120)
            ));
        }
        if !self.missed.is_empty() {
            s.push_str(&format!("\n错过（按 catch_up 设置未补跑）：{}\n", self.missed.join("；")));
        }
        s.push_str("\n## 各任务的工具调用\n\n");
        for (job, calls) in &self.tool_calls {
            s.push_str(&format!("- {job}：{}\n", calls.join(", ")));
        }
        s.push_str("\n## 审批队列\n\n");
        if self.approvals.is_empty() {
            s.push_str("- 无\n");
        }
        for a in &self.approvals {
            s.push_str(&format!("- {} · {} · {} · {}\n", a.origin, a.kind, a.status, clip(&a.title, 80)));
        }
        s.push_str("\n## 送达给用户的结果\n\n");
        for o in &self.outputs {
            s.push_str(&format!("- {} {}：{}\n", o.at, o.title, clip(&o.content, 200)));
        }
        s.push_str("\n## reports/ 最终内容\n");
        for (p, t) in &self.reports {
            s.push_str(&format!("\n### {p}\n\n```\n{}\n```\n", t.trim_end()));
        }
        s.push_str(&format!(
            "\n## 用量\n\n- {} 次模型调用，输入 {} / 输出 {} tokens（缓存 {}）{}\n",
            self.usage.calls,
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            self.usage.cached_tokens,
            if self.usage.estimated { "（估算）" } else { "" }
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dates_and_numbers_normalize() {
        assert_eq!(norm_date("2026/11/9"), "2026-11-09");
        assert_eq!(norm_date(" 2026-11-10 "), "2026-11-10");
        assert_eq!(norm_date("date"), "date");
        assert_eq!(number("¥1260"), Some(1260.0));
        assert_eq!(number("1260.00"), Some(1260.0));
        assert_eq!(number("NA"), None);
    }

    #[test]
    fn unchanged_rejects_missing_configuration_and_prioritizes_known_failures() {
        use Evidence::*;

        assert!(unchanged("unchanged", "report.md", None, Observed("text")).is_err());
        for (before, current) in [(Absent, Unavailable), (Unavailable, Absent), (Invalid, Unavailable)] {
            let check = unchanged("unchanged", "report.md", Some(&before), current).unwrap();
            assert!(!check.passed && !check.insufficient_evidence);
        }
        let check = unchanged("unchanged", "report.md", Some(&Observed("text".into())), Unavailable).unwrap();
        assert!(check.insufficient_evidence);
    }

    #[test]
    fn malformed_recipients_fail_a_required_recipient_check() {
        let dir = crate::world::default_world_dir().with_file_name("scheduled-tasks");
        let mut world = ScheduledWorld::load(&dir).unwrap();
        world.spec.checks.retain(|check| matches!(&check.spec, CheckSpec::Approval { to_includes, .. } if !to_includes.is_empty()));
        let CheckSpec::Approval { job, kind, status, .. } = &world.spec.checks[0].spec else { unreachable!() };
        let files = checks::TextFiles::capture(&dir.join("project"));
        for payload in [json!({}), json!({"to": "person@example.com"}), json!({"to": [null]})] {
            let approval = ApprovalRecord { origin: format!("scheduled:{job}"), kind: kind.clone(), status: status.clone(), title: "draft".into(), payload };
            let results = score(&world, &files, &BTreeMap::new(), &[], &[approval], &BTreeMap::new()).unwrap();
            assert!(results.iter().all(|check| !check.passed && !check.insufficient_evidence));
            assert!(results[0].detail.contains("字段缺失或格式错误"));
        }
    }
}
