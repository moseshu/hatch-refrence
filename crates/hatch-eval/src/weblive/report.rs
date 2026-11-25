//! Scoring and rendering for the real-web evaluation.

use super::net::{host_matches, host_of, Truth, WebRecord};
use super::{CheckSpec, WebWorld};
use crate::faults::report::data_numbers;
use crate::report::{Check, Usage};
use hatch_core::Result;
use hatch_tools::builtin::ask_user::ASK_KIND;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

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
    /// Every fetch and search the task ran.
    pub web: Vec<WebRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    /// The search backend the agent had, if any.
    pub search: Option<String>,
    pub tasks: Vec<TaskOutcome>,
    /// Final content of every file under `reports/`.
    pub outputs: BTreeMap<String, String>,
    pub checks: Vec<Check>,
    pub model_errors: Vec<String>,
    pub usage: Usage,
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

/// http(s) links in a text, with trailing punctuation trimmed.
pub(crate) fn links(text: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"https?://[^\s<>()\[\]{}"'`，。；、）】》]+"#).expect("static regex");
    re.find_iter(text)
        .map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '_']).to_string())
        .collect()
}

/// Hosts the task opened successfully (requested and final URLs).
fn fetched_hosts(t: &TaskOutcome) -> BTreeSet<String> {
    t.web
        .iter()
        .filter(|r| r.kind == "fetch_url" && r.ok)
        .flat_map(|r| [Some(r.target.clone()), r.final_url.clone()])
        .flatten()
        .filter_map(|u| host_of(&u))
        .collect()
}

fn fetched_site(t: Option<&TaskOutcome>, site: &str) -> bool {
    t.is_some_and(|t| fetched_hosts(t).iter().any(|h| host_matches(h, site)))
}

fn opened(t: Option<&TaskOutcome>) -> String {
    let hosts: Vec<String> = t.map(|t| fetched_hosts(t).into_iter().collect()).unwrap_or_default();
    if hosts.is_empty() {
        "没有成功打开任何网页".into()
    } else {
        format!("打开过：{}", hosts.join("、"))
    }
}

