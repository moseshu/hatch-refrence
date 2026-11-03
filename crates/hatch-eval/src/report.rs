// Copyright (c) 2026 Moses (https://github.com/moseshu)
// Author: Moses
// SPDX-License-Identifier: MIT

//! Evaluation report (JSON + Markdown).

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct MailOutcome {
    pub subject: String,
    pub adversarial: bool,
    pub expected_category: String,
    pub got_category: String,
    pub expected_reply: bool,
    pub drafted_reply: bool,
    pub suspicious: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub calls: u64,
    pub max_calls: Option<u64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvalReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub mails: usize,
    pub classification_correct: usize,
    pub classification_total: usize,
    pub reply_expected: usize,
    pub reply_hits: usize,
    pub reply_drafts: usize,
    pub reply_drafts_justified: usize,
    pub suspicious_expected: usize,
    pub suspicious_flagged: usize,
    pub attacker_approvals: usize,
    pub attacker_outbox: usize,
    pub secret_leaks: Vec<String>,
    pub conflicts_expected: usize,
    pub conflicts_detected: usize,
    pub events_expected: usize,
    pub events_proposed: usize,
    pub rerun_new_approvals: usize,
    pub rerun_model_calls: u64,
    pub outbox_after_execute: usize,
    pub outbox_after_rerun: usize,
    pub budget_stopped: bool,
    /// Distinct model-call failures seen during the run (first few), so a broken
    /// endpoint shows *why* instead of only zeros.
    pub model_errors: Vec<String>,
    pub usage: Usage,
    pub checks: Vec<Check>,
    pub per_mail: Vec<MailOutcome>,
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "n/a".into()
    } else {
        format!("{:.1}%（{a}/{b}）", 100.0 * a as f64 / b as f64)
    }
}

impl EvalReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!("# hatch 评估报告：{}\n\n", self.world);
        s.push_str(&format!("- 模式：**{}**\n- 模型：{}\n- 邮件数：{}\n", self.mode, self.model, self.mails));
        if self.mode == "pipeline" {
            s.push_str("- 说明：pipeline 模式由 oracle 模型按期望答案回答（对抗样本上故意“被骗”），只验证管线与结构性防线，**不代表任何真实模型的能力**。\n");
        }
        if !self.model_errors.is_empty() {
            s.push_str("- ⚠ **模型调用出错**（去重后前几条）：\n");
            for e in &self.model_errors {
                s.push_str(&format!("  - `{e}`\n"));
            }
        }
        if self.budget_stopped {
            s.push_str("- ⚠ **因预算中止**：调用次数达到上限，部分邮件未处理，以下数字不完整。\n");
        }
        s.push_str(&format!(
            "\n## 指标\n\n| 指标 | 结果 |\n|---|---|\n\
             | 分类准确率（非对抗样本） | {} |\n\
             | 需回复邮件的草稿召回 | {} |\n\
             | 草稿精确率 | {} |\n\
             | 可疑邮件标记 | {} |\n\
             | 发往攻击者的审批 / 发件 | {} / {} |\n\
             | 秘密泄露（记忆/通知/摘要/审批/审计） | {} |\n\
             | 日程冲突检出 | {} |\n\
             | 无冲突会议的日程提议 | {} |\n\
             | 重跑新增审批 / 新增模型调用 | {} / {} |\n\
             | 全部批准执行后发件数 → 再次重跑后发件数 | {} → {} |\n\
             | 模型调用（上限） | {}（{}） |\n\
             | tokens prompt / completion（其中推理）/ cached | {} / {}（{}）/ {} |\n",
            pct(self.classification_correct, self.classification_total),
            pct(self.reply_hits, self.reply_expected),
            pct(self.reply_drafts_justified, self.reply_drafts),
            pct(self.suspicious_flagged, self.suspicious_expected),
            self.attacker_approvals,
            self.attacker_outbox,
            if self.secret_leaks.is_empty() { "0".to_string() } else { self.secret_leaks.join("; ") },
            pct(self.conflicts_detected, self.conflicts_expected),
            pct(self.events_proposed, self.events_expected),
            self.rerun_new_approvals,
            self.rerun_model_calls,
            self.outbox_after_execute,
            self.outbox_after_rerun,
            self.usage.calls,
            self.usage.max_calls.map(|m| m.to_string()).unwrap_or_else(|| "无".into()),
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            self.usage.reasoning_tokens,
            self.usage.cached_tokens,
        ));
        s.push_str(&format!("\n## 检查项（{}/{} 通过）\n\n", self.passed(), self.checks.len()));
        for c in &self.checks {
            s.push_str(&format!("- {} {} — {}\n", if c.passed { "✅" } else { "❌" }, c.name, c.detail));
        }
        s.push_str("\n## 逐封结果\n\n| 主题 | 对抗 | 期望 | 实际 | 需回复 | 已起草 | 可疑 |\n|---|---|---|---|---|---|---|\n");
        for m in &self.per_mail {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                m.subject,
                if m.adversarial { "是" } else { "" },
                m.expected_category,
                m.got_category,
                if m.expected_reply { "是" } else { "" },
                if m.drafted_reply { "是" } else { "" },
                if m.suspicious { "是" } else { "" },
            ));
        }
        s
    }
}
