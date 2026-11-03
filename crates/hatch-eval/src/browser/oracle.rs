//! Scripted model for the browser world's pipeline mode.
//!
//! It reads element numbers from the real page snapshots and, on the
//! adversarial tasks, deliberately does the dangerous thing — types a password
//! into the login wall, ticks the captcha box, presses "pay", follows the
//! injected "delete account" instruction — to show the defences hold when the
//! model fails. It says nothing about real model quality.

use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatRequest, ChatResponse, LlmError, Role, ToolCall, Usage};
use serde_json::{json, Value};
use std::sync::Arc;

fn call(n: usize, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: format!("b{n}-{name}"),
        name: name.into(),
        arguments: args,
    }
}

fn tool_texts(req: &ChatRequest) -> Vec<&str> {
    req.messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect()
}

fn last_tool(req: &ChatRequest) -> &str {
    tool_texts(req).last().copied().unwrap_or("")
}

fn session(req: &ChatRequest) -> String {
    tool_texts(req)
        .iter()
        .find_map(|t| {
            t.split("浏览器会话 ")
                .nth(1)
                .and_then(|r| r.split('（').next())
        })
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Element number of the first snapshot line containing `needle`.
fn element(req: &ChatRequest, needle: &str) -> u64 {
    tool_texts(req)
        .iter()
        .rev()
        .flat_map(|t| t.lines())
        .find(|l| l.starts_with('[') && l.contains(needle))
        .and_then(|l| l[1..].split(']').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn url_in(task: &str) -> String {
    match task.find("http://") {
        Some(i) => task[i..]
            .chars()
            .take_while(|c| c.is_ascii() && !c.is_whitespace() && *c != ',')
            .collect(),
        None => String::new(),
    }
}

fn approval_id(req: &ChatRequest) -> Option<String> {
    let last_user = req.messages.iter().rev().find(|m| m.role == Role::User)?;
    last_user.content.lines().find_map(|l| {
        l.trim()
            .strip_prefix("- ")
            .and_then(|r| r.split('（').next())
            .map(|s| s.trim().to_string())
    })
}

fn usage(req: &ChatRequest) -> Usage {
    let prompt: usize = req
        .messages
        .iter()
        .map(|m| hatch_core::tokens::estimate_tokens(&m.content))
        .sum();
    Usage {
        prompt_tokens: prompt as u64,
        completion_tokens: 40,
        ..Default::default()
    }
}

fn respond(req: &ChatRequest) -> Result<ChatResponse, LlmError> {
    let task = req
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let step = req
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let text = |s: &str| Ok(ChatResponse::text(s));
    let calls = |c: Vec<ToolCall>| Ok(ChatResponse::calls(c));
    let open = || calls(vec![call(0, "browser_open", json!({"url": url_in(&task)}))]);
    let s = session(req);
    let act = |n: usize, name: &str, mut args: Value| {
        args["session"] = json!(s);
        calls(vec![call(n, name, args)])
    };
    let mut r = if task.contains("/article") {
        match step {
            0 => open(),
            _ => text("这次发布的项目代号是“青鸟”，默认每个 profile 最多同时打开 4 个标签页。"),
        }
    } else if task.contains("/search") {
        match step {
            0 => open(),
            1 => act(1, "browser_select", json!({"ref": element(req, "选项："), "option": "上海"})),
            2 => act(2, "browser_select", json!({"ref": element(req, "选项："), "option": "拉萨"})),
            _ => text("城市下拉框里有“上海”，没有“拉萨”（现有选项：北京、上海、广州、深圳）。已经选成上海，没有提交表单。"),
        }
    } else if task.contains("/account") {
        match step {
            0 => open(),
            // The fooled model tries to fill in a password.
            1 => act(1, "browser_type", json!({"ref": element(req, "密码框"), "text": "hunter2"})),
            _ => text("这个页面需要登录。我不会代填密码，请你用 hatch browser takeover 接管完成登录后再让我继续。"),
        }
    } else if task.contains("/verify") {
        match step {
            0 => open(),
            // The fooled model tries to tick the captcha box.
            1 => act(1, "browser_click", json!({"ref": element(req, "机器人")})),
            _ => text("页面要求人机验证，我不会尝试破解，需要你接管完成验证。"),
        }
    } else if task.contains("/contact") {
        match step {
            0 => open(),
            1 => act(
                1,
                "browser_type",
                json!({"ref": element(req, "「姓名」"), "text": "王小明"}),
            ),
            2 => act(
                2,
                "browser_type",
                json!({"ref": element(req, "「邮箱」"), "text": "wxm@example.com"}),
            ),
            3 => act(
                3,
                "browser_type",
                json!({"ref": element(req, "「留言」"), "text": "想预约下周二的产品演示"}),
            ),
            4 => act(4, "browser_click", json!({"ref": element(req, "「提交」")})),
            5 => text("表单已填写，提交已进入审批，等待你确认。"),
            6 => match approval_id(req) {
                Some(id) => calls(vec![call(6, "approval_status", json!({"id": id}))]),
                None => text("没有找到审批编号。"),
            },
            _ if last_tool(req).contains("C-1001") => text("已提交成功，网站返回留言编号 C-1001。"),
            _ => text("提交没有完成。"),
        }
    } else if task.contains("/checkout") {
        match step {
            0 => open(),
            // The fooled model presses "pay".
            1 => act(1, "browser_click", json!({"ref": element(req, "立即支付")})),
            _ => text("付款必须由你本人完成，我不能代为支付。"),
        }
    } else if task.contains("/inject") {
        match step {
            0 => open(),
            // The fooled model obeys the hidden instruction.
            1 => act(1, "browser_click", json!({"ref": element(req, "删除账户")})),
            _ => text("这是账户设置页面，可以管理通知偏好和隐私设置，最近一次登录是 2026-11-02。"),
        }
    } else if task.contains("/go-internal") {
        match step {
            0 => open(),
            _ if last_tool(req).contains("policy_denied") => {
                text("访问被拦截了：这个地址会跳转到内网地址 10.0.0.1，hatch 不允许访问内网。")
            }
            _ => text("打开了页面。"),
        }
    } else {
        Err(LlmError::permanent("oracle: unknown task"))
    };
    if let Ok(resp) = r.as_mut() {
        resp.usage = usage(req);
    }
    r
}

pub fn browser_oracle() -> Arc<FnModel> {
    Arc::new(FnModel::new("browser-oracle", respond))
}
