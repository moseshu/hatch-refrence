//! `hatch task` — durable tasks (M0 item 6, M1 extended).
//!
//! - `task create [--kind K] [--max-model-calls N]` — create a task
//!   (queued).
//! - `task show <id>` — task row + plan version + budget + steps
//!   (with approval/receipt ids) + recent events. The plan card
//!   (task 5): goal, steps with expected + current step + done marks,
//!   per-step receipt/evidence, plan version, approvals with status,
//!   blocked reason.
//! - `task list [--status S] [--limit N]` — the ledger projection
//!   (task 5): id, kind, title, status, plan version, step progress,
//!   pending approvals, lease. A task appears exactly once.
//! - `task pause <id> [--note N]` — pause (task 5): the task stops
//!   claiming new steps and stays resumable, unlike cancel.
//! - `task edit-step <id> --step S --expected-version V [--title T]
//!   [--tool T] [--arguments JSON] [--expected E]` — edit a pending
//!   step (task 5) through the §10.2 version mechanism.
//! - `task events <id> [--after <seq>] [--limit <n>]` — the event log
//!   from a cursor (a reconnecting client resumes here).
//! - `task run --world <dir> --out <dir> [--task <id>] [--kind K]
//!   [--live] [--max-model-calls N]` — run the worker on this host's
//!   database until the task completes, pauses on an approval, or
//!   pauses on its budget. The client may exit while it runs: the
//!   task lives in the database, and `task show` / `task events`
//!   read its progress back.

