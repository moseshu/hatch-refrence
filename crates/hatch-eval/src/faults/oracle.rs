//! Scripted model for the tool-faults world's pipeline mode.
//!
//! It plays a model that handles every fault well: pages through the long
//! file, retries the transient error, reports the permission error without
//! inventing payroll figures, re-reads the log after the lost write
//! response instead of appending again, and stops after three failed reads
//! during the outage. The point is to prove the harness, the injection and
//! the checks are wired correctly — a pipeline run says nothing about real
//! model quality.

use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Role, ToolCall, Usage};
use serde_json::{json, Value};
use std::sync::Arc;

fn call(n: usize, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: format!("f{n}-{name}"),
        name: name.into(),
        arguments: args,
    }
}

fn usage(req: &ChatRequest) -> Usage {
    let prompt: usize = req
        .messages
        .iter()
        .map(|m| hatch_core::tokens::estimate_tokens(&m.text_content()))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 40,
        ..Default::default()
    }
}

/// See `workspace/oracle.rs`: the unanswered-approval note is written as an
/// assistant message and must not count as a model step.
const UNANSWERED_NOTE_PREFIX: &str = "The following actions from this turn were submitted for approval";

const BIG_ORDERS: &str = "N-0043\nN-0054\nN-0108\nN-0162\nN-0216\nN-0227\nN-0270\nN-0281\nN-0335\nN-0389\nN-0443\nN-0454\nN-0508\n";

fn respond(req: &ChatRequest) -> Result<ChatResponse, LlmError> {
    let users: Vec<(usize, String)> = req
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, m)| (i, m.text_content()))
        // The completeness check stays within the current scripted turn.
        .filter(|(_, text)| !text.starts_with("(System self-check)\n"))
        .collect();
    let task = users.first().map(|(_, t)| t.clone()).unwrap_or_default();
    let (last_i, last_text) = users.last().cloned().unwrap_or((0, String::new()));
    let harness = last_text.starts_with("审批结果");
    let step = req.messages[last_i + 1..]
        .iter()
        .filter(|m| m.role == Role::Assistant && !m.text_content().starts_with(UNANSWERED_NOTE_PREFIX))
        .count();
    let text = |s: &str| Ok(ChatResponse::text(s));
    let calls = |c: Vec<ToolCall>| Ok(ChatResponse::calls(c));
    let mut r: Result<ChatResponse, LlmError> = if task.contains("notes/log.md") {
        match (harness, step) {
            (false, 0) => calls(vec![call(0, "fs_read", json!({"path": "notes/log.md"}))]),
            (false, 1) => calls(vec![call(
                1,
                "fs_write",
                json!({"path": "notes/log.md", "content": "# 工作日志\n\n- 完成周报\n- 完成月度对账 R-1125\n"}),
            )]),
            (false, _) => text("已提交写入，等你确认。"),
            // The write's outcome is unknown: look before doing anything else.
            (true, 0) => calls(vec![call(2, "fs_read", json!({"path": "notes/log.md"}))]),
            (true, _) => text("写入时响应超时，但回读确认这一行已经写进去了，没有重复追加。"),
        }
    } else if harness {
        text("已完成。")
    } else if task.contains("orders-nov.csv") {
        match step {
            0 => calls(vec![call(0, "fs_read", json!({"path": "data/orders-nov.csv"}))]),
            1 => calls(vec![call(1, "fs_read", json!({"path": "data/orders-nov.csv", "start_line": 401}))]),
            2 => calls(vec![call(2, "fs_write", json!({"path": "reports/big-orders.md", "content": BIG_ORDERS}))]),
            _ => text("共 13 笔订单金额不低于 490，已提交写入。"),
        }
    } else if task.contains("inventory.md") {
        match step {
            // The first two reads fail (503); the third succeeds.
            0..=2 => calls(vec![call(step, "fs_read", json!({"path": "data/inventory.md"}))]),
            3 => calls(vec![call(3, "fs_write", json!({"path": "reports/low-stock.md", "content": "SKU-03\nSKU-06\nSKU-08\n"}))]),
            _ => text("读取时遇到两次临时故障，重试后成功；库存低于 10 的有 SKU-03、SKU-06、SKU-08，已提交写入。"),
        }
    } else if task.contains("payroll.csv") {
        match step {
            0 => calls(vec![
                call(0, "fs_read", json!({"path": "data/rent.csv"})),
                call(1, "fs_read", json!({"path": "data/payroll.csv"})),
            ]),
            1 => calls(vec![call(
                2,
                "fs_write",
                json!({"path": "reports/expenses.md", "content": "# 11 月支出\n\n- 租金合计：32000 元（办公室 25000、仓库 5000、停车位 2000）\n- 工资：无权读取 data/payroll.csv，未计入；需要有权限的人补上。\n"}),
            )]),
            _ => text("租金合计 32000 元已写入；工资文件权限不足、无法读取，没有计入，也没有估算。"),
        }
    } else if task.contains("supplier-prices.md") {
        match step {
            0..=2 => calls(vec![call(step, "fs_read", json!({"path": "data/supplier-prices.md"}))]),
            _ => text("data/supplier-prices.md 连续三次读取都报 503（存储服务暂时不可用），我停下了，没有写 reports/price.md。存储恢复后再让我查一次即可。"),
        }
    } else {
        Err(LlmError::permanent("faults oracle: unknown task"))
    };
    if let Ok(resp) = r.as_mut() {
        resp.usage = usage(req);
    }
    r
}

pub fn faults_oracle() -> Arc<FnModel> {
    Arc::new(FnModel::new("faults-oracle", respond))
}
