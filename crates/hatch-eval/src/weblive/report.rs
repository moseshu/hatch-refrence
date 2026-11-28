//! Scoring and rendering for the real-web evaluation.

use super::net::{host_matches, host_of, Truth, WebRecord};
use super::{CheckSpec, WebWorld};
use crate::checks::{self, Evidence, TextMatch, TextScope, Verdict};
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
        insufficient_evidence: false,
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

fn fetched_site(t: Option<&TaskOutcome>, site: &str) -> Verdict {
    match t {
        Some(task) => Verdict::from(fetched_hosts(task).iter().any(|host| host_matches(host, site))),
        None => Verdict::InsufficientEvidence,
    }
}

fn opened(t: Option<&TaskOutcome>) -> String {
    let Some(t) = t else { return "没有任务抓取记录".into() };
    let hosts: Vec<String> = fetched_hosts(t).into_iter().collect();
    if hosts.is_empty() {
        "没有成功打开任何网页".into()
    } else {
        format!("打开过：{}", hosts.join("、"))
    }
}

pub fn score(
    world: &WebWorld,
    tasks: &[TaskOutcome],
    files: &checks::TextFiles,
    truth: &Truth,
    year: i32,
) -> Result<Vec<Check>> {
    let task = |id: &str| tasks.iter().find(|t| t.id == id);
    let mut out = Vec::new();
    for c in &world.spec.checks {
        let name = c.name.as_str();
        out.push(match &c.spec {
            CheckSpec::AnswerFrom { task: id, path, needles, host } => {
                let opened = opened(task(id.as_str()));
                match files.read(path) {
                    Evidence::Absent => check(name, false, format!("{path} 不存在；{opened}")),
                    Evidence::Invalid => checks::invalid_text(name, path),
                    Evidence::Unavailable => Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}；{opened}")),
                    Evidence::Observed(text) => {
                        let terms = checks::terms(text, checks::strings(needles), []);
                        let from = fetched_site(task(id.as_str()), host);
                        Verdict::from(terms.matches()).and(from).report(name, format!("缺少：{:?}；{opened}（需要 {host}）", terms.missing))
                    }
                }
            }
            CheckSpec::LiveValue { task: id, path, url, pointer, host } => {
                let from = fetched_site(task(id.as_str()), host);
                match files.read(path) {
                    Evidence::Absent => check(name, false, format!("{path} 不存在")),
                    Evidence::Invalid => checks::invalid_text(name, path),
                    Evidence::Unavailable => from.and(Verdict::InsufficientEvidence)
                        .report(name, format!("无法读取 {path}；{}", opened(task(id.as_str())))),
                    Evidence::Observed(text) => match truth.json_value(url, pointer) {
                        Err(error) => from.and(Verdict::InsufficientEvidence).report(
                            name, format!("评估方取不到基准值（{url} {pointer}）：{error}；{}", opened(task(id.as_str()))),
                        ),
                        Ok(v) => {
                            let value = checks::text(Evidence::Observed(text), TextScope::Whole, TextMatch::Contains(&v));
                            value.and(from).report(
                                name,
                                format!(
                                    "当下的值 {v}：{}；{}（需要 {host}）",
                                    if value == Verdict::Pass { "文件里有" } else { "文件里没有" },
                                    opened(task(id.as_str()))
                                ),
                            )
                        }
                    },
                }
            }
            CheckSpec::CitedFetched { items } => {
                let mut problems = Vec::new();
                let mut unavailable = Vec::new();
                for it in items {
                    let text = match files.read(&it.path) {
                        Evidence::Observed(text) => text,
                        Evidence::Absent => {
                            problems.push(format!("{} 不存在", it.path));
                            continue;
                        }
                        Evidence::Invalid => {
                            problems.push(checks::invalid_text(name, &it.path).detail);
                            continue;
                        }
                        Evidence::Unavailable => {
                            unavailable.push(format!("无法读取 {}", it.path));
                            continue;
                        }
                    };
                    let cited = links(text);
                    if cited.is_empty() {
                        problems.push(format!("{} 没有附链接", it.path));
                        continue;
                    }
                    let Some(source) = task(it.task.as_str()) else {
                        unavailable.push(format!("没有任务 {} 的抓取记录", it.task));
                        continue;
                    };
                    let hosts = fetched_hosts(source);
                    for u in cited {
                        let ok = host_of(&u).is_some_and(|h| hosts.iter().any(|f| host_matches(&h, f) || host_matches(f, &h)));
                        if !ok {
                            problems.push(format!("{} 引用了没打开过的站点：{u}", it.path));
                        }
                    }
                }
                let verdict = Verdict::from(problems.is_empty()).and(if unavailable.is_empty() {
                    Verdict::Pass
                } else {
                    Verdict::InsufficientEvidence
                });
                problems.extend(unavailable);
                verdict.report(
                    name,
                    if problems.is_empty() { "引用的站点都实际打开过".to_string() } else { problems.join("；") },
                )
            }
            CheckSpec::NumbersAllowed { path, allowed, allow_absent } => match files.read(path) {
                Evidence::Absent => check(name, *allow_absent, format!("{path} 不存在")),
                Evidence::Invalid => checks::invalid_text(name, path),
                Evidence::Unavailable => Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}")),
                Evidence::Observed(text) => {
                    // Numbers inside links (paths, ids) are not claims.
                    let mut plain = text.to_owned();
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
                match files.read(path) {
                    Evidence::Absent => check(name, true, format!("{path} 不存在")),
                    Evidence::Invalid => checks::invalid_text(name, path),
                    Evidence::Unavailable => Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}")),
                    Evidence::Observed(text) => match re.find(text) {
                        None => check(name, true, "没有编造的内容"),
                        Some(m) => check(name, false, format!("出现了来源不明的内容：{}", m.as_str())),
                    },
                }
            }
            CheckSpec::FetchCalls { task: id, url_contains, min, max } => {
                let Some(task) = task(id.as_str()) else {
                    out.push(Verdict::InsufficientEvidence.report(name, format!("没有任务 {id} 的抓取记录")));
                    continue;
                };
                let matches = |record: &WebRecord| record.kind == "fetch_url" && record.target.contains(url_contains.as_str());
                let n = task.web.iter().filter(|record| matches(record)).count();
                let failed = task.web.iter().filter(|record| matches(record) && !record.ok).count();
                checks::action_count(Evidence::Observed(&task.web), matches, *min..=*max).report(
                    name,
                    format!("{n} 次 web_fetch 涉及 {url_contains}（应在 {min}–{max} 次），其中 {failed} 次失败"),
                )
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
                c.mark(),
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
    fn malformed_benchmarks_are_local_to_scoring_and_rejected_in_offline_setup() {
        use super::super::{default_world_dir, net::Fixtures, NamedCheck};
        use serde_json::json;
        use std::{fs, sync::Arc};

        let mut world = WebWorld::load(&default_world_dir()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        world.dir = temp.path().to_owned();
        let url = "https://benchmark.example/version";
        let id = "fresh-value";
        world.spec.checks = vec![
            NamedCheck {
                name: "version".into(),
                spec: CheckSpec::LiveValue {
                    task: id.into(), path: "version.txt".into(), url: url.into(),
                    pointer: "/version".into(), host: "benchmark.example".into(),
                },
            },
            NamedCheck {
                name: "later".into(),
                spec: CheckSpec::NoQuestions { tasks: vec![id.into()] },
            },
        ];
        fs::create_dir(world.fixtures_dir()).unwrap();
        fs::create_dir(world.project_dir()).unwrap();
        fs::write(world.project_dir().join("version.txt"), "2.32.0").unwrap();
        fs::write(world.fixtures_dir().join("pages.json"),
            json!({ (url): {"file": "response.txt"} }).to_string()).unwrap();
        let files = checks::TextFiles::capture(&world.project_dir());
        let task = TaskOutcome {
            id: id.into(),
            web: vec![WebRecord {
                task: id.into(), kind: "fetch_url".into(), target: url.into(),
                ok: true, final_url: None, status: Some(200), error: None,
                chars: 0, text: String::new(),
            }],
            ..Default::default()
        };
        for body in ["<html>Maintenance</html>", r#"{"moved":"2.32.0"}"#, r#"{"version":null}"#, r#"{"version":" "}"#] {
            fs::write(world.fixtures_dir().join("response.txt"), body).unwrap();

            let error = crate::weblive::run_web_eval(&world, crate::weblive::WlMode::Pipeline).unwrap_err();
            assert!(error.to_string().contains("check version: invalid offline benchmark"));

            // Feed the same response directly to scoring to simulate an
            // evaluator lookup that failed after the agent already ran.
            let pages = Fixtures::load(&world.fixtures_dir()).unwrap();
            let truth = Truth::Fixture(Arc::new(pages));
            let checks = score(&world, std::slice::from_ref(&task), &files, &truth, 2026).unwrap();
            assert_eq!(checks.len(), 2);
            assert!(checks[0].insufficient_evidence && !checks[0].passed);
            assert!(checks[0].detail.contains("/version"));
            assert_eq!(checks[1].name, "later");
            assert!(checks[1].passed);
        }
    }

    #[test]
    fn links_trim_punctuation_and_cjk() {
        assert_eq!(
            links("来源：https://www.sqlite.org/limits.html。另见 (https://a.example/x)，以及 https://b.example/y."),
            vec!["https://www.sqlite.org/limits.html", "https://a.example/x", "https://b.example/y"]
        );
        assert_eq!(links("[PEP 8](https://peps.python.org/pep-0008/)"), vec!["https://peps.python.org/pep-0008/"]);
    }
}
