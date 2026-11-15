//! Persisted execution parameters (P0 product closed loop): the
//! validated `POST /v1/tasks` payload, stored as one JSON row per
//! task (`task_run_params`, schema v20) so a worker resuming after
//! a daemon restart needs no CLI flags.
//!
//! The shape mirrors the CLI's `task run` flags: a fixture task
//! names its eval world, a Gmail task names its account + scope +
//! invite spec. The resident worker interprets the row back into
//! `run_worker` / `run_worker_gmail` inputs; the API create handler
//! validates it first, so a stored row is always runnable.
//!
//! The payload hash is the sha256 of the canonical JSON (serde_json
//! objects sort their keys, so the same content hashes the same way
//! regardless of field order). A repeated idempotency key with a
//! different hash is a conflict, not a silent duplicate.

use super::task_run_params_gmail::{default_title, parse_invite, InviteParams};
use crate::commands::task_kinds::is_known_kind;
use hatch_core::{Error, Result};
use hatch_store::Db;
use hatch_tasks::{RunParamsRecord, TaskRunParams};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// One task's execution parameters, as created (API) or inherited
/// (a delegated child copies its parent's row with its own kind and
/// title). `out_dir: None` means the per-task default
/// `<home>/task-out/<task_id>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunParams {
    /// Schema version of this JSON (1 today).
    #[serde(default = "default_version")]
    pub version: u32,
    pub kind: String,
    #[serde(default = "default_source")]
    pub source: String,
    /// Eval world dir (fixture source), absolute at creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world: Option<PathBuf>,
    /// Artifact output dir; None = `<home>/task-out/<task_id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_dir: Option<PathBuf>,
    /// Task title (already defaulted at creation).
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Gmail search query (gmail source).
    #[serde(default = "default_gmail_query")]
    pub gmail_query: String,
    /// How many recent messages to sync, 1..=20 (gmail source).
    #[serde(default = "default_gmail_max")]
    pub gmail_max: u64,
    /// The real account's address (gmail source, required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gmail_user: Option<String>,
    /// The account owner's display name (gmail source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gmail_name: Option<String>,
    /// Invite arguments (gmail + meeting-invite kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<InviteParams>,
    /// Model-call budget (0 = unlimited).
    #[serde(default)]
    pub max_model_calls: u64,
    /// Lease TTL in seconds (0 = the worker default).
    #[serde(default)]
    pub lease_ttl_secs: u64,
    /// The routed execution mode (P1, §6.1): `fixed-flow` /
    /// `bounded-react` / `plan-and-execute` / `durable-subtasks`.
    /// `""` = a pre-P1 row: the worker dispatches by kind alone and
    /// does not validate the mode.
    #[serde(default)]
    pub mode: String,
    /// Bounded-ReAct caps (P1, §6.1: every step limits model calls,
    /// tool calls and time). 0 = the runner default (6 / 20 / 300).
    #[serde(default)]
    pub react_max_steps: u64,
    #[serde(default)]
    pub react_max_tool_calls: u64,
    #[serde(default)]
    pub react_timeout_secs: u64,
}

fn default_version() -> u32 {
    1
}

fn default_source() -> String {
    "fixture".to_string()
}

fn default_timezone() -> String {
    "Asia/Shanghai".to_string()
}

fn default_gmail_query() -> String {
    "is:unread".to_string()
}

fn default_gmail_max() -> u64 {
    10
}

