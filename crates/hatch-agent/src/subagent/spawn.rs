//! Spawn and monitor a sub-agent child process.
//!
//! One child = one `hatch __subagent-run` process. The parent writes the
//! task (one JSON line) to the child's stdin and **keeps stdin open** for
//! the whole run — the child exits when stdin hits EOF, so a parent that
//! dies never leaves an orphan behind. Progress events stream back as
//! JSONL on stdout; the monitor records them in the run store and forwards
//! them to the parent's observer, exactly like the in-process path.
//!
//! Timeout and parent cancellation both kill the child (SIGKILL via
//! [`std::process::Child::kill`]) and are reported as failures, never as
//! a crash of the parent. A [`ChildGuard`] owns the handle and the scratch
//! directory, so every exit path — including panics — cleans up.

use super::process::{ChildEvent, ProcessOutcome, ProcessTask};
use super::store::RunStore;
use super::RunStatus;
use crate::progress::{Observer, Progress, ProgressKind};
use hatch_core::CancelToken;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Owns a child process and its scratch dir; killing and removing on drop.
struct ChildGuard {
    child: Option<Child>,
    scratch: std::path::PathBuf,
}

impl ChildGuard {
    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while c.try_wait().map(|s| s.is_none()).unwrap_or(false) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = c.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn kind_of(s: &str) -> Option<ProgressKind> {
    Some(match s {
        "step" => ProgressKind::Step,
        "tool_call" => ProgressKind::ToolCall,
        "tool_result" => ProgressKind::ToolResult,
        "delegate" => ProgressKind::Delegate,
        "subagent_done" => ProgressKind::SubagentDone,
        "answer" => ProgressKind::Answer,
        "stopped" => ProgressKind::Stopped,
        _ => return None,
    })
}

fn failed(error: String) -> ProcessOutcome {
    ProcessOutcome {
        status: RunStatus::Failed,
        error,
        answer: None,
        steps: 0,
        model_calls: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        tainted: false,
        approvals: vec![],
        consumed_grants: vec![],
        unavailable_tools: vec![],
    }
}

/// Run one child process to a terminal state.
///
/// Returns `Err` only when the child could not be spawned at all — the
/// caller falls back to the in-process path. Every other outcome (timeout,
/// cancel, crash, bad output) is an `Ok` [`ProcessOutcome`] with a
/// terminal status.
pub fn run_child(
    task: &ProcessTask,
    binary: &Path,
    store: &RunStore,
    cancel: &CancelToken,
    observer: Option<&Observer>,
) -> Result<ProcessOutcome, String> {
    let run_id = task.run_id.clone();
    let depth = task.depth;
    std::fs::create_dir_all(&task.scratch_dir).map_err(|e| format!("无法创建子代理工作区：{e}"))?;
    let mut child = Command::new(binary)
        .arg("__subagent-run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "无法启动子代理进程（{binary}）：{e}",
                binary = binary.display()
            )
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let line = serde_json::to_string(task).map_err(|e| format!("任务无法序列化：{e}"))?;
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|e| {
            let _ = child.kill();
            let _ = child.wait();
            format!("无法把任务写入子代理进程：{e}")
        })?;
    let mut guard = ChildGuard {
        child: Some(child),
        scratch: task.scratch_dir.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(task.timeout_secs.max(1));
    let (tx, rx) = channel::<String>();
    let mut result: Option<ProcessOutcome> = None;
    let mut failure: Option<String> = None;
    let stderr_tail = std::sync::Mutex::new(String::new());
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut r = BufReader::new(stdout);
            loop {
                let mut buf = String::new();
                match r.read_line(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(buf).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        s.spawn(|| {
            let mut r = BufReader::new(stderr);
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf);
            let tail: String = text
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            *stderr_tail.lock().unwrap() = tail;
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    let line = line.trim_end();
                    if line.is_empty() {
                        continue;
                    }
                    let ev: ChildEvent = match serde_json::from_str(line) {
                        Ok(e) => e,
                        Err(e) => {
                            failure = Some(format!("子代理输出了无法解析的一行：{e}"));
                            break;
                        }
                    };
                    match ev {
                        ChildEvent::Progress { kind, detail } => {
                            let _ = store.event(&run_id, &kind, &detail);
                            if let (Some(o), Some(k)) = (observer, kind_of(&kind)) {
                                o(&Progress {
                                    depth,
                                    agent: run_id.clone(),
                                    kind: k,
                                    detail,
                                });
                            }
                        }
                        ChildEvent::Result(r) => {
                            let status = match r.status.as_str() {
                                "completed" => RunStatus::Completed,
                                "cancelled" => RunStatus::Cancelled,
                                _ => RunStatus::Failed,
                            };
                            result = Some(ProcessOutcome {
                                status,
                                error: r.error,
                                answer: r.answer,
                                steps: r.steps,
                                model_calls: r.model_calls,
                                prompt_tokens: r.prompt_tokens,
                                completion_tokens: r.completion_tokens,
                                tainted: r.tainted,
                                approvals: r.approvals,
                                consumed_grants: r.consumed_grants,
                                unavailable_tools: r.unavailable_tools,
                            });
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        failure = Some(format!(
                            "父任务已取消：{}",
                            cancel.reason().unwrap_or_else(|| "cancelled".into())
                        ));
                        guard.kill();
                        break;
                    }
                    if Instant::now() >= deadline {
                        failure = Some(format!("子代理运行超过 {} 秒，已终止", task.timeout_secs));
                        guard.kill();
                        break;
                    }
                    if let Some(c) = guard.child.as_mut() {
                        if let Ok(Some(status)) = c.try_wait() {
                            if result.is_none() && failure.is_none() {
                                failure = Some(match status.code() {
                                    Some(code) => format!("子代理进程异常退出（exit code {code}）"),
                                    None => "子代理进程被信号终止".into(),
                                });
                            }
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Stdout closed: the child is gone (or done). If no
                    // result arrived, its exit status says why.
                    if result.is_none() && failure.is_none() {
                        if let Some(c) = guard.child.as_mut() {
                            match c.try_wait() {
                                Ok(Some(status)) => {
                                    failure = Some(match status.code() {
                                        Some(code) => format!(
                                            "子代理进程异常退出（exit code {code}），未返回结果"
                                        ),
                                        None => "子代理进程被信号终止，未返回结果".into(),
                                    });
                                }
                                _ => failure = Some("子代理输出已关闭，未返回结果".into()),
                            }
                        }
                    }
                    break;
                }
            }
        }
    });
    // The child is done or dead: close stdin (its EOF watchdog exits it
    // if it is somehow still alive), then reap it.
    drop(stdin);
    if let Some(mut c) = guard.child.take() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.try_wait().map(|s| s.is_none()).unwrap_or(false) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if c.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            let _ = c.kill();
        }
        let _ = c.wait();
    }
    let _ = std::fs::remove_dir_all(&guard.scratch);
    if let Some(mut o) = result {
        if cancel.is_cancelled() && o.status != RunStatus::Completed {
            o.status = RunStatus::Cancelled;
            o.error = cancel.reason().unwrap_or_else(|| "cancelled".into());
        }
        return Ok(o);
    }
    if cancel.is_cancelled() {
        let mut o = failed(failure.unwrap_or_else(|| "父任务已取消".into()));
        o.status = RunStatus::Cancelled;
        return Ok(o);
    }
    let mut msg = failure.unwrap_or_else(|| "子代理未返回结果".into());
    let tail = stderr_tail.lock().unwrap();
    if !tail.trim().is_empty() && !msg.contains("exit code") {
        msg.push_str(&format!("；子代理 stderr：{tail}"));
    } else if !tail.trim().is_empty() {
        msg.push_str(&format!("；stderr：{tail}"));
    }
    Ok(failed(msg))
}
