//! Free text → validated step edits (P1, plan-3 §10.2): the
//! `command` path finally applies what the instruction says, instead
//! of only recording it (the P0 #2 residual).
//!
//! Two forms, one gate:
//! - A structured `edit` object: `{step, title?, tool?,
//!   arguments?, expected?}` — the model (or client) names the step
//!   by id, exact title, or an unambiguous keyword, and the new
//!   field values.
//! - Free text with a `把X改成Y` / `X改为Y` / `X换成Y` replacement:
//!   `X` must occur in exactly one unexecuted step's title or
//!   arguments, and is replaced by `Y` there.
//!
//! Both resolve among UNEXECUTED steps only (pending, or running
//! but gated on a still-pending approval — the task-5 rule), then
//! apply through the existing `edit_step` mechanics: version-checked
//! against `expected_version`, params re-validated, version bumped,
//! older approvals invalidated. An ambiguous target (two steps
//! match, or no step matches) or an executed step is REFUSED with a
//! reason asking for clarification — never guessed silently. Text
//! with no edit intent at all (no `edit` object, no replacement
//! pattern) still goes through the old record-only `apply_command`.

use super::task_control::apply_command;
use super::task_edit::edit_step;
use super::task_version::reject_command;
use super::task_view::approval_status;
use hatch_core::time::Timestamp;
use hatch_core::{Error, Result};
use hatch_policy::ApprovalStatus;
use hatch_store::Db;
use hatch_tasks::{ExecStatus, StepEdit, TaskStep, TaskStore, UnitStore};
use serde_json::Value;

/// A structured step edit carried by a command.
#[derive(Clone, Debug)]
pub struct CommandEdit {
    /// Step id, exact title, or an unambiguous keyword.
    pub step_ref: String,
    pub title: Option<String>,
    pub tool: Option<String>,
    pub arguments: Option<Value>,
    pub expected: Option<String>,
}

impl CommandEdit {
    /// Parse the `edit` object of a command body.
    pub fn from_json(v: &Value) -> Result<CommandEdit> {
        let step_ref = v
            .get("step")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Invalid("edit 缺少 step（步骤 id / 标题 / 关键词）".into()))?
            .to_string();
        let arguments = match v.get("arguments") {
            Some(a) if !a.is_null() => {
                if !a.is_object() {
                    return Err(Error::Invalid("edit.arguments 必须是 JSON 对象".into()));
                }
                Some(a.clone())
            }
            _ => None,
        };
        Ok(CommandEdit {
            step_ref,
            title: v.get("title").and_then(Value::as_str).map(str::to_string),
            tool: v.get("tool").and_then(Value::as_str).map(str::to_string),
            arguments,
            expected: v.get("expected").and_then(Value::as_str).map(str::to_string),
        })
    }

    fn to_step_edit(&self) -> StepEdit {
        StepEdit {
            title: self.title.clone(),
            tool: self.tool.clone(),
            arguments: self.arguments.clone(),
            expected: self.expected.clone(),
        }
    }

    fn has_content(&self) -> bool {
        self.title.is_some() || self.tool.is_some() || self.arguments.is_some() || self.expected.is_some()
    }
}

/// What resolving a command produced: the step to edit and how.
#[derive(Debug)]
pub(crate) struct ResolvedEdit {
    pub(crate) step_id: String,
    pub(crate) edit: StepEdit,
}

/// The outcome of resolving a command: a validated step edit, or
/// record-only — with an optional note when the text looked like a
/// replacement but no step matched (the instruction may target the
/// task itself, e.g. its title; it is recorded and the version
/// bumps, as before, but no step is changed).
#[derive(Debug)]
pub(crate) enum CommandResolution {
    Edit(ResolvedEdit),
    RecordOnly(Option<String>),
}

