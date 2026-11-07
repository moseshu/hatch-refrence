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
    /// Whether these token counts are estimated (chars/4 + a fixed
    /// completion count from a pipeline oracle) rather than
    /// provider-reported. True in pipeline mode, false in live mode;
    /// an estimate must never be read as a billed number.
    pub estimated: bool,
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

/// Per-check verdict flags across the repeated runs of one world
/// (`hatch eval --repeat N`), after OpenMausBot's skill-bench flags
/// (`nonDiscriminating` / `highVariance` in `evals/scorers/skill-bench.ts`).
///
/// A repeat report has no arms (no with/without skill), so "discriminates"
/// means "tells runs apart":
/// - `non_discriminating`: the check's verdict is identical in every
///   observed run — always passes or always fails. It cannot distinguish
///   one run from another, so it is a control item, not evidence of
///   stability or of a change. An always-failing check counts too: a
///   check that fails every time tells you nothing about run-to-run
///   differences either. With a single run there is nothing to
///   discriminate with, so `n == 1` is non-discriminating as well.
/// - `high_variance`: the verdict flips across runs (some pass, some
///   fail). Only assessable with at least 2 runs; at `n == 1` it is
///   false, and the report says so plainly instead of letting the false
///   read as "no variance observed".
#[derive(Clone, Debug, Serialize)]
pub struct CheckFlags {
    pub name: String,
    pub passes: usize,
    pub runs: usize,
    pub non_discriminating: bool,
    pub high_variance: bool,
}

/// Compute [`CheckFlags`] for every check of the first report, reading
/// each check's verdict by name from every run. A check absent from a run
/// does not contribute to that run's count (it never happens for one
/// world, but a missing verdict must not be read as a failure).
pub fn check_flags(reports: &[EvalReport]) -> Vec<CheckFlags> {
    let mut flags = Vec::new();
    let Some(first) = reports.first() else {
        return flags;
    };
    for check in &first.checks {
        let mut passes = 0;
        let mut runs = 0;
        for r in reports {
            if let Some(c) = r.checks.iter().find(|c| c.name == check.name) {
                runs += 1;
                if c.passed {
                    passes += 1;
                }
            }
        }
        flags.push(CheckFlags {
            name: check.name.clone(),
            passes,
            runs,
            // Identical in every observed run (all pass, all fail, or a
            // single observation): cannot tell runs apart.
            non_discriminating: passes == 0 || passes == runs,
            // Flips across runs; needs at least two observations.
            high_variance: runs >= 2 && passes > 0 && passes < runs,
        });
    }
    flags
}

/// Result of running the same world N times (`hatch eval --repeat N`).
///
/// Repeated runs report the **mean**, never a cherry-picked best run:
/// a single run is still labelled as a single run (see [`EvalReport`]).
/// Pipeline mode is deterministic (oracle), so its mean equals the
/// single-run values — the repeat tests assert exactly that.
#[derive(Clone, Debug, Serialize)]
pub struct RepeatedEvalReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub runs: usize,
    pub per_run_checks: Vec<usize>,
    pub per_run_total_checks: Vec<usize>,
    pub per_run_calls: Vec<u64>,
    pub per_run_prompt_tokens: Vec<u64>,
    pub per_run_completion_tokens: Vec<u64>,
    pub mean_checks: f64,
    pub min_checks: usize,
    pub max_checks: usize,
    pub mean_calls: f64,
    pub mean_prompt_tokens: f64,
    pub mean_completion_tokens: f64,
    /// Per-check verdict flags across the runs (see [`CheckFlags`]).
    pub check_flags: Vec<CheckFlags>,
    /// Summary counts of the flags above.
    pub non_discriminating_count: usize,
    pub high_variance_count: usize,
    pub reports: Vec<EvalReport>,
}

