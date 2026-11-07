//! Sub-agent spawn failures: classification, bounded retry, and the
//! honest text the parent model reads when the spawn did not work.
//!
//! Spawning a child process is a setup step — nothing has executed yet —
//! so a transient failure (a timeout, a refused socket, a resource
//! wobble) is retried up to [`hatch_core::retry::MAX_ATTEMPTS`] times.
//! A permanent failure (missing binary, bad path, permission refused)
//! fails closed at once: retrying what can never succeed only burns
//! attempts and lies about it.
//!
//! A failed spawn never fails the turn on its own: the caller falls back
//! to the in-process path, exactly as before. What changes is that the
//! fallback is no longer silent — the handoff carries a note saying the
//! process spawn itself did not happen, how often it was tried, and
//! whether it is worth trying again later. A cancelled spawn is not a
//! failure and is never reported as one.

use super::process::{ProcessTask, SOCKET_ENV};
use super::spawn::{ChildGuard, LiveChild};
use hatch_core::error::io_is_transient;
use hatch_core::retry::{retry_transient, RETRY_BACKOFF};
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

/// A sub-agent spawn that failed, with its classification and how often
/// it was tried. `attempts` is 0 until the retry wrapper fills it in.
#[derive(Clone, Debug)]
pub struct SpawnFailure {
    pub message: String,
    pub transient: bool,
    pub attempts: usize,
    pub cause_class: &'static str,
}

impl SpawnFailure {
    /// A failure that is permanent by construction (bad task data, a
    /// missing binary path): one attempt, never retried.
    pub fn permanent(message: String, cause_class: &'static str) -> Self {
        SpawnFailure {
            message,
            transient: false,
            attempts: 1,
            cause_class,
        }
    }

    /// Classify one I/O error from a spawn step. The human message keeps
    /// the caller's context; the classification comes from the raw error.
    pub(crate) fn from_io(e: &std::io::Error, context: String) -> Self {
        SpawnFailure {
            message: format!("{context}：{e}"),
            transient: io_is_transient(e),
            attempts: 0,
            cause_class: cause_class_io(e),
        }
    }
}

/// Cause class for one spawn I/O error, in the words the model sees:
/// a missing binary or path is a config problem, a refusal is a
/// permission problem, a transient cause is named as one.
fn cause_class_io(e: &std::io::Error) -> &'static str {
    match e.kind() {
        ErrorKind::NotFound => "配置错误",
        ErrorKind::PermissionDenied => "权限拒绝",
        _ if io_is_transient(e) => "临时故障",
        _ => "启动失败",
    }
}

/// Spawn a child process, retrying transient failures up to
/// [`hatch_core::retry::MAX_ATTEMPTS`] attempts in total. Returns the
/// child, or the classified failure with its attempt count.
pub fn spawn_child(task: &ProcessTask, binary: &Path) -> Result<LiveChild, SpawnFailure> {
    let (result, attempts) = retry_transient(
        || spawn_child_once(task, binary),
        |f: &SpawnFailure| f.transient,
        RETRY_BACKOFF,
    );
    match result {
        Ok(live) => Ok(live),
        Err(mut f) => {
            f.attempts = attempts;
            Err(f)
        }
    }
}

/// The note prepended to the handoff when a spawn failed and the task ran
/// in-process instead. It scopes the honest line to the spawn itself —
/// the task did run, in-process — so the model is never told a failed
/// call succeeded, and never told a completed task did not happen.
pub fn spawn_fallback_note(f: &SpawnFailure) -> String {
    if f.transient {
        format!(
            "注意：子代理进程启动失败（临时故障：{}，已重试 {} 次）。进程外启动这次没执行，过会儿再试；本次任务已改在进程内执行，结论见下。",
            f.message, f.attempts
        )
    } else {
        format!(
            "注意：子代理进程启动失败（{}：{}，不再重试）。本次任务已改在进程内执行，结论见下。",
            f.cause_class, f.message
        )
    }
}