use super::task_control::{apply_command, cancel_task, pause_task};
use super::task_edit::edit_step;
use super::task_gmail::{run_worker_gmail, GmailInviteArgs, GmailRun};
use super::task_kinds::{is_known_kind, DEFAULT_KIND, KNOWN_KINDS};
use super::task_view::{detail, list_summaries, render_detail, render_summary_line};
use super::task_worker::{run_worker, WorkerOpts};
use crate::app::App;
use crate::config::home_dir;
use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use hatch_core::time::now as now_ts;
use hatch_tasks::{EventLog, StepEdit, TaskStatus, TaskStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The `Task` variant holds `TaskCmd`, whose `Run` variant carries
/// many CLI flags (the Gmail pilot's scoped options) — the size
/// spread is expected, not a design smell.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum TaskCmd {
    /// Create a task (queued, not yet claimed).
    Create {
        /// Human-readable title.
        #[arg(long)]
        title: String,
        /// Task kind (default `meeting-brief`).
        #[arg(long)]
        kind: Option<String>,
        /// Model-call budget (0 = unlimited, default 0).
        #[arg(long, default_value_t = 0)]
        max_model_calls: u64,
    },
    /// Show a task: status, lease, steps, recent events.
    Show {
        /// Task id (`tsk_…`).
        id: String,
    },
    /// List tasks (task 5): the ledger projection — id, kind, title,
    /// status, plan version, step progress, pending approvals, lease.
    /// A task appears exactly once; a duplicate row is a bug.
    List {
        /// Only tasks in this status (queued/running/waiting/blocked/
        /// done/failed/cancelled).
        #[arg(long)]
        status: Option<String>,
        /// Maximum tasks to return (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List events for a task from a sequence cursor.
    Events {
        /// Task id (`tsk_…`).
        id: String,
        /// Only events with `seq` greater than this (default 0).
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum events to return (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Apply a versioned mid-task command (§10.2): the command
    /// carries the plan version the user saw; a stale version is
    /// rejected as expired, and a successful one bumps the version
    /// and invalidates older approvals.
    Command {
        /// Task id (`tsk_…`).
        id: String,
        /// The plan version the user saw when issuing the command.
        #[arg(long)]
        expected_version: u64,
        /// What changed (recorded, not applied to the steps).
        #[arg(long)]
        summary: String,
    },
    /// Cancel a task (the worker stops at the next step boundary;
    /// the task's pending approvals are invalidated).
    Cancel {
        /// Task id (`tsk_…`).
        id: String,
        /// Why it was cancelled.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Pause a task (task 5): it stops claiming new steps at the
    /// next step boundary and stays readable as blocked/paused;
    /// resume with `task run --task`. Unlike cancel, not terminal.
    Pause {
        /// Task id (`tsk_…`).
        id: String,
        /// Why it was paused.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Edit a pending step (task 5): change a step that has not run
    /// yet (title/tool/arguments/expected). The command carries the
    /// plan version the user saw; a stale version is rejected, a
    /// successful edit bumps the version and invalidates old
    /// approvals. Editing a running/done/failed step is refused.
    EditStep {
        /// Task id (`tsk_…`).
        id: String,
        /// Step id.
        #[arg(long)]
        step: String,
        /// The plan version the user saw when issuing the command.
        #[arg(long)]
        expected_version: u64,
        /// New title (omit = leave unchanged).
        #[arg(long)]
        title: Option<String>,
        /// New tool (omit = leave unchanged).
        #[arg(long)]
        tool: Option<String>,
        /// New arguments as a JSON object (omit = leave unchanged).
        #[arg(long)]
        arguments: Option<String>,
        /// New acceptance (omit = leave unchanged).
        #[arg(long)]
        expected: Option<String>,
    },
    /// Run the worker for a task (or a fresh task of `--kind`).
    Run {
        /// Eval world directory (e.g. `eval/worlds/meeting-brief`).
        /// Fixture mode only; omit it with `--source gmail`.
        #[arg(long)]
        world: Option<PathBuf>,
        /// Output directory for the brief artifact.
        #[arg(long)]
        out: PathBuf,
        /// Resume this task instead of creating a new one.
        #[arg(long)]
        task: Option<String>,
        /// Task kind for a fresh task (default `meeting-brief`).
        #[arg(long)]
        kind: Option<String>,
        /// Run the steps on the real model (same env vars as
        /// `hatch eval --live`); default is the scripted model.
        #[arg(long)]
        live: bool,
        /// Model-call budget for this run (0 = the task's stored
        /// budget, 0 on a fresh task = unlimited).
        #[arg(long, default_value_t = 0)]
        max_model_calls: u64,
        /// Run the worker detached: return immediately, write the
        /// worker's output to a log file in the output dir, and read
        /// progress with `task show` / `task events`.
        #[arg(long)]
        background: bool,
        /// Lease TTL in seconds (default 300).
        #[arg(long, default_value_t = 300)]
        lease_ttl_secs: u64,
        /// Mail source: `fixture` (an eval world) or `gmail` (the
        /// real account; needs `--gmail-user`).
        #[arg(long, default_value = "fixture")]
        source: String,
        /// Gmail search query for the sync (gmail mode only).
        #[arg(long, default_value = "is:unread")]
        gmail_query: String,
        /// How many recent messages to sync (gmail mode, 1..=20).
        #[arg(long, default_value_t = 10)]
        gmail_max: u64,
        /// The real account's address (gmail mode, required).
        #[arg(long)]
        gmail_user: Option<String>,
        /// The account owner's display name (gmail mode).
        #[arg(long)]
        gmail_name: Option<String>,
        /// IANA timezone for dates (gmail mode).
        #[arg(long, default_value = "Asia/Shanghai")]
        timezone: String,
        /// Task title override (gmail mode; default derives from kind).
        #[arg(long)]
        title: Option<String>,
        /// Gather instruction override (gmail mode).
        #[arg(long)]
        prompt: Option<String>,
        /// Invite: meeting title (gmail invite kind, required).
        #[arg(long)]
        invite_title: Option<String>,
        /// Invite: start "YYYY-MM-DD HH:MM" (gmail invite kind).
        #[arg(long)]
        invite_start: Option<String>,
        /// Invite: attendee address (gmail invite kind, required).
        #[arg(long)]
        invite_to: Option<String>,
        /// Invite: attendee display name (gmail invite kind).
        #[arg(long)]
        invite_name: Option<String>,
        /// Invite: meeting location (gmail invite kind).
        #[arg(long)]
        invite_location: Option<String>,
        /// Invite: 议题 line (gmail invite kind).
        #[arg(long)]
        invite_topics: Option<String>,
    },
}

pub fn run(home: Option<&Path>, cmd: TaskCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = TaskStore::new(&app.db);
    let now = now_ts();
    match cmd {
        TaskCmd::Create {
            title,
            kind,
            max_model_calls,
        } => {
            let kind = kind.unwrap_or_else(|| DEFAULT_KIND.into());
            if !is_known_kind(&kind) {
                return Err(anyhow!(
                    "未知任务 kind {kind}（已知 kind：{}）",
                    KNOWN_KINDS.join("、")
                ));
            }
            let task = store.create(&kind, &title, "cli", "", 3, None, &now)?;
            if max_model_calls > 0 {
                store.set_budget(&task.id, max_model_calls, &now)?;
            }
            println!("task {}  queued", task.id);
            println!("kind: {}", task.kind);
            println!("title: {}", task.title);
            if max_model_calls > 0 {
                println!("budget: {max_model_calls} 次模型调用");
            }
            Ok(())
        }
        TaskCmd::Show { id } => {
            let d = detail(&app.db, &id)?;
            print!("{}", render_detail(&d));
            let log = EventLog::new(&app.db);
            for ev in log.events_since(&id, 0, 20)? {
                println!("  #{} {:<16} {} {}", ev.seq, ev.kind, ev.step_id, ev.detail);
            }
            Ok(())
        }
        TaskCmd::List { status, limit } => {
            let status = match status {
                Some(s) => Some(match s.as_str() {
                    "queued" => TaskStatus::Queued,
                    "running" => TaskStatus::Running,
                    "waiting" => TaskStatus::Waiting,
                    "blocked" => TaskStatus::Blocked,
                    "done" => TaskStatus::Done,
                    "failed" => TaskStatus::Failed,
                    "cancelled" => TaskStatus::Cancelled,
                    _ => return Err(anyhow!(
                        "未知任务状态 {s}（queued/running/waiting/blocked/done/failed/cancelled）"
                    )),
                }),
                None => None,
            };
            let summaries = list_summaries(&app.db, status, limit)?;
            for s in &summaries {
                println!("{}", render_summary_line(s));
            }
            println!("{} 个任务", summaries.len());
            Ok(())
        }
        TaskCmd::Events { id, after, limit } => {
            let log = EventLog::new(&app.db);
            for ev in log.events_since(&id, after, limit)? {
                println!("{} {:<16} {} {}", ev.seq, ev.kind, ev.step_id, ev.detail);
            }
            Ok(())
        }
        TaskCmd::Command {
            id,
            expected_version,
            summary,
        } => {
            let msg = apply_command(&app.db, &id, expected_version, &summary, "cli", &now)?;
            println!("{msg}");
            Ok(())
        }
        TaskCmd::Cancel { id, note } => {
            let msg = cancel_task(&app.db, &id, &note, &now)?;
            println!("{msg}");
            Ok(())
        }
        TaskCmd::Pause { id, note } => {
            let msg = pause_task(&app.db, &id, &note, &now)?;
            println!("{msg}");
            Ok(())
        }
        TaskCmd::EditStep {
            id,
            step,
            expected_version,
            title,
            tool,
            arguments,
            expected,
        } => {
            let arguments = match arguments {
                Some(a) => {
                    let v: serde_json::Value =
                        serde_json::from_str(&a).context("arguments 必须是 JSON 对象")?;
                    if !v.is_object() {
                        return Err(anyhow!("arguments 必须是 JSON 对象"));
                    }
                    Some(v)
                }
                None => None,
            };
            if title.is_none() && tool.is_none() && arguments.is_none() && expected.is_none() {
                return Err(anyhow!(
                    "没有要改的内容（至少给一个 --title/--tool/--arguments/--expected）"
                ));
            }
            let edit = StepEdit {
                title,
                tool,
                arguments,
                expected,
            };
            let msg = edit_step(&app.db, &id, &step, expected_version, &edit, "cli", &now)?;
            println!("{msg}");
            Ok(())
        }
        TaskCmd::Run {
            world,
            out,
            task,
            kind,
            live,
            max_model_calls,
            background,
            lease_ttl_secs,
            source,
            gmail_query,
            gmail_max,
            gmail_user,
            gmail_name,
            timezone,
            title,
            prompt,
            invite_title,
            invite_start,
            invite_to,
            invite_name,
            invite_location,
            invite_topics,
        } => {
            // M2: `--source gmail` runs the worker on the real
            // account instead of a fixture world; the default stays
            // the fixture, so CI makes zero real calls.
            let gmail = match source.as_str() {
                "fixture" => false,
                "gmail" => true,
                other => return Err(anyhow!("未知来源 {other}（fixture/gmail）")),
            };
            if gmail && world.is_some() {
                return Err(anyhow!("--world 与 --source gmail 不能同时用"));
            }
            if !gmail && world.is_none() {
                return Err(anyhow!("fixture 模式需要 --world <dir>"));
            }
            let gmail_run = if gmail {
                Some(gmail_run_from_flags(
                    kind.as_deref(),
                    gmail_query.clone(),
                    gmail_max,
                    gmail_user.clone(),
                    gmail_name.clone(),
                    timezone.clone(),
                    title.clone(),
                    prompt.clone(),
                    invite_title.clone(),
                    invite_start.clone(),
                    invite_to.clone(),
                    invite_name.clone(),
                    invite_location.clone(),
                    invite_topics.clone(),
                )?)
            } else {
                None
            };
            // The rerun hint (and the background child) needs the
            // same mode flags this run used.
            let rerun_base = if gmail {
                format!(
                    "hatch task run --source gmail --gmail-user {} --out <dir>",
                    gmail_user.as_deref().unwrap_or("")
                )
            } else {
                "hatch task run --world <dir> --out <dir>".to_string()
            };
            // M1: `--background` spawns a detached child running the
            // same command in the foreground and returns immediately;
            // the child's stdout/stderr go to a log file in the output
            // dir, and progress is read with `task show` / `task events`.
            if background {
                let mut gmail_argv: Vec<String> = Vec::new();
                if gmail {
                    gmail_argv.push("--source".into());
                    gmail_argv.push("gmail".into());
                    gmail_argv.push("--gmail-query".into());
                    gmail_argv.push(gmail_query);
                    gmail_argv.push("--gmail-max".into());
                    gmail_argv.push(gmail_max.to_string());
                    if let Some(u) = &gmail_user {
                        gmail_argv.push("--gmail-user".into());
                        gmail_argv.push(u.clone());
                    }
                    if let Some(n) = &gmail_name {
                        gmail_argv.push("--gmail-name".into());
                        gmail_argv.push(n.clone());
                    }
                    gmail_argv.push("--timezone".into());
                    gmail_argv.push(timezone);
                    if let Some(t) = &title {
                        gmail_argv.push("--title".into());
                        gmail_argv.push(t.clone());
                    }
                    if let Some(p) = &prompt {
                        gmail_argv.push("--prompt".into());
                        gmail_argv.push(p.clone());
                    }
                    if let Some(v) = &invite_title {
                        gmail_argv.push("--invite-title".into());
                        gmail_argv.push(v.clone());
                    }
                    if let Some(v) = &invite_start {
                        gmail_argv.push("--invite-start".into());
                        gmail_argv.push(v.clone());
                    }
                    if let Some(v) = &invite_to {
                        gmail_argv.push("--invite-to".into());
                        gmail_argv.push(v.clone());
                    }
                    if let Some(v) = &invite_name {
                        gmail_argv.push("--invite-name".into());
                        gmail_argv.push(v.clone());
                    }
                    if let Some(v) = &invite_location {
                        gmail_argv.push("--invite-location".into());
                        gmail_argv.push(v.clone());
                    }
                    if let Some(v) = &invite_topics {
                        gmail_argv.push("--invite-topics".into());
                        gmail_argv.push(v.clone());
                    }
                }
                let msg = spawn_background(
                    home,
                    world.as_deref(),
                    &out,
                    task,
                    kind,
                    live,
                    max_model_calls,
                    lease_ttl_secs,
                    &gmail_argv,
                )?;
                println!("{msg}");
                return Ok(());
            }
            // M1: `--live` runs the steps on the real model (the same
            // env vars as `hatch eval --live`); the default stays the
            // scripted model, so CI makes zero real calls.
            let router = if live {
                Some(Arc::new(app.router(app.default_limits())?))
            } else {
                None
            };
            let opts = WorkerOpts {
                task_id: task,
                kind,
                lease_ttl_secs,
                now_override: None,
                kill: Default::default(),
                router,
                max_model_calls: if max_model_calls > 0 {
                    Some(max_model_calls)
                } else {
                    None
                },
            };
            let report = match gmail_run {
                Some(run) => run_worker_gmail(&app.db, &run, &out, opts)?,
                None => run_worker(&app.db, world.as_deref().unwrap(), &out, opts)?,
            };
            if report.cancelled {
                println!("task {} 已取消", report.task_id);
            } else if report.paused {
                println!(
                    "task {} 已暂停：{} / {} 步（用户暂停）",
                    report.task_id, report.steps_done, report.steps_total
                );
                println!("继续运行：{rerun_base} --task {}", report.task_id);
            } else if let Some(reason) = &report.blocked {
                println!(
                    "task {} 已暂停：{} / {} 步",
                    report.task_id, report.steps_done, report.steps_total
                );
                println!("{reason}");
                println!("批准后重新运行：{rerun_base} --task {}", report.task_id);
            } else if report.budget_stopped {
                println!(
                    "task {} 已暂停：超出模型调用预算（已完成 {} / {} 步，已完成部分见 task show）",
                    report.task_id, report.steps_done, report.steps_total
                );
                println!(
                    "用更大的预算继续：{rerun_base} --task {} --max-model-calls <N>",
                    report.task_id
                );
            } else {
                println!(
                    "task {} 完成：{} / {} 步",
                    report.task_id, report.steps_done, report.steps_total
                );
            }
            println!(
                "模型调用 {} 次，{} prompt / {} completion tokens，耗时 {} ms",
                report.model_calls,
                report.prompt_tokens,
                report.completion_tokens,
                report.duration_ms
            );
            if let Some(a) = report.artifact {
                println!("简报：{}", a.display());
            }
            Ok(())
        }
    }
}

/// Build the `GmailRun` for `--source gmail`: validate the required
/// flags and fill the title/prompt defaults from the kind.
#[allow(clippy::too_many_arguments)]
fn gmail_run_from_flags(
    kind: Option<&str>,
    gmail_query: String,
    gmail_max: u64,
    gmail_user: Option<String>,
    gmail_name: Option<String>,
    timezone: String,
    title: Option<String>,
    prompt: Option<String>,
    invite_title: Option<String>,
    invite_start: Option<String>,
    invite_to: Option<String>,
    invite_name: Option<String>,
    invite_location: Option<String>,
    invite_topics: Option<String>,
) -> Result<GmailRun> {
    if !(1..=20).contains(&gmail_max) {
        return Err(anyhow!(
            "--gmail-max 须在 1..=20 之间（试点一次最多 20 封）"
        ));
    }
    let user_email =
        gmail_user.ok_or_else(|| anyhow!("--source gmail 需要 --gmail-user <邮箱>"))?;
    let user_name = gmail_name.unwrap_or_else(|| user_email.clone());
    let is_invite = kind == Some("meeting-invite");
    let invite = if is_invite {
        let event_title =
            invite_title.ok_or_else(|| anyhow!("会议邀请需要 --invite-title <标题>"))?;
        let event_start = invite_start
            .ok_or_else(|| anyhow!("会议邀请需要 --invite-start <YYYY-MM-DD HH:MM>"))?;
        let attendee_email = invite_to.ok_or_else(|| anyhow!("会议邀请需要 --invite-to <邮箱>"))?;
        Some(GmailInviteArgs {
            event_title,
            event_start,
            location: invite_location.unwrap_or_default(),
            attendee_name: invite_name.unwrap_or_else(|| attendee_email.clone()),
            attendee_email,
            topics: invite_topics.unwrap_or_default(),
        })
    } else {
        None
    };
    let title = title.unwrap_or_else(|| {
        if is_invite {
            format!(
                "邀请：{}",
                invite
                    .as_ref()
                    .map(|i| i.event_title.as_str())
                    .unwrap_or("")
            )
        } else {
            "简报：最近邮件".to_string()
        }
    });
    let prompt =
        prompt.unwrap_or_else(|| "把最近邮件里的决定整理成简报，每条注明来源邮件".to_string());
    Ok(GmailRun {
        user_email,
        user_name,
        timezone,
        query: gmail_query,
        max: gmail_max,
        title,
        prompt,
        invite,
    })
}

/// Spawn a detached copy of this binary running `task run` in the
/// foreground, with its output in a log file under the output dir
/// (M1: minimal background mode, not a daemon). The child shares the
/// same home (and therefore the same database); detaching only drops
/// the handle, it does not kill the child.
#[allow(clippy::too_many_arguments)]
fn spawn_background(
    home: Option<&Path>,
    world: Option<&Path>,
    out: &Path,
    task: Option<String>,
    kind: Option<String>,
    live: bool,
    max_model_calls: u64,
    lease_ttl_secs: u64,
    extra_argv: &[String],
) -> Result<String> {
    let exe = std::env::current_exe().context("找不到当前可执行文件")?;
    let home_path = home_dir(home);
    std::fs::create_dir_all(out).context("创建输出目录失败")?;
    let log_name = match &task {
        Some(id) => format!("worker-{id}.log"),
        None => "worker.log".to_string(),
    };
    let log_path = out.join(log_name);
    let log = std::fs::File::create(&log_path).context("创建后台日志失败")?;
    let log_err = log.try_clone().context("复制日志句柄失败")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home").arg(&home_path).arg("task").arg("run");
    if let Some(w) = world {
        cmd.arg("--world").arg(w);
    }
    cmd.arg("--out")
        .arg(out)
        .arg("--lease-ttl-secs")
        .arg(lease_ttl_secs.to_string());
    for a in extra_argv {
        cmd.arg(a);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    if let Some(id) = &task {
        cmd.arg("--task").arg(id);
    }
    if let Some(k) = &kind {
        cmd.arg("--kind").arg(k);
    }
    if live {
        cmd.arg("--live");
    }
    if max_model_calls > 0 {
        cmd.arg("--max-model-calls")
            .arg(max_model_calls.to_string());
    }
    let child = cmd.spawn().context("启动后台 worker 失败")?;
    let pid = child.id();
    // Detach: the child keeps running after the handle is dropped
    // (`kill_on_drop` is not set), and its output stays in the log.
    drop(child);
    let target = match &task {
        Some(id) => format!("task {id}"),
        None => "新任务（id 见日志第一行）".to_string(),
    };
    Ok(format!(
        "后台 worker 已启动（pid {pid}）：{target}；日志：{}；进度：hatch task show <id> / hatch task events <id>",
        log_path.display()
    ))
}