impl RepeatedEvalReport {
    pub fn all_passed(&self) -> bool {
        self.reports.iter().all(|r| r.passed() == r.checks.len())
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!("# hatch 评估报告（重复运行）：{}\n\n", self.world);
        s.push_str(&format!(
            "- 模式：**{}**\n- 模型：{}\n- 运行次数：{}\n- 报告取**均值**，不是最好的一次；单次运行的结果仍以单次报告为准。\n",
            self.mode, self.model, self.runs
        ));
        s.push_str("\n## 每次运行\n\n");
        let est = if self.mode == "pipeline" { "（估算）" } else { "" };
        s.push_str(&format!(
            "| # | 检查通过 | 模型调用 | tokens prompt / completion{est} |\n|---|---|---|---|\n"
        ));
        for (i, r) in self.reports.iter().enumerate() {
            s.push_str(&format!(
                "| {} | {}/{} | {} | {} / {} |\n",
                i + 1,
                r.passed(),
                r.checks.len(),
                r.usage.calls,
                r.usage.prompt_tokens,
                r.usage.completion_tokens
            ));
        }
        s.push_str(&format!(
            "\n## 均值汇总\n\n| 指标 | 均值 |\n|---|---|\n| 检查通过 | {:.1}（最低 {}，最高 {}，共 {} 项） |\n| 模型调用 | {:.1} |\n| tokens prompt{est} | {:.1} |\n| tokens completion{est} | {:.1} |\n",
            self.mean_checks,
            self.min_checks,
            self.max_checks,
            self.reports.first().map(|r| r.checks.len()).unwrap_or(0),
            self.mean_calls,
            self.mean_prompt_tokens,
            self.mean_completion_tokens,
        ));
        s.push_str("\n## 检查项标志\n\n");
        if self.runs < 2 {
            s.push_str("- 高方差需至少运行 2 次才能评估：本次只运行 1 次，未评估（不是“没有方差”）。\n");
        }
        s.push_str("- 非歧视性：某项检查在所有运行中结论相同（全通过或全不通过）——它区分不了各次运行，是对照项而非效应证据。\n");
        s.push_str("- 高方差：某项检查在各次运行间结论翻转（有的通过、有的不通过）。\n\n");
        s.push_str("| 检查项 | 通过 | 非歧视性 | 高方差 |\n|---|---|---|---|\n");
        for f in &self.check_flags {
            s.push_str(&format!(
                "| {} | {}/{} | {} | {} |\n",
                f.name,
                f.passes,
                f.runs,
                if f.non_discriminating { "是" } else { "" },
                if f.high_variance { "是" } else { "" },
            ));
        }
        s.push_str(&format!(
            "\n非歧视性 {} 项，高方差 {} 项。\n",
            self.non_discriminating_count, self.high_variance_count
        ));
        s.push_str("\n## 首次运行的完整报告\n\n");
        s.push_str(
            &self
                .reports
                .first()
                .map(|r| r.render_markdown())
                .unwrap_or_default(),
        );
        s
    }
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
        s.push_str(&format!(
            "- 模式：**{}**\n- 模型：{}\n- 邮件数：{}\n",
            self.mode, self.model, self.mails
        ));
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
        // Pipeline token counts are estimated (chars/4 + a fixed
        // completion count from the oracle), not provider-reported.
        let est = if self.usage.estimated { "（估算）" } else { "" };
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
             | tokens prompt / completion（其中推理）/ cached{est} | {} / {}（{}）/ {} |\n",
            pct(self.classification_correct, self.classification_total),
            pct(self.reply_hits, self.reply_expected),
            pct(self.reply_drafts_justified, self.reply_drafts),
            pct(self.suspicious_flagged, self.suspicious_expected),
            self.attacker_approvals,
            self.attacker_outbox,
            if self.secret_leaks.is_empty() {
                "0".to_string()
            } else {
                self.secret_leaks.join("; ")
            },
            pct(self.conflicts_detected, self.conflicts_expected),
            pct(self.events_proposed, self.events_expected),
            self.rerun_new_approvals,
            self.rerun_model_calls,
            self.outbox_after_execute,
            self.outbox_after_rerun,
            self.usage.calls,
            self.usage
                .max_calls
                .map(|m| m.to_string())
                .unwrap_or_else(|| "无".into()),
            self.usage.prompt_tokens,
            self.usage.completion_tokens,
            self.usage.reasoning_tokens,
            self.usage.cached_tokens,
        ));
        s.push_str(&format!(
            "\n## 检查项（{}/{} 通过）\n\n",
            self.passed(),
            self.checks.len()
        ));
        for c in &self.checks {
            s.push_str(&format!(
                "- {} {} — {}\n",
                if c.passed { "✅" } else { "❌" },
                c.name,
                c.detail
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn report(checks: &[(&str, bool)]) -> EvalReport {
        EvalReport {
            mode: "pipeline".into(),
            model: "test".into(),
            world: "test".into(),
            mails: 0,
            classification_correct: 0,
            classification_total: 0,
            reply_expected: 0,
            reply_hits: 0,
            reply_drafts: 0,
            reply_drafts_justified: 0,
            suspicious_expected: 0,
            suspicious_flagged: 0,
            attacker_approvals: 0,
            attacker_outbox: 0,
            secret_leaks: vec![],
            conflicts_expected: 0,
            conflicts_detected: 0,
            events_expected: 0,
            events_proposed: 0,
            rerun_new_approvals: 0,
            rerun_model_calls: 0,
            outbox_after_execute: 0,
            outbox_after_rerun: 0,
            budget_stopped: false,
            model_errors: vec![],
            usage: Usage::default(),
            checks: checks
                .iter()
                .map(|(n, p)| Check {
                    name: n.to_string(),
                    passed: *p,
                    detail: String::new(),
                })
                .collect(),
            per_mail: vec![],
        }
    }

    #[test]
    fn empty_reports_have_no_flags() {
        assert!(check_flags(&[]).is_empty());
    }

    #[test]
    fn always_passing_check_is_non_discriminating() {
        let reports = vec![
            report(&[("分类准确", true)]),
            report(&[("分类准确", true)]),
            report(&[("分类准确", true)]),
        ];
        let flags = check_flags(&reports);
        assert_eq!(flags.len(), 1);
        assert_eq!((flags[0].passes, flags[0].runs), (3, 3));
        assert!(flags[0].non_discriminating);
        assert!(!flags[0].high_variance);
    }

    #[test]
    fn always_failing_check_is_non_discriminating_too() {
        // A check that fails every time tells you nothing about run-to-run
        // differences either — it is a control item, not an effect.
        let reports = vec![
            report(&[("秘密泄露", false)]),
            report(&[("秘密泄露", false)]),
        ];
        let flags = check_flags(&reports);
        assert_eq!((flags[0].passes, flags[0].runs), (0, 2));
        assert!(flags[0].non_discriminating);
        assert!(!flags[0].high_variance);
    }

    #[test]
    fn flipping_check_is_high_variance_not_non_discriminating() {
        let reports = vec![
            report(&[("草稿召回", true)]),
            report(&[("草稿召回", false)]),
            report(&[("草稿召回", true)]),
        ];
        let flags = check_flags(&reports);
        assert_eq!((flags[0].passes, flags[0].runs), (2, 3));
        assert!(!flags[0].non_discriminating);
        assert!(flags[0].high_variance);
    }

    #[test]
    fn single_run_cannot_discriminate_and_cannot_show_variance() {
        for passed in [true, false] {
            let reports = vec![report(&[("分类准确", passed)])];
            let flags = check_flags(&reports);
            assert_eq!((flags[0].passes, flags[0].runs), (passed as usize, 1));
            assert!(flags[0].non_discriminating, "n == 1: {passed}");
            assert!(!flags[0].high_variance, "n == 1: {passed}");
        }
    }

    #[test]
    fn identical_verdicts_at_n2_are_not_high_variance() {
        let reports =
            vec![report(&[("分类准确", true)]), report(&[("分类准确", true)])];
        let flags = check_flags(&reports);
        assert!(flags[0].non_discriminating);
        assert!(!flags[0].high_variance);
    }

    #[test]
    fn mixed_checks_flag_independently() {
        let reports = vec![
            report(&[("稳定项", true), ("翻转项", true)]),
            report(&[("稳定项", true), ("翻转项", false)]),
        ];
        let flags = check_flags(&reports);
        assert_eq!(flags.len(), 2);
        assert!(flags[0].non_discriminating && !flags[0].high_variance);
        assert!(!flags[1].non_discriminating && flags[1].high_variance);
    }

    #[test]
    fn check_missing_from_a_run_does_not_count_as_failure() {
        let reports = vec![
            report(&[("分类准确", true), ("稀有项", true)]),
            report(&[("分类准确", true)]),
        ];
        let flags = check_flags(&reports);
        let rare = &flags[1];
        assert_eq!((rare.passes, rare.runs), (1, 1));
        assert!(rare.non_discriminating);
        assert!(!rare.high_variance);
    }

    #[test]
    fn pipeline_usage_renders_estimated_label() {
        let mut r = report(&[]);
        r.mode = "pipeline".into();
        r.usage.estimated = true;
        r.usage.prompt_tokens = 100;
        r.usage.completion_tokens = 60;
        let md = r.render_markdown();
        assert!(md.contains("cached（估算）"), "{md}");
    }

    #[test]
    fn live_usage_renders_no_estimated_label() {
        // Provider-reported usage must never be labelled estimated.
        let mut r = report(&[]);
        r.mode = "live".into();
        r.usage.estimated = false;
        r.usage.prompt_tokens = 100;
        r.usage.completion_tokens = 60;
        let md = r.render_markdown();
        assert!(!md.contains("估算"), "{md}");
    }

    #[test]
    fn usage_estimated_flag_serializes_in_json() {
        let mut r = report(&[]);
        r.usage.estimated = true;
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"estimated\":true"), "{json}");
        let mut live = report(&[]);
        live.usage.estimated = false;
        let json = serde_json::to_string(&live).unwrap();
        assert!(json.contains("\"estimated\":false"), "{json}");
    }
}
