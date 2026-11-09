//! How a turn ended, and the receipt shown when it did not end in an answer.

use crate::reanchor::PlanTurnReport;
use crate::system_prompt::LayerTokens;
use hatch_llm::Usage;

#[derive(Clone, Debug, PartialEq)]
pub enum StopReason {
    Answered,
    StepLimit,
    Budget(String),
    ModelError(String),
    /// Stopped on request (Ctrl-C, parent cancelled). A third terminal state:
    /// not a failure, not "ran out of steps". Effects already applied stay.
    Cancelled(String),
    /// D2: the turn drifted off its bound plan (N consecutive off-plan
    /// calls, or one off-plan state-changing call). The plan was flagged
    /// `needs_reapproval`; the turn stopped instead of running on.
    PlanDeviation(String),
}

#[derive(Clone, Debug)]
pub struct TurnReport {
    pub answer: Option<String>,
    pub stop: StopReason,
    pub steps: usize,
    pub tool_calls: Vec<String>,
    pub approvals: Vec<String>,
    pub compacted: bool,
    /// A background compaction job was requested for this session. The
    /// turn itself ran with the full history; the summary lands next turn.
    pub compaction_requested: bool,
    /// Model calls that returned an answer in this turn.
    pub model_calls: u64,
    pub usage: Usage,
    /// Per-layer estimated token counts of the last assembled context
    /// of this turn (spec, memory, skills, summary, history, ...).
    /// ESTIMATES from the local `chars / 4` heuristic, not provider
    /// usage: see [`LayerTokens`]. All zero when the turn never
    /// assembled a context (cancelled before the first step).
    pub layers: LayerTokens,
    /// The context contained untrusted content when the turn ended.
    pub tainted: bool,
    /// Sub-agent runs started in this turn.
    pub subagents: Vec<String>,
    /// D2: what this turn did against its bound plan (None when the turn
    /// was unbound). Carries even when the turn answered normally.
    pub plan: Option<PlanTurnReport>,
}

impl Default for TurnReport {
    fn default() -> Self {
        TurnReport {
            answer: None,
            stop: StopReason::StepLimit,
            steps: 0,
            tool_calls: vec![],
            approvals: vec![],
            compacted: false,
            compaction_requested: false,
            model_calls: 0,
            usage: Usage::default(),
            layers: LayerTokens::default(),
            tainted: false,
            subagents: vec![],
            plan: None,
        }
    }
}

pub(crate) fn add_usage(total: &mut Usage, u: &Usage) {
    total.prompt_tokens += u.prompt_tokens;
    total.completion_tokens += u.completion_tokens;
    total.cached_tokens += u.cached_tokens;
    total.reasoning_tokens += u.reasoning_tokens;
}

impl TurnReport {
    pub fn is_cancelled(&self) -> bool {
        matches!(self.stop, StopReason::Cancelled(_))
    }

    /// One-line summary of what was done so far (used in receipts and handoffs).
    pub fn done_summary(&self) -> String {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for t in &self.tool_calls {
            match counts.iter_mut().find(|(n, _)| n == t) {
                Some((_, c)) => *c += 1,
                None => counts.push((t.clone(), 1)),
            }
        }
        if counts.is_empty() {
            return "还没有调用任何工具".into();
        }
        let list: Vec<String> = counts.iter().map(|(n, c)| format!("{n}×{c}")).collect();
        format!("调用了 {}", list.join("、"))
    }

    /// What happened in a turn that did not reach an answer — never pretend
    /// the task is done.
    pub fn receipt(&self) -> String {
        let why = match &self.stop {
            StopReason::Answered => return "已完成。".into(),
            StopReason::StepLimit => format!("达到单轮步数上限（{} 步），任务没有完成", self.steps),
            StopReason::Budget(m) => format!("模型调用预算用尽（{m}），任务没有完成"),
            StopReason::ModelError(m) => format!("模型调用失败（{m}），任务没有完成"),
            StopReason::Cancelled(r) => format!("已按你的要求停止（{r}），停在第 {} 步", self.steps),
            StopReason::PlanDeviation(m) => format!("偏离计划，已停下等你决定（{m}），任务没有完成"),
        };
        let mut s = format!("（{why}。）\n");
        if !self.tool_calls.is_empty() {
            s.push_str(&format!("已做：{}。已经生效的操作不会回滚。\n", self.done_summary()));
        }
        if let Some(p) = &self.plan {
            s.push_str(&format!(
                "计划覆盖：{} / {} 步；计划外调用：{}。\n",
                p.covered.len(),
                p.covered.len() + p.uncovered.len(),
                if p.off_plan.is_empty() {
                    "无".into()
                } else {
                    p.off_plan.join("、")
                }
            ));
        }
        if !self.subagents.is_empty() {
            s.push_str(&format!("启动过的子代理：{}（`hatch agents show <id>` 查看）。\n", self.subagents.join(", ")));
        }
        if !self.approvals.is_empty() {
            s.push_str(&format!("等待你确认的动作：{}。\n", self.approvals.join(", ")));
        }
        s.push_str("没做完的部分需要继续：直接追问“继续”即可，已完成的步骤不会重复。");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_carries_layer_breakdown() {
        let r = TurnReport::default();
        assert_eq!(r.layers, LayerTokens::default());
        assert_eq!(r.layers.total(), 0);
        let mut r = TurnReport {
            layers: LayerTokens { spec: 5, history: 7, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(r.layers.spec, 5);
        assert_eq!(r.layers.history, 7);
        assert_eq!(r.layers.total(), 12);
        r.layers.spec = 6;
        assert_eq!(r.layers.total(), 13, "the breakdown is carried, not recomputed");
    }

    #[test]
    fn cancelled_receipt_is_honest() {
        let mut r = TurnReport { steps: 3, tool_calls: vec!["fs_read".into(), "fs_read".into(), "shell_run".into()], ..Default::default() };
        r.stop = StopReason::Cancelled("Ctrl-C".into());
        let s = r.receipt();
        assert!(s.contains("已按你的要求停止（Ctrl-C），停在第 3 步"), "{s}");
        assert!(s.contains("fs_read×2、shell_run×1"));
        assert!(s.contains("不会回滚"));
        assert!(r.is_cancelled());
        r.stop = StopReason::Answered;
        assert_eq!(r.receipt(), "已完成。");
        assert_eq!(r.stop, StopReason::Answered);
    }
}
