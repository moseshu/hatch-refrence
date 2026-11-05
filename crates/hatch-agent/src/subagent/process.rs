//! Parent ↔ child process protocol for sub-agents.
//!
//! Transport: a Unix socket ([`tokio::net::UnixListener`], see
//! [`super::spawn`]). The parent binds `<scratch>/subagent.sock` (0600)
//! and spawns the child with `HATCH_SUBAGENT_SOCKET` set; the child
//! connects, the parent writes one JSON line (a [`ProcessTask`]), and
//! the child writes JSONL events back over the same socket:
//!
//! ```text
//! {"type":"progress","kind":"tool_call","detail":"fs_list ."}   // forwarded
//! {"type":"result","status":"completed","answer":"…",…}          // exactly one, last
//! ```
//!
//! The child watches the socket for EOF on a watchdog thread: the parent
//! keeps the connection open for the whole run, so a parent that dies
//! closes the socket and the child exits (130) instead of becoming an
//! orphan.
//!
//! Exit code 0 means "a result event was written" (even when the turn
//! itself failed — the failure is in the event). A non-zero exit without a
//! result event means the child died; the parent reports that as a failure.
//! Nothing but JSONL goes over the socket; diagnostics go to stderr.
//!
//! The child never touches the shared database: it works in a scratch
//! directory (`scratch_dir`, owned by the parent) with its own database
//! file. Approvals it queued and grants it consumed come back inside the
//! result event, and the parent replays them into the shared database.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Env var the parent sets on the child with the socket path.
pub const SOCKET_ENV: &str = "HATCH_SUBAGENT_SOCKET";

/// One approval, as the child saw it, for the parent to replay.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalWire {
    pub idempotency_key: String,
    pub kind: String,
    pub class: String,
    pub title: String,
    pub preview: String,
    pub payload: serde_json::Value,
    pub reasons: Vec<String>,
    pub origin: String,
    /// RFC 3339, as stored by the child.
    pub created_at: String,
    pub expires_at: String,
    /// `pending` | `approved` | `executed` | `failed` | …
    pub status: String,
}

/// A standing grant, as the parent seeded it, for the child to use.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrantWire {
    pub class: String,
    pub recipients: Vec<String>,
    pub max_uses: Option<i64>,
    pub allow_when_tainted: bool,
    /// RFC 3339, or `None` for no expiry.
    pub expires_at: Option<String>,
    pub note: String,
}

/// A grant the child used, identified by content (child-side grant ids are
/// random and meaningless to the parent).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsumedGrant {
    pub class: String,
    pub recipients: Vec<String>,
    pub note: String,
    pub uses: i64,
}

/// Everything a child process needs to run one delegated task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessTask {
    pub run_id: String,
    pub agent_type: String,
    pub task: String,
    /// Type instructions (appended to the sub-agent role text).
    pub instructions: String,
    pub max_steps: usize,
    pub depth: usize,
    pub max_depth: usize,
    pub max_parallel: usize,
    pub max_tasks: usize,
    /// Top-level chat session id.
    pub session: String,
    /// `session:<id>` or `subagent:<id>` of the delegating agent.
    pub parent_ref: String,
    pub tainted: bool,
    pub origin: String,
    pub user_name: String,
    pub user_email: String,
    /// Timezone name, e.g. `Asia/Shanghai`.
    pub tz: String,
    /// Workload name, e.g. `main`.
    pub workload: String,
    /// Tool names the child may offer (already narrowed by the parent).
    pub tools: Vec<String>,
    /// Recalled memory lines, already formatted (`[id] text（来源 …）`).
    pub memory_context: Vec<String>,
    /// Active skill bodies, as context layers.
    pub skills: Vec<String>,
    /// Home dir to read config from; `None` = test mode (scripted model).
    pub home: Option<PathBuf>,
    /// Workspace dir for the child's file/shell tools.
    pub workspace: Option<PathBuf>,
    /// Scratch dir owned by the parent (child database, outbox, …).
    pub scratch_dir: PathBuf,
    pub timeout_secs: u64,
    /// Standing grants active in the parent, seeded into the child's db.
    pub grants: Vec<GrantWire>,
}

/// One line the child writes to stdout.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChildEvent {
    Progress { kind: String, detail: String },
    Result(ChildResult),
}

/// The final report of a child run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildResult {
    /// `completed` | `failed` | `cancelled`.
    pub status: String,
    pub error: String,
    pub answer: Option<String>,
    pub steps: usize,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tainted: bool,
    pub approvals: Vec<ApprovalWire>,
    pub consumed_grants: Vec<ConsumedGrant>,
    /// Tool names the parent allowed but the child could not offer.
    pub unavailable_tools: Vec<String>,
}

/// What the parent takes from a child process run.
#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub status: crate::subagent::RunStatus,
    pub error: String,
    pub answer: Option<String>,
    pub steps: usize,
    pub model_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tainted: bool,
    pub approvals: Vec<ApprovalWire>,
    pub consumed_grants: Vec<ConsumedGrant>,
    pub unavailable_tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_and_result_round_trip() {
        let task = ProcessTask {
            run_id: "sa_1".into(),
            agent_type: "explore".into(),
            task: "数文件".into(),
            instructions: String::new(),
            max_steps: 12,
            depth: 1,
            max_depth: 2,
            max_parallel: 3,
            max_tasks: 6,
            session: "ses_1".into(),
            parent_ref: "session:ses_1".into(),
            tainted: false,
            origin: "subagent:sa_1".into(),
            user_name: "用户".into(),
            user_email: String::new(),
            tz: "Asia/Shanghai".into(),
            workload: "main".into(),
            tools: vec!["fs_read".into()],
            memory_context: vec!["[c1] 偏好短回复".into()],
            skills: vec![],
            home: None,
            workspace: None,
            scratch_dir: PathBuf::from("/tmp/sa_1"),
            timeout_secs: 600,
            grants: vec![],
        };
        let line = serde_json::to_string(&task).unwrap();
        let back: ProcessTask = serde_json::from_str(&line).unwrap();
        assert_eq!(back.run_id, "sa_1");
        assert_eq!(back.memory_context.len(), 1);
        let ev = ChildEvent::Result(ChildResult {
            status: "completed".into(),
            error: String::new(),
            answer: Some("3 个文件".into()),
            steps: 2,
            model_calls: 2,
            prompt_tokens: 10,
            completion_tokens: 5,
            tainted: false,
            approvals: vec![],
            consumed_grants: vec![],
            unavailable_tools: vec![],
        });
        let line = serde_json::to_string(&ev).unwrap();
        assert!(matches!(
            serde_json::from_str::<ChildEvent>(&line).unwrap(),
            ChildEvent::Result(_)
        ));
        let p = ChildEvent::Progress {
            kind: "tool_call".into(),
            detail: "fs_list .".into(),
        };
        let line = serde_json::to_string(&p).unwrap();
        assert!(line.contains("\"type\":\"progress\""));
    }
}
