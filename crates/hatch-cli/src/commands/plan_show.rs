//! `hatch plan show` rendering (split out of `commands::plan` to keep
//! that file under the 400-line limit).

use hatch_plan::Plan;

pub(crate) fn print_plan(plan: &Plan) {
    println!("计划 {}  [{}]", plan.id, plan.status.as_str());
    println!("任务：{}", plan.task);
    if !plan.goal.is_empty() {
        println!("目标：{}", plan.goal);
    }
    if let Some(s) = &plan.current_step {
        println!("当前步骤：{s}");
    }
    if !plan.milestones.is_empty() {
        let marks: Vec<String> = plan
            .milestones
            .iter()
            .map(|m| format!("{} {}", if m.done { "[x]" } else { "[ ]" }, m.title))
            .collect();
        println!("里程碑：{}", marks.join("；"));
    }
    if !plan.model.is_empty() {
        println!("模型：{}", plan.model);
    }
    println!("创建：{}  更新：{}", plan.created_at, plan.updated_at);
    if let Some(d) = &plan.decided_at {
        println!(
            "决定：{} 由 {} 决定（{}）",
            d, plan.decided_by, plan.decision_note
        );
    }
    if !plan.deviation.is_empty() {
        println!("偏差：{}", plan.deviation);
    }
    for (i, s) in plan.steps.iter().enumerate() {
        println!(
            "  {}. [{}] {} — {} {}（风险 {}；预期：{}）",
            i + 1,
            s.status.as_str(),
            s.id,
            s.title,
            s.tool,
            s.risk.as_str(),
            s.expected
        );
        println!("     参数：{}", s.arguments);
        if !s.files.is_empty() {
            println!("     文件：{}", s.files.join(", "));
        }
        if !s.output.is_empty() {
            println!("     结果：{}", s.output);
        }
        if let Some(a) = &s.approval_id {
            println!("     等待审批：{a}");
        }
    }
}