impl RunParams {
    /// Parse + validate a create body into parameters. Mirrors the
    /// CLI's `task run` validation (`gmail_run_from_flags`): the
    /// same flags, the same bounds, the same required fields.
    pub fn from_create_body(body: &serde_json::Value) -> Result<RunParams> {
        let kind = body
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Invalid("缺少字段 kind（字符串）".into()))?
            .to_string();
        if !is_known_kind(&kind) {
            return Err(Error::Invalid(format!(
                "未知任务 kind {kind}（已知 kind：{}）",
                crate::commands::task_kinds::KNOWN_KINDS.join("、")
            )));
        }
        let source = body
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("fixture");
        if source != "fixture" && source != "gmail" {
            return Err(Error::Invalid(format!(
                "未知来源 {source}（fixture/gmail）"
            )));
        }
        let world = match body.get("world").and_then(|v| v.as_str()) {
            Some(w) => {
                if source == "gmail" {
                    return Err(Error::Invalid("world 与 source gmail 不能同时用".into()));
                }
                let p = absolutize(w)?;
                if !p.is_dir() {
                    return Err(Error::Invalid(format!(
                        "world 不存在或不是目录：{}",
                        p.display()
                    )));
                }
                Some(p)
            }
            None => None,
        };
        // A fixture world is required only by the fixed flows (they
        // load it for the mailbox and expectations). React / plan /
        // delegate-join tasks submitted from chat carry no world —
        // the resident worker runs them on a synthetic persona world
        // instead (see `run_ready_task`).
        if source == "fixture"
            && world.is_none()
            && matches!(kind.as_str(), "meeting-brief" | "meeting-invite" | "weekly-report")
        {
            return Err(Error::Invalid("fixture 模式需要 world <dir>".into()));
        }
        let gmail_max = body.get("gmail_max").and_then(|v| v.as_u64()).unwrap_or(10);
        if source == "gmail" && !(1..=20).contains(&gmail_max) {
            return Err(Error::Invalid(
                "gmail_max 须在 1..=20 之间（试点一次最多 20 封）".into(),
            ));
        }
        let gmail_user = body
            .get("gmail_user")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if source == "gmail" && gmail_user.is_none() {
            return Err(Error::Invalid("source gmail 需要 gmail_user <邮箱>".into()));
        }
        let invite = match body.get("invite") {
            Some(v) if !v.is_null() => Some(parse_invite(v)?),
            _ => None,
        };
        if kind == "meeting-invite" && source == "gmail" && invite.is_none() {
            return Err(Error::Invalid(
                "会议邀请需要 invite {event_title, event_start, attendee_email}".into(),
            ));
        }
        // P1: the routed execution mode + ReAct caps, persisted so
        // the worker can validate them against the kind before
        // dispatching (parsed in `task_routing`).
        let (mode, react_max_steps, react_max_tool_calls, react_timeout_secs) =
            crate::commands::task_routing::parse_mode_caps(body)?;
        let title = body
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_title(&kind, invite.as_ref()));
        let out_dir = match body.get("out_dir").and_then(|v| v.as_str()) {
            Some(d) => Some(absolutize(d)?),
            None => None,
        };
        Ok(RunParams {
            version: 1,
            kind,
            source: source.to_string(),
            world,
            out_dir,
            title,
            prompt: body
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            timezone: body
                .get("timezone")
                .and_then(|v| v.as_str())
                .unwrap_or("Asia/Shanghai")
                .to_string(),
            gmail_query: body
                .get("gmail_query")
                .and_then(|v| v.as_str())
                .unwrap_or("is:unread")
                .to_string(),
            gmail_max,
            gmail_user,
            gmail_name: body
                .get("gmail_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            invite,
            max_model_calls: body
                .get("max_model_calls")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            lease_ttl_secs: body
                .get("lease_ttl_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            mode,
            react_max_steps,
            react_max_tool_calls,
            react_timeout_secs,
        })
    }

    /// Canonical JSON for storage + hashing. Keys sort (serde_json
    /// objects are BTreeMaps), so the same content hashes the same.
    pub fn to_canonical_json(&self) -> Result<String> {
        serde_json::to_string(&serde_json::to_value(self)?)
            .map_err(|e| Error::Invalid(format!("执行参数序列化失败：{e}")))
    }

    /// sha256 hex of the canonical JSON.
    pub fn payload_hash(&self) -> Result<String> {
        let json = self.to_canonical_json()?;
        Ok(hex(&Sha256::digest(json.as_bytes())))
    }

    /// Parse a stored row back into parameters.
    pub fn from_record(record: &RunParamsRecord) -> Result<RunParams> {
        serde_json::from_str(&record.params_json)
            .map_err(|e| Error::Invalid(format!("执行参数解析失败：{e}")))
    }

    /// The output dir for a run: the stored one, or the per-task
    /// default under the home dir.
    pub fn out_dir_for(&self, home: &Path, task_id: &str) -> PathBuf {
        match &self.out_dir {
            Some(d) => d.clone(),
            None => home.join("task-out").join(task_id),
        }
    }

    /// Load the stored parameters for a task, if it was created
    /// with any (API-created tasks always are; older CLI tasks are
    /// not — the resident worker skips those).
    pub fn load(db: &Db, task_id: &str) -> Result<Option<RunParams>> {
        match TaskRunParams::new(db).get(task_id)? {
            Some(record) => Ok(Some(RunParams::from_record(&record)?)),
            None => Ok(None),
        }
    }

    /// Store the parameters for a task (create, or the idempotency
    /// repair when a create crashed between the task row and this
    /// row).
    pub fn save(
        &self,
        db: &Db,
        task_id: &str,
        now: &hatch_core::time::Timestamp,
    ) -> Result<String> {
        let hash = self.payload_hash()?;
        let json = self.to_canonical_json()?;
        TaskRunParams::new(db).save(task_id, &hash, &json, now)?;
        Ok(hash)
    }

}

/// Resolve a path from a create body: absolute stays, relative is
/// resolved against the process working directory and stored absolute
/// — a daemon restart must not reinterpret it against a new cwd.
fn absolutize(p: &str) -> Result<PathBuf> {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd =
        std::env::current_dir().map_err(|e| Error::Invalid(format!("取当前目录失败：{e}")))?;
    Ok(cwd.join(path))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

