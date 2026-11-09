//! Spawn and monitor a sub-agent child process over a Unix socket.
//!
//! One child = one `hatch __subagent-run` process. The parent binds a
//! Unix socket at `<scratch>/subagent.sock` (mode 0600, inside the run's
//! scratch dir so it vanishes with the dir) and spawns the child with
//! `HATCH_SUBAGENT_SOCKET` set; the child connects, the parent writes
//! the task (one JSON line), and progress events stream back as JSONL.
//! Message shapes are unchanged from the old stdin/stdout transport —
//! only the carrier moved.
//!
//! The child watches the socket for EOF on a watchdog thread, so a
//! parent that dies closes the socket and the child exits (130) instead
//! of orphaning.
//!
//! Tokio lives only here: each [`LiveChild::wait`] builds a throwaway
//! current-thread runtime and drives the session on it; the turn loop
//! stays synchronous and `spawn_child` binds with the standard library.
//! Timeout and cancellation both kill the child (SIGKILL) and are
//! reported as failures. A [`ChildGuard`] owns the handle and the
//! scratch dir, so every exit path — including panics — cleans up.

use super::process::{ChildEvent, ProcessOutcome, ProcessTask};
use super::retry::{spawn_child, SpawnFailure};
use super::store::RunStore;
use super::RunStatus;
use crate::progress::{Observer, Progress, ProgressKind};
use hatch_core::CancelToken;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::UnixListener;
use tokio::time::timeout;

/// The scratch dir of a run: `<temp>/hatch-sa-<run_id>`. Owned by the
/// parent process; the child's socket, database and outbox live here.
/// The parent's [`ChildGuard`](ChildGuard) removes it when the child
/// ends; reconciliation removes it for runs whose owner died.
pub(crate) fn scratch_dir(run_id: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hatch-sa-{run_id}"))
}

/// Owns a child process and its scratch dir; killing and removing on drop.
pub(crate) struct ChildGuard {
    pub(crate) child: Option<Child>,
    pub(crate) scratch: std::path::PathBuf,
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

/// How a monitor session ended.
enum SessionEnd {
    Result(ProcessOutcome),
    Failed(String),
}

fn no_result_reason(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("子代理进程异常退出（exit code {code}），未返回结果"),
        None => "子代理进程被信号终止，未返回结果".into(),
    }
}

