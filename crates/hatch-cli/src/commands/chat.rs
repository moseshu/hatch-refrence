//! `hatch chat` / `hatch ask`

use crate::app::App;
use anyhow::Result;
use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_tools::builtin::default_registry;
use std::io::{BufRead, Write};
use std::path::Path;

fn session_id(app: &App, requested: Option<String>, new: bool) -> Result<String> {
    let s = SessionStore::new(&app.db);
    let now = hatch_core::time::now();
    if let Some(id) = requested {
        if !s.exists(&id)? {
            anyhow::bail!("会话 {id} 不存在");
        }
        return Ok(id);
    }
    if !new {
        if let Some(id) = s.latest()? {
            return Ok(id);
        }
    }
    Ok(s.create(&format!("prompt {}", hatch_agent::prompt::prompt_version()), &now)?)
}

fn turn(app: &App, session: &str, text: &str) -> Result<()> {
    let router = app.router(app.default_limits())?;
    let gate = app.policy()?;
    let executors = app.executors(false)?;
    let tools = default_registry();
    let agent = Agent {
        db: &app.db,
        router: &router,
        tools: &tools,
        executors: &executors,
        gate: &gate,
        config: AgentConfig {
            user_name: app.config.user.name.clone(),
            user_email: app.user_email(),
            tz: app.tz,
            max_tokens: app.config.limits.max_tokens,
            ..Default::default()
        },
    };
    let now = hatch_core::time::now();
    let r = agent.run_turn(session, text, &now)?;
    app.record_usage(&router, &now)?;
    match r.stop {
        StopReason::Answered => println!("{}", r.answer.unwrap_or_default()),
        StopReason::StepLimit => println!("（达到单轮步数上限，任务可能未完成；可以继续追问）"),
        StopReason::Budget(m) => println!("（模型调用预算已用尽：{m}）"),
        StopReason::ModelError(m) => println!("（模型调用失败：{m}）"),
    }
    if !r.approvals.is_empty() {
        println!("\n待你确认：{}（`hatch approvals show <id>`）", r.approvals.join(", "));
    }
    Ok(())
}

pub fn ask(home: Option<&Path>, text: &str, session: Option<String>) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, false)?;
    turn(&app, &id, text)
}

pub fn repl(home: Option<&Path>, session: Option<String>, new: bool) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, new)?;
    println!("hatch 会话 {id}。输入 /exit 退出，/approvals 查看待审批。");
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            "/approvals" => {
                for a in hatch_policy::ApprovalQueue::new(&app.db).list(Some(hatch_policy::ApprovalStatus::Pending))? {
                    println!("{}  {}", a.id, a.title);
                }
            }
            text => {
                if let Err(e) = turn(&app, &id, text) {
                    eprintln!("错误：{e:#}");
                }
            }
        }
    }
    Ok(())
}