/// Resolve a command to a validated step edit. `Ok(RecordOnly)` =
/// no edit intent, or free text that matched no step — record the
/// command only. `Err` = ambiguous, or a structured edit whose
/// step does not exist / already ran: refuse and ask for
/// clarification.
pub(crate) fn resolve_command_edit(
    db: &Db,
    task_id: &str,
    summary: &str,
    edit: Option<&CommandEdit>,
) -> Result<CommandResolution> {
    match edit {
        Some(e) => Ok(CommandResolution::Edit(resolve_structured(db, task_id, e)?)),
        None => resolve_free_text(db, task_id, summary),
    }
}

/// Apply a command that may carry a step edit. With an edit, the
/// existing `edit_step` path runs (version check → bump →
/// invalidate); without one, the record-only `apply_command` runs.
/// A refused resolution is recorded as a rejected command.
pub fn apply_command_edit(
    db: &Db,
    task_id: &str,
    expected_version: u64,
    summary: &str,
    edit: Option<&CommandEdit>,
    user_id: &str,
    now: &Timestamp,
) -> Result<String> {
    let resolved = match resolve_command_edit(db, task_id, summary, edit) {
        Ok(r) => r,
        Err(e) => {
            let current = TaskStore::new(db)
                .get(task_id)?
                .map(|t| t.plan_version)
                .unwrap_or(expected_version);
            return reject_command(
                db,
                task_id,
                expected_version,
                current,
                summary,
                user_id,
                &e.to_string(),
                now,
            )
            .map(|_| unreachable!());
        }
    };
    match resolved {
        CommandResolution::Edit(r) => {
            edit_step(db, task_id, &r.step_id, expected_version, &r.edit, user_id, now)
        }
        CommandResolution::RecordOnly(note) => {
            let msg = apply_command(db, task_id, expected_version, summary, user_id, now)?;
            Ok(match note {
                Some(n) => format!("{msg}；{n}"),
                None => msg,
            })
        }
    }
}

/// The steps an edit may touch: pending, or running but gated on a
/// still-pending approval (its side effect has not executed yet).
/// Any other running step is actively executing; a done/failed step
/// already ran. Same rule as `edit_step`'s pre-check.
fn editable_steps(db: &Db, task_id: &str) -> Result<Vec<TaskStep>> {
    let steps = UnitStore::new(db).list_steps(task_id)?;
    Ok(steps
        .into_iter()
        .filter(|s| match s.status {
            ExecStatus::Pending => true,
            ExecStatus::Running => {
                !s.approval_id.is_empty()
                    && approval_status(db, &s.approval_id) == Some(ApprovalStatus::Pending)
            }
            _ => false,
        })
        .collect())
}

/// Resolve a structured edit: the step reference must name exactly
/// one editable step, by id, exact title, or a unique keyword.
fn resolve_structured(
    db: &Db,
    task_id: &str,
    edit: &CommandEdit,
) -> Result<ResolvedEdit> {
    if !edit.has_content() {
        return Err(Error::Invalid("edit 没有要改的内容（title/tool/arguments/expected 至少给一个）".into()));
    }
    let editable = editable_steps(db, task_id)?;
    let reference = edit.step_ref.as_str();
    // Exact id first, then exact title, then a unique keyword.
    if let Some(s) = editable.iter().find(|s| s.step_id == reference) {
        return Ok(ResolvedEdit { step_id: s.step_id.clone(), edit: edit.to_step_edit() });
    }
    if let Some(s) = editable.iter().find(|s| s.title == reference) {
        return Ok(ResolvedEdit { step_id: s.step_id.clone(), edit: edit.to_step_edit() });
    }
    let lowered = reference.to_lowercase();
    let matches: Vec<&TaskStep> = editable
        .iter()
        .filter(|s| {
            s.title.to_lowercase().contains(&lowered)
                || s.tool.to_lowercase().contains(&lowered)
                || serde_json::to_string(&s.arguments)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&lowered)
        })
        .collect();
    match matches.len() {
        0 => Err(Error::Invalid(format!(
            "没有找到提到“{reference}”的未执行步骤：请用步骤 id 或完整标题指明要改的步骤"
        ))),
        1 => Ok(ResolvedEdit {
            step_id: matches[0].step_id.clone(),
            edit: edit.to_step_edit(),
        }),
        _ => Err(Error::Invalid(format!(
            "“{reference}”匹配到 {} 个未执行步骤（{}）：请用步骤 id 或完整标题明确指出要改哪一个",
            matches.len(),
            matches
                .iter()
                .map(|s| format!("{}《{}》", s.step_id, s.title))
                .collect::<Vec<_>>()
                .join("、")
        ))),
    }
}