/// The child's exit status, polling briefly: its socket closes a hair
/// before the task becomes a zombie, so a one-shot `try_wait` can miss it.
async fn wait_exit(guard: &mut ChildGuard) -> Option<std::process::ExitStatus> {
    for _ in 0..50 {
        let s = guard.child.as_mut().and_then(|c| c.try_wait().ok()).flatten();
        if s.is_some() {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// One monitor session: accept, send the task, read events until the
/// result arrives, the child dies, the run times out, or the parent cancels.
/// While `is_paused` is true the run's timeout clock is frozen: the
/// deadline is pushed forward every iteration, so a pause never times
/// out and the full timeout is available again on resume. Cancelling
/// still works while paused (killing a stopped process works).
#[allow(clippy::too_many_arguments)]
async fn run_session(
    listener: UnixListener,
    task_json: &str,
    guard: &mut ChildGuard,
    store: &RunStore<'_>,
    cancel: &CancelToken,
    observer: Option<&Observer<'_>>,
    run_id: &str,
    depth: usize,
    timeout_secs: u64,
    is_paused: &dyn Fn() -> bool,
) -> SessionEnd {
    // A child that never connects is a failed spawn, not a hung turn.
    let stream = match timeout(Duration::from_secs(30), listener.accept()).await {
        Ok(Ok((s, _))) => s,
        Ok(Err(e)) => return SessionEnd::Failed(format!("接受子代理连接失败：{e}")),
        Err(_) => {
            guard.kill();
            return SessionEnd::Failed("子代理 30 秒内没有连接".into());
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let send = async {
        write_half.write_all(task_json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await
    }
    .await;
    if let Err(e) = send {
        guard.kill();
        return SessionEnd::Failed(format!("无法把任务写入子代理：{e}"));
    }
    // `write_half` stays alive until this fn returns, so the child's watchdog sees EOF only when the parent goes away.
    let mut deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut reader = TokioBufReader::new(read_half);
    let mut buf = String::new();
    loop {
        if is_paused() {
            // Frozen, not dead: the timeout bounds active time only.
            deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
        }
        if cancel.is_cancelled() {
            guard.kill();
            return SessionEnd::Failed(format!(
                "父任务已取消：{}",
                cancel.reason().unwrap_or_else(|| "cancelled".into())
            ));
        }
        if Instant::now() >= deadline {
            guard.kill();
            return SessionEnd::Failed(format!("子代理运行超过 {timeout_secs} 秒，已终止"));
        }
        buf.clear();
        tokio::select! {
            r = reader.read_line(&mut buf) => {
                match r {
                    Ok(0) => {
                        // Socket closed: the child is exiting (or gone).
                        let why = wait_exit(guard)
                            .await
                            .map(no_result_reason)
                            .unwrap_or_else(|| "子代理输出已关闭，未返回结果".into());
                        return SessionEnd::Failed(why);
                    }
                    Ok(_) => {
                        let line = buf.trim_end();
                        if line.is_empty() {
                            continue;
                        }
                        let ev: ChildEvent = match serde_json::from_str(line) {
                            Ok(e) => e,
                            Err(e) => return SessionEnd::Failed(format!("子代理输出了无法解析的一行：{e}")),
                        };
                        match ev {
                            ChildEvent::Progress { kind, detail } => {
                                let _ = store.event(run_id, &kind, &detail);
                                if let (Some(o), Some(k)) = (observer, kind_of(&kind)) {
                                    o(&Progress {
                                        depth,
                                        agent: run_id.to_string(),
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
                                return SessionEnd::Result(ProcessOutcome {
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
                            }
                        }
                    }
                    Err(e) => return SessionEnd::Failed(format!("读取子代理输出失败：{e}")),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                // Quiet for a while: check whether the child died on its own.
                if let Some(c) = guard.child.as_mut() {
                    if let Ok(Some(status)) = c.try_wait() {
                        return SessionEnd::Failed(no_result_reason(status));
                    }
                }
            }
        }
    }
}

/// Run one child process to a terminal state. Returns `Err` only when
/// the child could not be spawned at all — the caller falls back to the
/// in-process path. Every other outcome is an `Ok` [`ProcessOutcome`].
/// `is_paused` is never true on this path (no registry owns the child),
/// so the synchronous run cannot be paused.
pub fn run_child(
    task: &ProcessTask,
    binary: &Path,
    store: &RunStore,
    cancel: &CancelToken,
    observer: Option<&Observer>,
) -> Result<ProcessOutcome, SpawnFailure> {
    let live = spawn_child(task, binary)?;
    Ok(live.wait(store, cancel, observer, &|| false))
}

/// A spawned child process, not yet waited on. Split from [`run_child`]
/// so async delegation can spawn in the turn thread (fast — spawn
/// failure is still known synchronously and the caller falls back to
/// the in-process path) and wait in a monitor thread, while the
/// synchronous path just does both in a row. The socket is bound with
/// the standard library in [`spawn_child`]; [`LiveChild::wait`] adopts
/// it into a throwaway current-thread Tokio runtime and drives the session.
pub struct LiveChild {
    pub(crate) run_id: String,
    pub(crate) depth: usize,
    pub(crate) timeout_secs: u64,
    pub(crate) guard: ChildGuard,
    pub(crate) listener: std::os::unix::net::UnixListener,
    pub(crate) task_json: String,
    pub(crate) stderr: std::process::ChildStderr,
}

impl LiveChild {
    /// The child process pid, for restart reconciliation.
    pub fn pid(&self) -> u32 {
        self.guard.child.as_ref().map(|c| c.id()).unwrap_or(0)
    }

    /// Wait for the child to a terminal state: accept its socket, send
    /// the task, read its events, kill it on cancel/timeout, reap it and
    /// remove its scratch dir. Consumes the child. `is_paused` freezes
    /// the run's timeout clock while the run is paused (D5); the async
    /// monitor thread passes the registry check, the synchronous path
    /// passes a closure that is always false.
    pub fn wait(
        self,
        store: &RunStore,
        cancel: &CancelToken,
        observer: Option<&Observer>,
        is_paused: &dyn Fn() -> bool,
    ) -> ProcessOutcome {
        let LiveChild { run_id, depth, timeout_secs, mut guard, listener, task_json, stderr } = self;
        // Drain stderr on its own thread (a chatty child must never block
        // on a full pipe); keep only the tail for failure reports.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = Arc::clone(&stderr_tail);
        let drain = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = BufReader::new(stderr).read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf);
            let t: String = text.chars().rev().take(500).collect::<String>().chars().rev().collect();
            *tail.lock().unwrap() = t;
        });
        // One throwaway current-thread runtime for this session; adopt
        // the std listener into Tokio inside it.
        let end = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(async {
                match UnixListener::from_std(listener) {
                    Ok(tokio_listener) => {
                        run_session(
                            tokio_listener,
                            &task_json,
                            &mut guard,
                            store,
                            cancel,
                            observer,
                            &run_id,
                            depth,
                            timeout_secs,
                            is_paused,
                        )
                        .await
                    }
                    Err(e) => SessionEnd::Failed(format!("无法接管子代理 socket：{e}")),
                }
            }),
            Err(e) => SessionEnd::Failed(format!("无法启动子代理运行时：{e}")),
        };
        // Reap the child (kill it if it is somehow still alive) and remove the scratch dir with the socket file.
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
        let _ = drain.join();
        match end {
            SessionEnd::Result(mut o) => {
                if cancel.is_cancelled() && o.status != RunStatus::Completed {
                    o.status = RunStatus::Cancelled;
                    o.error = cancel.reason().unwrap_or_else(|| "cancelled".into());
                }
                o
            }
            SessionEnd::Failed(failure) => {
                if cancel.is_cancelled() {
                    let mut o = failed(failure);
                    o.status = RunStatus::Cancelled;
                    return o;
                }
                let mut msg = failure;
                let tail = stderr_tail.lock().unwrap();
                if !tail.trim().is_empty() && !msg.contains("exit code") {
                    msg.push_str(&format!("；子代理 stderr：{tail}"));
                } else if !tail.trim().is_empty() {
                    msg.push_str(&format!("；stderr：{tail}"));
                }
                failed(msg)
            }
        }
    }
}

