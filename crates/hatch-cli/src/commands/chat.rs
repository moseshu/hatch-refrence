//! `hatch chat` / `hatch ask`

use crate::app::App;
use anyhow::Result;
use hatch_agent::{Agent, AgentConfig, SessionStore, StopReason};
use hatch_llm::StreamEvent;
use hatch_tools::builtin::{add_general_tools, default_registry};
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

fn turn(app: &App, session: &str, text: &str, stream: bool, workspace: Option<&Path>) -> Result<()> {
    let router = app.router(app.default_limits())?;
    let gate = app.policy()?;
    let executors = app.executors(false)?;
    let tools = add_general_tools(default_registry(), &app.general_tools(workspace)?);
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
            max_steps: app.config.agent.max_steps,
            ..Default::default()
        },
    };
    let now = hatch_core::time::now();
    let stream = stream || app.config.limits.stream;
    let mut printed = false;
    let r = if stream {
        let mut sink = |e: StreamEvent| match e {
            StreamEvent::TextDelta(t) => {
                printed = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::ToolCallStarted { name } => {
                if printed {
                    println!();
                    printed = false;
                }
                eprintln!("  · 调用工具 {name}");
            }
            StreamEvent::Done => {
                if printed {
                    println!();
                    printed = false;
                }
            }
        };
        agent.run_turn_streaming(session, text, &now, Some(&mut sink))?
    } else {
        agent.run_turn(session, text, &now)?
    };
    app.record_usage(&router, &now)?;
    match r.stop {
        // When streaming, the answer is already on screen.
        StopReason::Answered if stream => {}
        StopReason::Answered => println!("{}", r.answer.unwrap_or_default()),
        StopReason::StepLimit => println!("{}", r.receipt()),
        StopReason::Budget(m) => println!("（模型调用预算已用尽：{m}）"),
        StopReason::ModelError(m) => println!("（模型调用失败：{m}）"),
    }
    if !r.approvals.is_empty() {
        println!("\n待你确认：{}（`hatch approvals show <id>`）", r.approvals.join(", "));
    }
    Ok(())
}

pub fn ask(home: Option<&Path>, text: &str, session: Option<String>, stream: bool, workspace: Option<&Path>) -> Result<()> {
    let app = App::open(home)?;
    let id = session_id(&app, session, false)?;
    turn(&app, &id, text, stream, workspace)
}

pub fn repl(home: Option<&Path>, session: Option<String>, new: bool, stream: bool, workspace: Option<&Path>) -> Result<()> {
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
                if let Err(e) = turn(&app, &id, text, stream, workspace) {
                    eprintln!("错误：{e:#}");
                }
            }
        }
    }
    Ok(())
}
