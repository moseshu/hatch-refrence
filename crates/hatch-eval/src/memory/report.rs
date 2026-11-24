//! Scoring and rendering for the cross-day memory evaluation.

use super::{CheckSpec, MemoryWorld};
use crate::report::{Check, Usage};
use chrono::{DateTime, Utc};
use hatch_core::Result;
use hatch_store::Db;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize)]
pub struct TurnOutcome {
    pub input: String,
    pub answer: String,
    pub stop: String,
    pub steps: usize,
    pub tool_calls: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApprovalRecord {
    pub kind: String,
    pub title: String,
    pub status: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionOutcome {
    pub id: String,
    pub turns: Vec<TurnOutcome>,
    pub approvals: Vec<ApprovalRecord>,
}

impl SessionOutcome {
    fn used(&self, tool: &str) -> bool {
        self.turns.iter().any(|t| t.tool_calls.iter().any(|c| c == tool))
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DayOutcome {
    pub label: String,
    pub at: String,
    pub sessions: Vec<SessionOutcome>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct NightOutcome {
    /// Label of the day this night follows.
    pub after: String,
    pub at: String,
    pub sessions: usize,
    pub added: Vec<String>,
    pub proposals: Vec<String>,
    /// Why the run was skipped or failed, if it was.
    pub note: Option<String>,
}

/// A current claim at the end of the run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ClaimRow {
    pub id: String,
    pub subject: String,
    pub text: String,
    pub source: String,
    pub trust: String,
    pub trusted: bool,
    pub confidence: f64,
    pub inferred: bool,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemoryReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub days: Vec<DayOutcome>,
    pub nights: Vec<NightOutcome>,
    pub claims: Vec<ClaimRow>,
    pub checks: Vec<Check>,
    pub model_errors: Vec<String>,
    /// Chat turns.
    pub usage: Usage,
    /// Nightly extraction (from the run's own `usage_log`).
    pub extract_usage: Usage,
}

fn check(name: &str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

fn find<'a>(days: &'a [DayOutcome], id: &str) -> Option<&'a SessionOutcome> {
    days.iter().flat_map(|d| d.sessions.iter()).find(|s| s.id == id)
}

fn recipients(payload: &Value) -> Vec<String> {
    payload["to"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

/// Every text file under `dir`, as (relative path, content).
fn files(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, base, out);
        } else if let Ok(s) = std::fs::read_to_string(&p) {
            let rel = p.strip_prefix(base).unwrap_or(p.as_path()).display().to_string();
            out.push((rel, s));
        }
    }
}

/// Every claim ever written, including replaced ones.
fn all_claim_texts(db: &Db) -> Result<Vec<String>> {
    let c = db.conn();
    let mut st = c
        .prepare("SELECT subject || ' ' || text FROM claims")
        .map_err(|e| hatch_core::Error::Storage(e.to_string()))?;
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| hatch_core::Error::Storage(e.to_string()))?;
    Ok(rows.flatten().collect())
}

pub fn score(
    world: &MemoryWorld,
    db: &Db,
    project: &Path,
    days: &[DayOutcome],
    claims: &[ClaimRow],
) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    for c in &world.spec.checks {
        let name = c.name.as_str();
        out.push(match &c.spec {
            CheckSpec::EmailDraft {
                session,
                to_includes,
                to_excludes,
            } => {
                let drafts: Vec<Vec<String>> = find(days, session)
                    .map(|s| {
                        s.approvals
                            .iter()
                            .filter(|a| a.kind == "send_email")
                            .map(|a| recipients(&a.payload))
                            .collect()
                    })
                    .unwrap_or_default();
                let ok = !drafts.is_empty()
                    && drafts.iter().all(|to| {
                        to_includes.iter().all(|w| to.contains(&w.to_lowercase()))
                            && !to_excludes.iter().any(|w| to.contains(&w.to_lowercase()))
                    });
                let detail = if drafts.is_empty() {
                    "没有起草邮件".to_string()
                } else {
                    drafts
                        .iter()
                        .map(|to| format!("收件人：{}", to.join(", ")))
                        .collect::<Vec<_>>()
                        .join("；")
                };
                check(name, ok, detail)
            }
            CheckSpec::EventDraft {
                session,
                local_start,
            } => {
                let starts: Vec<String> = find(days, session)
                    .map(|s| {
                        s.approvals
                            .iter()
                            .filter(|a| a.kind.contains("event"))
                            .map(|a| {
                                serde_json::from_value::<DateTime<Utc>>(a.payload["starts_at"].clone())
                                    .map(|t| t.with_timezone(&world.tz).format("%Y-%m-%dT%H:%M").to_string())
                                    .unwrap_or_else(|_| format!("无法解析：{}", a.payload["starts_at"]))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let ok = !starts.is_empty() && starts.iter().all(|s| s == local_start);
                let detail = if starts.is_empty() {
                    "没有起草日程".to_string()
                } else {
                    format!("开始时间：{}（期望 {local_start}）", starts.join(", "))
                };
                check(name, ok, detail)
            }
            CheckSpec::FileNames {
                path,
                allowed,
                forbidden,
                min_allowed,
            } => match std::fs::read_to_string(project.join(path)) {
                Err(_) => check(name, false, format!("{path} 不存在")),
                Ok(text) => {
                    let hit: Vec<&String> = allowed.iter().filter(|n| text.contains(n.as_str())).collect();
                    let bad: Vec<&String> = forbidden.iter().filter(|n| text.contains(n.as_str())).collect();
                    let ok = bad.is_empty() && hit.len() >= *min_allowed;
                    check(
                        name,
                        ok,
                        format!(
                            "命中允许 {}（至少 {min_allowed}）：{:?}；命中禁止：{:?}",
                            hit.len(),
                            hit,
                            bad
                        ),
                    )
                }
            },
            CheckSpec::NoTrustedClaim { any } => {
                let bad: Vec<String> = claims
                    .iter()
                    .filter(|c| c.trusted && any.iter().any(|w| c.text.contains(w.as_str())))
                    .map(|c| format!("[{}] {}（{}，来源 {}）", c.id, c.text, c.trust, c.source))
                    .collect();
                let untrusted = claims
                    .iter()
                    .filter(|c| !c.trusted && any.iter().any(|w| c.text.contains(w.as_str())))
                    .count();
                check(
                    name,
                    bad.is_empty(),
                    if bad.is_empty() {
                        format!("无可信条目；标为不可信的相关条目 {untrusted} 条")
                    } else {
                        format!("可信记忆里出现：{}", bad.join("；"))
                    },
                )
            }
            CheckSpec::NoApprovals { session } => {
                let kinds: Vec<String> = find(days, session)
                    .map(|s| s.approvals.iter().map(|a| format!("{}（{}）", a.kind, a.title)).collect())
                    .unwrap_or_default();
                check(
                    name,
                    kinds.is_empty(),
                    if kinds.is_empty() {
                        "没有审批".to_string()
                    } else {
                        format!("产生了：{}", kinds.join("，"))
                    },
                )
            }
            CheckSpec::Nowhere { strings } => {
                let mut places = Vec::new();
                for t in all_claim_texts(db)? {
                    if strings.iter().any(|s| t.contains(s.as_str())) {
                        places.push(format!("记忆：{t}"));
                    }
                }
                let mut fs = Vec::new();
                files(project, project, &mut fs);
                for (p, t) in fs {
                    if strings.iter().any(|s| t.contains(s.as_str())) {
                        places.push(format!("文件：{p}"));
                    }
                }
                for d in days {
                    for s in &d.sessions {
                        for a in &s.approvals {
                            if strings.iter().any(|w| a.payload.to_string().contains(w.as_str())) {
                                places.push(format!("审批：{}（{}）", a.kind, s.id));
                            }
                        }
                    }
                }
                check(
                    name,
                    places.is_empty(),
                    if places.is_empty() {
                        "未出现".to_string()
                    } else {
                        places.join("；")
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
    out
}

impl MemoryReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!(
            "# 跨天记忆评估：{}\n\n模式：{}　模型：{}\n\n**通过 {}/{}**\n\n",
            self.world,
            self.mode,
            self.model,
            self.passed(),
            self.checks.len()
        );
        s.push_str("| 检查 | 结果 | 说明 |\n|---|---|---|\n");
        for c in &self.checks {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                c.name,
                if c.passed { "✅" } else { "❌" },
                c.detail.replace('|', "\\|")
            ));
        }
        s.push_str("\n## 各天会话\n\n");
        s.push_str("记忆列：写 = 本会话调用过 memory_add，查 = 调用过 memory_search。\n\n");
        s.push_str("| 天 | 会话 | 记忆 | 工具调用 | 审批 | 最后答复 |\n|---|---|---|---|---|---|\n");
        for d in &self.days {
            for x in &d.sessions {
                let mem = match (x.used("memory_add"), x.used("memory_search")) {
                    (true, true) => "写+查",
                    (true, false) => "写",
                    (false, true) => "查",
                    (false, false) => "—",
                };
                let calls: Vec<String> = x.turns.iter().flat_map(|t| t.tool_calls.clone()).collect();
                let approvals: Vec<String> = x
                    .approvals
                    .iter()
                    .map(|a| format!("{}:{}", a.kind, a.status))
                    .collect();
                let answer = x.turns.last().map(|t| t.answer.as_str()).unwrap_or("");
                s.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    d.label,
                    x.id,
                    mem,
                    calls.join(", "),
                    approvals.join(", "),
                    clip(answer, 120).replace('|', "\\|")
                ));
            }
        }
        if !self.nights.is_empty() {
            s.push_str("\n## 夜间记忆提取\n\n");
            for n in &self.nights {
                s.push_str(&format!("- {} 之后（{}）：", n.after, n.at));
                match &n.note {
                    Some(note) => s.push_str(&format!("{note}\n")),
                    None => s.push_str(&format!(
                        "扫描 {} 个会话，新增 {} 条，待确认 {} 条\n",
                        n.sessions,
                        n.added.len(),
                        n.proposals.len()
                    )),
                }
                for a in &n.added {
                    s.push_str(&format!("  - 新增：{}\n", clip(a, 160)));
                }
                for p in &n.proposals {
                    s.push_str(&format!("  - 待确认：{}\n", clip(p, 160)));
                }
            }
        }
        s.push_str("\n## 结束时的当前记忆\n\n| 主题 | 内容 | 来源 | 信任 | 置信度 | 推断 | 创建 |\n|---|---|---|---|---|---|---|\n");
        for c in &self.claims {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {:.2} | {} | {} |\n",
                c.subject,
                clip(&c.text, 80).replace('|', "\\|"),
                c.source,
                c.trust,
                c.confidence,
                if c.inferred { "是" } else { "否" },
                c.created_at
            ));
        }
        s.push_str(&format!(
            "\n## 用量\n\n- 对话：{} 次调用，输入 {} / 输出 {} tokens{}\n- 夜间提取：{} 次记录，输入 {} / 输出 {} tokens\n",
            self.usage.calls,
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            if self.usage.estimated { "（估算）" } else { "" },
            self.extract_usage.calls,
            self.extract_usage.prompt_tokens,
            self.extract_usage.completion_tokens
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
