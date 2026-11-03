//! `hatch-skills` — skill files (`SKILL.md` with frontmatter).
//!
//! A skill is a directory containing a `SKILL.md` file. The file starts
//! with a `---` fenced frontmatter block (`key: value` lines, hand-rolled
//! parser — no YAML dependency), followed by the markdown body that is the
//! skill's instructions.
//!
//! * [`parse`]   — [`parse_skill`], the frontmatter parser.
//! * [`registry`] — [`SkillRegistry`], discovery (`scan`) and loading.

pub mod parse;
pub mod registry;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use parse::parse_skill;
pub use registry::{skill_dirs, SkillRegistry};

/// Maximum accepted size of a `SKILL.md` file, in bytes.
pub const MAX_SKILL_BYTES: usize = 65_536;

/// A fully loaded skill: metadata plus its markdown instruction body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub scope: String,
    pub version: String,
    pub body: String,
    pub path: PathBuf,
}

/// Metadata only — what [`SkillRegistry::scan`] returns. Never the body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub scope: String,
    pub version: String,
}

impl From<&Skill> for SkillMeta {
    fn from(skill: &Skill) -> Self {
        Self {
            name: skill.name.clone(),
            description: skill.description.clone(),
            tools: skill.tools.clone(),
            scope: skill.scope.clone(),
            version: skill.version.clone(),
        }
    }
}

impl From<Skill> for SkillMeta {
    fn from(skill: Skill) -> Self {
        SkillMeta::from(&skill)
    }
}
