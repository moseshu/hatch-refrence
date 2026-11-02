// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Run receipt: what was done, what was not done and why, what needs a decision.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ItemReceipt {
    pub mail_id: String,
    pub from: String,
    pub subject: String,
    pub category: String,
    pub priority: String,
    pub summary: String,
    pub suspicious: bool,
    pub notes: Vec<String>,
    pub actions: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub run_id: String,
    pub source: String,
    pub fetched: usize,
    pub new_mails: usize,
    /// Mails processed in this run (new + resumed from an earlier interrupted run).
    pub processed: usize,
    pub by_category: BTreeMap<String, usize>,
    pub items: Vec<ItemReceipt>,
    /// `(approval id, title)` queued for the user's decision in this run.
    pub approvals_queued: Vec<(String, String)>,
    pub auto_executed: Vec<String>,
    pub conflicts: Vec<String>,
    pub not_done: Vec<String>,
    pub errors: Vec<String>,
    pub budget_stopped: bool,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl Receipt {
    pub fn render_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("## 邮件分诊回执（{}）\n\n", self.run_id));
        s.push_str(&format!(
            "来源 {}：拉取 {} 封，新邮件 {} 封，本次处理 {} 封。\n\n",
            self.source, self.fetched, self.new_mails, self.processed
        ));
        s.push_str("### 已完成\n");
        if self.by_category.is_empty() {
            s.push_str("- 没有需要处理的新邮件\n");
        } else {
            let cats: Vec<String> = self.by_category.iter().map(|(k, v)| format!("{k} {v}")).collect();
            s.push_str(&format!("- 分类：{}\n", cats.join("，")));
        }
        for i in &self.items {
            let flag = if i.suspicious { " ⚠可疑" } else { "" };
            s.push_str(&format!("- [{}|{}]{} {} — {}：{}\n", i.category, i.priority, flag, i.from, i.subject, i.summary));
            for a in &i.actions {
                s.push_str(&format!("    - {a}\n"));
            }
            for n in &i.notes {
                s.push_str(&format!("    - 注：{n}\n"));
            }
        }
        for a in &self.auto_executed {
            s.push_str(&format!("- 已按常设授权自动执行：{a}\n"));
        }
        s.push_str("\n### 未完成\n");
        let mut nd = self.not_done.clone();
        if self.auto_executed.is_empty() {
            nd.push("没有发出任何邮件、没有修改日历：所有外发动作都在等待你的确认。".into());
        }
        if self.budget_stopped {
            nd.push("模型调用预算已用尽，剩余邮件未处理（下次运行会自动续上）。".into());
        }
        for e in &self.errors {
            nd.push(format!("出错：{e}"));
        }
        for n in nd {
            s.push_str(&format!("- {n}\n"));
        }
        s.push_str("\n### 待你决定\n");
        if self.approvals_queued.is_empty() && self.conflicts.is_empty() {
            s.push_str("- 暂无\n");
        }
        for (id, title) in &self.approvals_queued {
            s.push_str(&format!("- {title}（`hatch approvals show {id}`）\n"));
        }
        for c in &self.conflicts {
            s.push_str(&format!("- 日程冲突：{c}\n"));
        }
        s.push_str(&format!(
            "\n模型调用 {} 次；tokens：prompt {} / completion {} / cached {}。\n",
            self.model_calls, self.prompt_tokens, self.completion_tokens, self.cached_tokens
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_three_sections() {
        let mut r = Receipt { run_id: "run_1".into(), source: "fixture".into(), ..Default::default() };
        r.by_category.insert("reply".into(), 1);
        r.approvals_queued.push(("apr_1".into(), "回复李雷".into()));
        r.budget_stopped = true;
        let md = r.render_markdown();
        for h in ["### 已完成", "### 未完成", "### 待你决定", "hatch approvals show apr_1", "预算已用尽", "没有发出任何邮件"] {
            assert!(md.contains(h), "missing {h}");
        }
    }
}
