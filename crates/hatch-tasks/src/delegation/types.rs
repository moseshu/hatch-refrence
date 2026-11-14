//! Durable delegation, part 1: the types.

use crate::EvidenceRef;
use serde::{Deserialize, Serialize};

/// One file/resource write a child reports: the path, the version it
/// read (`""` = new), the version it wrote, and a content hash. The
/// version is an opaque token — a hash for files, a counter string
/// for anything else — compared only for equality.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileWrite {
    pub path: String,
    pub base_version: String,
    pub new_version: String,
    pub hash: String,
}

/// The structured result a child hands back: what it found, what it
/// concluded, what is still open, and what it wrote. The parent
/// merges this; it never re-reads the child's working memory.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChildResult {
    pub summary: String,
    pub evidence: Vec<EvidenceRef>,
    pub conclusions: Vec<String>,
    pub open_items: Vec<String>,
    pub writes: Vec<FileWrite>,
}

/// A delegation's lifecycle. `Superseded` is a failed branch that
/// was re-spawned: the old row stays for audit, the new child does
/// the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Delegated,
    Running,
    Done,
    Failed,
    Cancelled,
    Consumed,
    Superseded,
}

impl DelegationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DelegationStatus::Delegated => "delegated",
            DelegationStatus::Running => "running",
            DelegationStatus::Done => "done",
            DelegationStatus::Failed => "failed",
            DelegationStatus::Cancelled => "cancelled",
            DelegationStatus::Consumed => "consumed",
            DelegationStatus::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> DelegationStatus {
        match s {
            "running" => DelegationStatus::Running,
            "done" => DelegationStatus::Done,
            "failed" => DelegationStatus::Failed,
            "cancelled" => DelegationStatus::Cancelled,
            "consumed" => DelegationStatus::Consumed,
            "superseded" => DelegationStatus::Superseded,
            _ => DelegationStatus::Delegated,
        }
    }

    /// Terminal: the delegation will not move again (except a failed
    /// one, which `respawn_branch` supersedes).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DelegationStatus::Failed
                | DelegationStatus::Cancelled
                | DelegationStatus::Consumed
                | DelegationStatus::Superseded
        )
    }
}

/// One row of `task_delegations`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    pub parent_task: String,
    pub child_task: String,
    pub step_id: String,
    pub status: DelegationStatus,
    pub result: Option<ChildResult>,
    pub failure_kind: String,
    pub consumed_at: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A write the merge refused: the parent already knows a newer
/// version than the one the child read. Both versions are kept, so
/// the conflict is resolvable instead of silently resolved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: String,
    pub current_version: String,
    pub current_writer: String,
    pub attempted_version: String,
    pub attempted_writer: String,
}

/// What one `consume_ready` run did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsumeOutcome {
    /// Children merged this run.
    pub consumed: Vec<String>,
    /// Children already consumed by an earlier run (replay no-op).
    pub already: Vec<String>,
    /// Writes refused by the merge, with both versions.
    pub conflicts: Vec<MergeConflict>,
    /// Steps still pending because no lease was given to complete
    /// them — the next worker run completes them from the stored
    /// result.
    pub steps_pending: Vec<String>,
}

/// What one `handle_failed` run did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FailureReport {
    /// Failed branches re-spawned (transient, retries left).
    pub respawned: Vec<String>,
    /// Branches that failed permanently: the parent proceeds with
    /// partial results and the gap is recorded in its context.
    pub failed_branches: Vec<String>,
    /// A branch that needs a human: the parent is blocked with this
    /// reason.
    pub blocked_reason: Option<String>,
}

pub(crate) fn delegation_row(r: &hatch_store::Row) -> hatch_store::rusqlite::Result<Delegation> {
    let result_json: String = r.get(4)?;
    Ok(Delegation {
        parent_task: r.get(0)?,
        child_task: r.get(1)?,
        step_id: r.get(2)?,
        status: DelegationStatus::parse(&r.get::<_, String>(3)?),
        result: serde_json::from_str(&result_json)
            .ok()
            .filter(|_| !result_json.is_empty()),
        failure_kind: r.get(5)?,
        consumed_at: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

pub(crate) const DELEGATION_COLS: &str =
    "parent_task, child_task, step_id, status, result_json, failure_kind, consumed_at, created_at, updated_at";
