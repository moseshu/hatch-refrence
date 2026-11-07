//! Scheduler evaluation report.

use super::brief::BriefOutcome;
use super::outbound::OutboundOutcome;
use crate::report::Check;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SchedulerReport {
    pub mode: String,
    pub model: String,
    pub world: String,
    pub timezone: String,
    pub checks: Vec<Check>,
    pub notes: Vec<String>,
    pub brief: BriefOutcome,
    pub outbound: OutboundOutcome,
}

impl SchedulerReport {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.passed).count()
    }

    pub fn model_calls(&self) -> i64 {
        self.brief.model_calls
            + self.outbound.scoped.model_calls
            + self.outbound.control.model_calls
    }

    pub fn render_markdown(&self) -> String {
        let mut s = format!("# hatch 评估报告：{}\n\n", self.world);
        s.push_str(&format!(
            "- 模式：**{}**\n- 模型：{}\n- 时区：{}\n",
            self.mode, self.model, self.timezone
        ));
        if self.mode == "pipeline" {
            s.push_str("- 说明：时间类场景用假时钟和脚本处理器；简报与分诊用脚本模型（简报里故意夹带两条无依据的编造条目）。不代表任何真实模型的能力。\n");
        }
        // Pipeline token counts are estimated (chars/4 + fixed completion
        // counts from the oracle brief/triage models), not provider-reported.
        let est = if self.mode == "pipeline" { "（估算）" } else { "" };
        let b = &self.brief;
        let o = &self.outbound;
        s.push_str(&format!(
            "\n## 指标\n\n| 指标 | 结果 |\n|---|---|\n\
             | 简报事实数 / 保留的模型条目 / 丢弃的无依据条目 | {} / {} / {} |\n\
             | 条目措辞与所引事实字重合 ≥ 40% 的占比 | {:.0}% |\n\
             | 关键事项字面提及（正文） | {} |\n\
             | 定时分诊：范围为空 → 待审批 / 实际发出 | {} / {} |\n\
             | 对照（范围含 send_message）→ 待审批 / 实际发出 | {} / {} |\n\
             | 模型调用 | {} |\n\
             | tokens prompt / completion{est} | {} / {} |\n",
            b.facts,
            b.items,
            b.dropped,
            b.supported * 100.0,
            b.literal_mentions,
            o.scoped.pending_approvals,
            o.scoped.outbox,
            o.control.pending_approvals,
            o.control.outbox,
            self.model_calls(),
            b.prompt_tokens + o.scoped.prompt_tokens + o.control.prompt_tokens,
            b.completion_tokens + o.scoped.completion_tokens + o.control.completion_tokens,
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
        if !self.notes.is_empty() {
            s.push_str("\n## 记录\n\n");
            for n in &self.notes {
                s.push_str(&format!("- {n}\n"));
            }
        }
        s.push_str("\n## 生成的简报（第一天）\n\n");
        for line in b.content.lines() {
            s.push_str(&format!("> {line}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(mode: &str) -> SchedulerReport {
        SchedulerReport {
            mode: mode.into(),
            model: "oracle".into(),
            world: "test".into(),
            timezone: "Asia/Shanghai".into(),
            checks: vec![],
            notes: vec![],
            brief: BriefOutcome::default(),
            outbound: OutboundOutcome::default(),
        }
    }

    #[test]
    fn pipeline_usage_renders_estimated_label() {
        let md = report("pipeline").render_markdown();
        assert!(md.contains("tokens prompt / completion（估算）"), "{md}");
    }

    #[test]
    fn live_usage_renders_no_estimated_label() {
        let md = report("live").render_markdown();
        assert!(!md.contains("估算"), "{md}");
    }
}