/// One spawn attempt, no retry: [`spawn_child`] below calls this per
/// attempt and classifies the failure. `Err` only when the socket could
/// not be bound or the child could not be spawned (caller falls back
/// in-process).
pub(crate) fn spawn_child_once(
    task: &ProcessTask,
    binary: &Path,
) -> Result<LiveChild, SpawnFailure> {
    let run_id = task.run_id.clone();
    let depth = task.depth;
    std::fs::create_dir_all(&task.scratch_dir)
        .map_err(|e| SpawnFailure::from_io(&e, "无法创建子代理工作区".into()))?;
    let socket_path = task.scratch_dir.join("subagent.sock");
    let _ = std::fs::remove_file(&socket_path);
    // Bound with the standard library (no runtime on the turn thread), non-blocking so Tokio can adopt it in `wait`.
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .and_then(|l| {
            l.set_nonblocking(true)?;
            Ok(l)
        })
        .map_err(|e| {
            SpawnFailure::from_io(
                &e,
                format!("无法创建子代理 socket（{}）", socket_path.display()),
            )
        })?;
    // Only the owner may connect: the task brief travels over this socket (never credentials — those stay in the vault).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }
    let mut child = Command::new(binary)
        .arg("__subagent-run")
        .env(SOCKET_ENV, &socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&socket_path);
            SpawnFailure::from_io(&e, format!("无法启动子代理进程（{}）", binary.display()))
        })?;
    let stderr = child.stderr.take().expect("piped stderr");
    let line = serde_json::to_string(task)
        .map_err(|e| SpawnFailure::permanent(format!("任务无法序列化：{e}"), "配置错误"))?;
    Ok(LiveChild {
        run_id,
        depth,
        timeout_secs: task.timeout_secs,
        guard: ChildGuard { child: Some(child), scratch: task.scratch_dir.clone() },
        listener,
        task_json: line,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transient_failure() -> SpawnFailure {
        SpawnFailure {
            message: "无法启动子代理进程（/bin/hatch）：Connection refused (os error 111)".into(),
            transient: true,
            attempts: 3,
            cause_class: "临时故障",
        }
    }

    fn permanent_failure() -> SpawnFailure {
        SpawnFailure {
            message: "无法启动子代理进程（/nonexistent/hatch）：No such file or directory (os error 2)".into(),
            transient: false,
            attempts: 1,
            cause_class: "配置错误",
        }
    }

    #[test]
    fn transient_note_has_the_verbatim_retry_line() {
        let note = spawn_fallback_note(&transient_failure());
        assert!(note.contains("这次没执行，过会儿再试"), "{note}");
        assert!(note.contains("已重试 3 次"), "{note}");
        assert!(note.contains("已改在进程内执行"), "{note}");
        assert!(!note.contains("已完成"), "{note}");
    }

    #[test]
    fn permanent_note_names_the_cause_and_fails_closed() {
        let note = spawn_fallback_note(&permanent_failure());
        assert!(note.contains("配置错误"), "{note}");
        assert!(note.contains("不再重试"), "{note}");
        assert!(note.contains("已改在进程内执行"), "{note}");
        assert!(!note.contains("过会儿再试"), "{note}");
        assert!(!note.contains("已完成"), "{note}");
    }

    #[test]
    fn io_classification_matrix() {
        let missing = std::io::Error::from_raw_os_error(2);
        let f = SpawnFailure::from_io(&missing, "无法启动子代理进程".into());
        assert!(!f.transient);
        assert_eq!(f.cause_class, "配置错误");

        let refused = std::io::Error::new(ErrorKind::ConnectionRefused, "probe");
        let f = SpawnFailure::from_io(&refused, "无法创建子代理 socket".into());
        assert!(f.transient);
        assert_eq!(f.cause_class, "临时故障");

        let denied = std::io::Error::new(ErrorKind::PermissionDenied, "probe");
        let f = SpawnFailure::from_io(&denied, "无法创建子代理工作区".into());
        assert!(!f.transient);
        assert_eq!(f.cause_class, "权限拒绝");
    }

    #[test]
    fn missing_binary_spawn_fails_closed_with_one_attempt() {
        use crate::subagent::process::ProcessTask;
        let scratch = std::env::temp_dir()
            .join(format!("hatch-sa-retry-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()));
        let task = ProcessTask {
            run_id: "sa_retry".into(),
            agent_type: "explore".into(),
            task: "数文件".into(),
            instructions: String::new(),
            max_steps: 3,
            depth: 1,
            max_depth: 2,
            max_parallel: 3,
            max_tasks: 6,
            session: "ses_1".into(),
            parent_ref: "session:ses_1".into(),
            tainted: false,
            origin: "subagent:sa_retry".into(),
            user_name: "用户".into(),
            user_email: String::new(),
            tz: "Asia/Shanghai".into(),
            workload: "main".into(),
            tools: vec![],
            memory_context: vec![],
            skills: vec![],
            home: None,
            workspace: None,
            scratch_dir: scratch.clone(),
            timeout_secs: 600,
            grants: vec![],
        };
        let err = match spawn_child(&task, Path::new("/nonexistent/hatch-binary")) {
            Ok(_) => panic!("a missing binary must fail"),
            Err(e) => e,
        };
        assert!(!err.transient, "{err:?}");
        assert_eq!(err.attempts, 1, "{err:?}");
        assert_eq!(err.cause_class, "配置错误", "{err:?}");
        assert!(err.message.contains("无法启动子代理进程"), "{err:?}");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
