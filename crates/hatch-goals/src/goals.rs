//! Goal workspaces on disk (`$HATCH_HOME/goals/<id>/`).
//!
//! Layout (mirrors the workspace layout the repo's docs already use):
//! `GOAL.md` (title, status, created/updated), `files/` (supporting docs
//! the user may read) and `hidden_files/` (internal worker state).
//!
//! The status machine and the attention levels below are ORIGINAL design
//! for this codebase — they are not a claim about how the Muse framework
//! behaves; no Muse evidence for a goals surface was available.

use hatch_core::time::{parse_rfc3339, to_rfc3339, Timestamp};
use hatch_core::{ids, Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Goal lifecycle. Terminal states never leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Completed,
    Paused,
    Abandoned,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Completed => "completed",
            GoalStatus::Paused => "paused",
            GoalStatus::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<GoalStatus> {
        match s {
            "active" => Some(GoalStatus::Active),
            "completed" => Some(GoalStatus::Completed),
            "paused" => Some(GoalStatus::Paused),
            "abandoned" => Some(GoalStatus::Abandoned),
            _ => None,
        }
    }

    /// Allowed transitions. Completed / abandoned are terminal.
    pub fn can_transition_to(&self, next: GoalStatus) -> bool {
        matches!(
            (self, next),
            (GoalStatus::Active, GoalStatus::Completed)
                | (GoalStatus::Active, GoalStatus::Paused)
                | (GoalStatus::Active, GoalStatus::Abandoned)
                | (GoalStatus::Paused, GoalStatus::Active)
                | (GoalStatus::Paused, GoalStatus::Completed)
                | (GoalStatus::Paused, GoalStatus::Abandoned)
        )
    }
}

/// How much a goal needs the user right now (ORIGINAL design).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    UserActionRequired,
    NewInformation,
    DoesNotNeedAttention,
}

impl Attention {
    pub fn as_str(&self) -> &'static str {
        match self {
            Attention::UserActionRequired => "user_action_required",
            Attention::NewInformation => "new_information",
            Attention::DoesNotNeedAttention => "does_not_need_attention",
        }
    }
}