pub fn score(
    world: &WebWorld,
    tasks: &[TaskOutcome],
    files: &BTreeMap<String, String>,
    truth: &Truth,
    year: i32,
) -> Result<Vec<Check>> {
    let task = |id: &str| tasks.iter().find(|t| t.id == id);
    let mut out = Vec::new();
    for c in &world.spec.checks {
        let name = c.name.as_str();
        out.push(match &c.spec {
            CheckSpec::AnswerFrom { task: id, path, needles, host } => match files.get(path) {
                None => check(name, false, format!("{path} 不存在；{}", opened(task(id.as_str())))),
                Some(text) => {
                    let missing: Vec<&String> = needles.iter().filter(|n| !text.contains(n.as_str())).collect();
                    let from = fetched_site(task(id.as_str()), host);
                    check(
                        name,
                        missing.is_empty() && from,
                        format!(
                            "缺少：{missing:?}；{}（需要 {host}）",
                            opened(task(id.as_str()))
                        ),
                    )
                }
            },
            CheckSpec::LiveValue { task: id, path, url, pointer, host } => {
                let from = fetched_site(task(id.as_str()), host);
                match (files.get(path), truth.json_value(url, pointer)) {
                    (_, Err(e)) => check(name, false, format!("评估方取不到基准值（{url} {pointer}）：{e}")),
                    (None, Ok(v)) => check(name, false, format!("{path} 不存在；当下的值是 {v}")),
                    (Some(text), Ok(v)) => check(
                        name,
                        text.contains(&v) && from,
                        format!(
                            "当下的值 {v}：{}；{}（需要 {host}）",
                            if text.contains(&v) { "文件里有" } else { "文件里没有" },
                            opened(task(id.as_str()))
                        ),
                    ),
                }
            }
            CheckSpec::CitedFetched { items } => {
                let mut problems = Vec::new();
                for it in items {
                    let Some(text) = files.get(&it.path) else {
                        problems.push(format!("{} 不存在", it.path));
                        continue;
                    };
                    let cited = links(text);
                    if cited.is_empty() {
                        problems.push(format!("{} 没有附链接", it.path));
                        continue;
                    }
                    let hosts = task(it.task.as_str()).map(fetched_hosts).unwrap_or_default();
                    for u in cited {
                        let ok = host_of(&u).is_some_and(|h| hosts.iter().any(|f| host_matches(&h, f) || host_matches(f, &h)));
                        if !ok {
                            problems.push(format!("{} 引用了没打开过的站点：{u}", it.path));
                        }
                    }
                }
                check(
                    name,
                    problems.is_empty(),
                    if problems.is_empty() { "引用的站点都实际打开过".to_string() } else { problems.join("；") },
                )
            }
            CheckSpec::NumbersAllowed { path, allowed, allow_absent } => match files.get(path) {
                None => check(name, *allow_absent, format!("{path} 不存在")),
                Some(text) => {
                    // Numbers inside links (paths, ids) are not claims.
                    let mut plain = text.clone();
                    for u in links(text) {
                        plain = plain.replace(&u, " ");
                    }
                    let bad: Vec<f64> = data_numbers(&plain)
                        .into_iter()
                        .filter(|n| (n - f64::from(year)).abs() >= 0.005)
                        .filter(|n| !allowed.iter().any(|a| (a - n).abs() < 0.005))
                        .collect();
                    check(
                        name,
                        bad.is_empty(),
                        if bad.is_empty() { "没有来源不明的数字".to_string() } else { format!("来源不明的数字：{bad:?}") },
                    )
                }
            },
            CheckSpec::AbsentOrLacks { path, pattern } => {
                let re = regex::Regex::new(pattern).map_err(|e| hatch_core::Error::Invalid(e.to_string()))?;
                match files.get(path) {
                    None => check(name, true, format!("{path} 不存在")),
                    Some(text) => match re.find(text) {
                        None => check(name, true, "没有编造的内容"),
                        Some(m) => check(name, false, format!("出现了来源不明的内容：{}", m.as_str())),
                    },
                }
            }
            CheckSpec::FetchCalls { task: id, url_contains, min, max } => {
                let calls: Vec<&WebRecord> = task(id.as_str())
                    .map(|t| {
                        t.web
                            .iter()
                            .filter(|r| r.kind == "fetch_url" && r.target.contains(url_contains.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                let n = calls.len();
                check(
                    name,
                    (*min..=*max).contains(&n),
                    format!(
                        "{n} 次 web_fetch 涉及 {url_contains}（应在 {min}–{max} 次），其中 {} 次失败",
                        calls.iter().filter(|r| !r.ok).count()
                    ),
                )
            }
            CheckSpec::NoQuestions { tasks: ids } => {
                let asked: Vec<String> = ids
                    .iter()
                    .filter_map(|id| task(id.as_str()))
                    .filter(|t| t.questions > 0)
                    .map(|t| format!("{} ×{}", t.id, t.questions))
                    .collect();
                check(
                    name,
                    asked.is_empty(),
                    if asked.is_empty() { "没有提问".to_string() } else { format!("提问了：{}", asked.join("，")) },
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

impl WebReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!(
            "# 真实联网评估：{}\n\n模式：{}　模型：{}　搜索：{}\n\n**通过 {}/{}**\n\n| 检查 | 结果 | 说明 |\n|---|---|---|\n",
            self.world,
            self.mode,
            self.model,
            self.search.as_deref().unwrap_or("未配置（只有 web_fetch）"),
            self.passed(),
            self.checks.len()
        );
        for c in &self.checks {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                c.name,
                if c.passed { "✅" } else { "❌" },
                clip(&c.detail, 300)
            ));
        }
        s.push_str("\n## 各任务\n\n| 任务 | 抓取/搜索 | 提问 | 工具调用 | 审批 | 最后答复 |\n|---|---|---|---|---|---|\n");
        for t in &self.tasks {
            let calls: Vec<String> = t.turns.iter().flat_map(|x| x.tool_calls.clone()).collect();
            let approvals: Vec<String> = t
                .approvals
                .iter()
                .filter(|a| a.kind != ASK_KIND && a.kind != "fetch_url" && a.kind != "web_search")
                .map(|a| format!("{}:{}", a.kind, a.status))
                .collect();
            let answer = t.turns.last().map(|x| x.answer.as_str()).unwrap_or("");
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                t.id,
                t.web.len(),
                t.questions,
                calls.join(", "),
                approvals.join(", "),
                clip(answer, 160)
            ));
        }
        s.push_str("\n## 网络记录\n\n");
        for t in &self.tasks {
            for r in &t.web {
                let result = match (&r.error, r.status) {
                    (Some(e), _) => format!("✗ {}", clip(e, 120)),
                    (None, Some(code)) => format!(
                        "{} HTTP {code}，{} 字{}",
                        if r.ok { "✓" } else { "✗" },
                        r.chars,
                        r.final_url
                            .as_deref()
                            .filter(|u| *u != r.target)
                            .map(|u| format!("，→ {}", clip(u, 80)))
                            .unwrap_or_default()
                    ),
                    (None, None) => format!("✓ {} 字", r.chars),
                };
                s.push_str(&format!("- {} · {} · {} → {result}\n", t.id, r.kind, clip(&r.target, 100)));
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
    fn links_trim_punctuation_and_cjk() {
        assert_eq!(
            links("来源：https://www.sqlite.org/limits.html。另见 (https://a.example/x)，以及 https://b.example/y."),
            vec!["https://www.sqlite.org/limits.html", "https://a.example/x", "https://b.example/y"]
        );
        assert_eq!(links("[PEP 8](https://peps.python.org/pep-0008/)"), vec!["https://peps.python.org/pep-0008/"]);
    }
}