/// Parse `把X改成Y` / `X改成Y` / `X改为Y` / `把X换成Y` out of free
/// text. Returns `(X, Y)` with surrounding punctuation stripped, or
/// `None` when the text carries no replacement.
fn parse_replacement(summary: &str) -> Option<(String, String)> {
    for marker in ["改成", "改为", "换成"] {
        if let Some(at) = summary.find(marker) {
            let before = summary[..at].trim();
            let after = summary[at + marker.len()..].trim();
            if before.is_empty() || after.is_empty() {
                return None;
            }
            let from = before.strip_prefix('把').unwrap_or(before);
            let from = from.trim();
            let to = after
                .trim_matches(|c: char| c == '。' || c == '，' || c == '；' || c == '"' || c == '\'' || c == '”' || c == '’')
                .trim();
            if from.is_empty() || to.is_empty() {
                return None;
            }
            return Some((from.to_string(), to.to_string()));
        }
    }
    None
}

/// Resolve free text: a replacement must land in exactly one
/// editable step. No replacement pattern means no edit intent; a
/// pattern that matches no step is recorded only (the instruction
/// may target the task itself) with a note saying so.
fn resolve_free_text(
    db: &Db,
    task_id: &str,
    summary: &str,
) -> Result<CommandResolution> {
    let Some((from, to)) = parse_replacement(summary) else {
        return Ok(CommandResolution::RecordOnly(None));
    };
    let editable = editable_steps(db, task_id)?;
    let matches: Vec<&TaskStep> = editable
        .iter()
        .filter(|s| {
            s.title.contains(&from)
                || serde_json::to_string(&s.arguments)
                    .unwrap_or_default()
                    .contains(&from)
        })
        .collect();
    match matches.len() {
        0 => Ok(CommandResolution::RecordOnly(Some(format!(
            "没有步骤提到“{from}”，未修改任何步骤"
        )))),
        1 => {
            let step = matches[0];
            let mut edit = StepEdit {
                title: None,
                tool: None,
                arguments: None,
                expected: None,
            };
            if step.title.contains(&from) {
                edit.title = Some(step.title.replace(&from, &to));
            }
            let args_json = serde_json::to_string(&step.arguments)
                .map_err(|e| Error::Invalid(format!("步骤参数序列化失败：{e}")))?;
            if args_json.contains(&from) {
                let replaced = args_json.replace(&from, &to);
                let value: Value = serde_json::from_str(&replaced).map_err(|e| {
                    Error::Invalid(format!("把“{from}”改成“{to}”后参数不是合法 JSON：{e}"))
                })?;
                edit.arguments = Some(value);
            }
            if edit.title.is_none() && edit.arguments.is_none() {
                return Err(Error::Invalid(format!(
                    "“{from}”只出现在步骤 {} 的其他字段里，无法替换",
                    step.step_id
                )));
            }
            Ok(CommandResolution::Edit(ResolvedEdit { step_id: step.step_id.clone(), edit }))
        }
        _ => Err(Error::Invalid(format!(
            "“{from}”匹配到 {} 个未执行步骤（{}）：请明确指出要改哪一个",
            matches.len(),
            matches
                .iter()
                .map(|s| format!("{}《{}》", s.step_id, s.title))
                .collect::<Vec<_>>()
                .join("、")
        ))),
    }
}