/// Pure attention rule, so it is testable without a database:
/// terminal goals never need attention; unread worker output linked to
/// the goal needs the user; a goal updated in the last 48h is new
/// information; anything else can wait.
pub fn attention_level(
    status: GoalStatus,
    unread_linked_outputs: usize,
    updated_at: &Timestamp,
    now: &Timestamp,
) -> Attention {
    match status {
        GoalStatus::Completed | GoalStatus::Abandoned => Attention::DoesNotNeedAttention,
        _ if unread_linked_outputs > 0 => Attention::UserActionRequired,
        _ if *now - *updated_at < chrono::Duration::hours(48) => Attention::NewInformation,
        _ => Attention::DoesNotNeedAttention,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: GoalStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Body of GOAL.md after the header (user notes, free text).
    pub body: String,
}

impl Goal {
    fn render(&self) -> String {
        let mut s = format!(
            "# {}\nstatus: {}\ncreated: {}\nupdated: {}\n",
            self.title,
            self.status.as_str(),
            to_rfc3339(&self.created_at),
            to_rfc3339(&self.updated_at)
        );
        if !self.body.trim().is_empty() {
            s.push('\n');
            s.push_str(self.body.trim());
            s.push('\n');
        }
        s
    }

    fn parse(id: &str, text: &str) -> Result<Goal> {
        let mut title = String::new();
        let mut status = GoalStatus::Active;
        let mut created = None;
        let mut updated = None;
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_header = true;
        for line in text.lines() {
            if in_header {
                if let Some(t) = line.strip_prefix("# ") {
                    title = t.trim().to_string();
                    continue;
                }
                if let Some((k, v)) = line.split_once(':') {
                    match k.trim() {
                        "status" => {
                            status = GoalStatus::parse(v.trim())
                                .ok_or_else(|| Error::Invalid(format!("goal {id}: unknown status {}", v.trim())))?;
                            continue;
                        }
                        "created" => {
                            created = Some(parse_rfc3339(v.trim())?);
                            continue;
                        }
                        "updated" => {
                            updated = Some(parse_rfc3339(v.trim())?);
                            continue;
                        }
                        _ => {}
                    }
                }
                if line.trim().is_empty() {
                    in_header = false;
                    continue;
                }
                in_header = false;
            }
            body_lines.push(line);
        }
        if title.is_empty() {
            return Err(Error::Invalid(format!("goal {id}: GOAL.md has no title")));
        }
        let created = created.ok_or_else(|| Error::Invalid(format!("goal {id}: GOAL.md has no created timestamp")))?;
        let updated = updated.unwrap_or(created);
        Ok(Goal { id: id.to_string(), title, status, created_at: created, updated_at: updated, body: body_lines.join("\n").trim().to_string() })
    }
}

/// Filesystem store for goal workspaces.
pub struct GoalStore {
    root: PathBuf,
}

impl GoalStore {
    pub fn new(home: &Path) -> Self {
        GoalStore { root: home.join("goals") }
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn validate_id(id: &str) -> Result<()> {
        if id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id == "."
            || id == ".."
        {
            return Err(Error::Invalid(format!("invalid goal id {id:?}")));
        }
        Ok(())
    }

    pub fn create(&self, title: &str, now: &Timestamp) -> Result<Goal> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::Invalid("goal title must not be empty".into()));
        }
        let goal = Goal {
            id: ids::random_id("goal"),
            title: title.to_string(),
            status: GoalStatus::Active,
            created_at: *now,
            updated_at: *now,
            body: String::new(),
        };
        let dir = self.dir(&goal.id);
        std::fs::create_dir_all(dir.join("files"))?;
        std::fs::create_dir_all(dir.join("hidden_files"))?;
        std::fs::write(dir.join("GOAL.md"), goal.render())?;
        Ok(goal)
    }

    pub fn get(&self, id: &str) -> Result<Goal> {
        Self::validate_id(id)?;
        let text = std::fs::read_to_string(self.dir(id).join("GOAL.md"))
            .map_err(|_| Error::Invalid(format!("no such goal {id}")))?;
        Goal::parse(id, &text)
    }

    pub fn list(&self) -> Result<Vec<Goal>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in rd {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(g) = self.get(&name) {
                out.push(g);
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        Ok(out)
    }

    /// Change status, enforcing the machine. Returns the updated goal.
    pub fn transition(&self, id: &str, next: GoalStatus, now: &Timestamp) -> Result<Goal> {
        let mut goal = self.get(id)?;
        if goal.status == next {
            return Ok(goal);
        }
        if !goal.status.can_transition_to(next) {
            return Err(Error::Invalid(format!(
                "goal {} is {} and cannot move to {}",
                id,
                goal.status.as_str(),
                next.as_str()
            )));
        }
        goal.status = next;
        goal.updated_at = *now;
        std::fs::write(self.dir(id).join("GOAL.md"), goal.render())?;
        Ok(goal)
    }

    /// Supporting documents the user may read (`files/`).
    pub fn files(&self, id: &str) -> Result<Vec<String>> {
        Self::validate_id(id)?;
        let dir = self.dir(id).join("files");
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    out.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Write internal worker state (`hidden_files/` — never shown to the user).
    pub fn write_hidden(&self, id: &str, name: &str, content: &str) -> Result<()> {
        Self::validate_id(id)?;
        if name.contains('/') || name.contains('\\') || name.is_empty() {
            return Err(Error::Invalid(format!("invalid hidden file name {name:?}")));
        }
        std::fs::write(self.dir(id).join("hidden_files").join(name), content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> Timestamp {
        chrono::Utc.with_ymd_and_hms(2026, 11, 3, 8, 0, 0).unwrap()
    }

    #[test]
    fn status_machine_allows_only_the_documented_moves() {
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Paused));
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Completed));
        assert!(GoalStatus::Active.can_transition_to(GoalStatus::Abandoned));
        assert!(GoalStatus::Paused.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Completed.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Completed.can_transition_to(GoalStatus::Paused));
        assert!(!GoalStatus::Abandoned.can_transition_to(GoalStatus::Active));
        assert!(!GoalStatus::Active.can_transition_to(GoalStatus::Active));
    }

    #[test]
    fn create_list_transition_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::new(dir.path());
        let g = store.create("学英语口语", &now()).unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert!(store.dir(&g.id).join("GOAL.md").exists());
        assert!(store.dir(&g.id).join("files").is_dir());
        assert!(store.dir(&g.id).join("hidden_files").is_dir());
        let g = store.transition(&g.id, GoalStatus::Paused, &now()).unwrap();
        assert_eq!(g.status, GoalStatus::Paused);
        let g = store.transition(&g.id, GoalStatus::Active, &now()).unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        let g = store.transition(&g.id, GoalStatus::Completed, &now()).unwrap();
        assert_eq!(g.status, GoalStatus::Completed);
        assert!(store.transition(&g.id, GoalStatus::Active, &now()).is_err(), "terminal state must not reopen");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.get("no-such-goal").is_err());
        assert!(store.get("../escape").is_err());
    }

    #[test]
    fn attention_levels_follow_the_rule() {
        let t = now();
        let old = t - chrono::Duration::days(10);
        assert_eq!(attention_level(GoalStatus::Active, 1, &old, &t), Attention::UserActionRequired);
        assert_eq!(attention_level(GoalStatus::Active, 0, &t, &t), Attention::NewInformation);
        assert_eq!(attention_level(GoalStatus::Active, 0, &old, &t), Attention::DoesNotNeedAttention);
        assert_eq!(attention_level(GoalStatus::Completed, 3, &t, &t), Attention::DoesNotNeedAttention);
        assert_eq!(attention_level(GoalStatus::Abandoned, 3, &t, &t), Attention::DoesNotNeedAttention);
        assert_eq!(attention_level(GoalStatus::Paused, 0, &old, &t), Attention::DoesNotNeedAttention);
    }

    #[test]
    fn goal_md_parses_and_rejects_bad_status() {
        let g = Goal::parse("goal_x", "# 标题\nstatus: paused\ncreated: 2026-11-03T08:00:00Z\nupdated: 2026-11-03T09:00:00Z\n\n一些笔记\n").unwrap();
        assert_eq!(g.title, "标题");
        assert_eq!(g.status, GoalStatus::Paused);
        assert_eq!(g.body, "一些笔记");
        assert!(Goal::parse("goal_x", "# 标题\nstatus: done\ncreated: 2026-11-03T08:00:00Z\n").is_err());
        assert!(Goal::parse("goal_x", "status: active\ncreated: 2026-11-03T08:00:00Z\n").is_err());
    }
}
