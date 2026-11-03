//! Discovery and loading of skills from skill directories.
//!
//! A skill directory holds one subdirectory per skill, each containing a
//! `SKILL.md` file. [`SkillRegistry::scan`] lists metadata only (never the
//! body); [`SkillRegistry::load`] reads and parses one skill by name.

use crate::parse::{parse_skill, valid_skill_name};
use crate::{Skill, SkillMeta};
use hatch_core::{Error, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The directories searched for skills, in order:
///
/// 1. `<home>/skills` — the user's installed skills.
/// 2. `./skills` — project-local skills, relative to the working directory.
pub fn skill_dirs(home: &Path) -> Vec<PathBuf> {
    vec![home.join("skills"), PathBuf::from("./skills")]
}

/// Discovers skills in a list of directories (first directory wins on
/// duplicate names).
pub struct SkillRegistry {
    dirs: Vec<PathBuf>,
}

impl SkillRegistry {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    /// Registry over [`skill_dirs`] for the given home directory.
    pub fn from_home(home: &Path) -> Self {
        Self::new(skill_dirs(home))
    }

    /// List metadata for every skill found, sorted by name.
    ///
    /// Each directory is scanned for subdirectories containing a
    /// `SKILL.md`. Directories with an unreadable or malformed
    /// `SKILL.md` are silently skipped. On duplicate names the first
    /// directory wins. The body is never returned.
    pub fn scan(&self) -> Result<Vec<SkillMeta>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<SkillMeta> = Vec::new();
        for dir in &self.dirs {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(_) => continue, // Missing/unreadable dir: skip it.
            };
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }
                let file = skill_dir.join("SKILL.md");
                let text = match fs::read_to_string(&file) {
                    Ok(text) => text,
                    Err(_) => continue, // Unreadable: skip.
                };
                let skill = match parse_skill(&text, &file) {
                    Ok(skill) => skill,
                    Err(_) => continue, // Malformed: skip.
                };
                if !seen.insert(skill.name.clone()) {
                    continue; // Duplicate name: first dir wins.
                }
                out.push(SkillMeta::from(skill));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Load one skill by name, searching the directories in order.
    ///
    /// Returns [`Error::Invalid`] when no skill with that name exists.
    pub fn load(&self, name: &str) -> Result<Skill> {
        if !valid_skill_name(name) {
            return Err(Error::Invalid(format!("unknown skill: {name:?}")));
        }
        for dir in &self.dirs {
            let file = dir.join(name).join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let text = fs::read_to_string(&file)
                .map_err(|e| Error::Invalid(format!("cannot read skill {name}: {e}")))?;
            return parse_skill(&text, &file);
        }
        Err(Error::Invalid(format!("unknown skill: {name}")))
    }
}

/// File recording which skills are active for chat turns:
/// `<home>/skills/active.json` (a JSON list of skill names).
pub fn active_file(home: &Path) -> PathBuf {
    home.join("skills").join("active.json")
}

/// Names of currently active skills (empty when the file is missing).
/// A corrupt file is treated as empty rather than failing the turn.
pub fn active_skills(home: &Path) -> Vec<String> {
    let text = match fs::read_to_string(active_file(home)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(names) => names.into_iter().filter(|n| valid_skill_name(n)).collect(),
        Err(_) => Vec::new(),
    }
}

fn write_active(home: &Path, names: &[String]) -> Result<()> {
    let dir = home.join("skills");
    fs::create_dir_all(&dir)
        .map_err(|e| Error::Invalid(format!("cannot create {}: {e}", dir.display())))?;
    let text = serde_json::to_string(names).map_err(Error::from)?;
    fs::write(active_file(home), text)
        .map_err(|e| Error::Invalid(format!("cannot write active skills: {e}")))?;
    Ok(())
}

/// Activate a skill for future chat turns (persists the name).
pub fn activate(home: &Path, name: &str) -> Result<()> {
    if !valid_skill_name(name) {
        return Err(Error::Invalid(format!("unknown skill: {name:?}")));
    }
    let mut names = active_skills(home);
    if !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
    write_active(home, &names)
}

/// Deactivate a skill. Returns true when it was active.
pub fn deactivate(home: &Path, name: &str) -> Result<bool> {
    let mut names = active_skills(home);
    let before = names.len();
    names.retain(|n| n != name);
    if names.len() == before {
        return Ok(false);
    }
    write_active(home, &names)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
    }

    fn fixture() -> (TempDir, TempDir) {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        // "beta" only in second dir; "alpha" in both (first wins).
        write_skill(first.path(), "alpha", "First alpha.", "Alpha body, first.");
        write_skill(
            second.path(),
            "alpha",
            "Second alpha.",
            "Alpha body, second.",
        );
        write_skill(second.path(), "beta", "Beta skill.", "Beta body.");
        // Malformed and unreadable entries must be skipped, not fail scan.
        let bad = second.path().join("broken");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "no frontmatter here\n").unwrap();
        fs::create_dir_all(second.path().join("empty-dir")).unwrap();
        (first, second)
    }

    #[test]
    fn scan_lists_metadata_sorted() {
        let (first, second) = fixture();
        let reg = SkillRegistry::new(vec![
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);
        let metas = reg.scan().unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].name, "alpha");
        assert_eq!(metas[0].description, "First alpha.");
        assert_eq!(metas[1].name, "beta");
        assert_eq!(metas[1].description, "Beta skill.");
    }

    #[test]
    fn scan_skips_broken_and_empty_dirs() {
        let (_first, second) = fixture();
        let reg = SkillRegistry::new(vec![second.path().to_path_buf()]);
        let metas = reg.scan().unwrap();
        // "broken" (malformed) and "empty-dir" (no SKILL.md) are skipped.
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().any(|m| m.name == "alpha"));
        assert!(metas.iter().any(|m| m.name == "beta"));
    }

    #[test]
    fn scan_skips_missing_dirs() {
        let reg = SkillRegistry::new(vec![PathBuf::from("/nonexistent/skills-xyz")]);
        assert!(reg.scan().unwrap().is_empty());
    }

    #[test]
    fn first_dir_wins_dedupe() {
        let (first, second) = fixture();
        let reg = SkillRegistry::new(vec![
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]);
        let skill = reg.load("alpha").unwrap();
        assert_eq!(skill.description, "First alpha.");
        assert!(skill.body.contains("first"));
    }

    #[test]
    fn load_returns_body() {
        let (first, _second) = fixture();
        let reg = SkillRegistry::new(vec![first.path().to_path_buf()]);
        let skill = reg.load("alpha").unwrap();
        assert_eq!(skill.name, "alpha");
        assert!(skill.body.contains("Alpha body, first."));
        assert!(skill.path.ends_with("alpha/SKILL.md"));
    }

    #[test]
    fn load_unknown_skill_errors() {
        let (first, _second) = fixture();
        let reg = SkillRegistry::new(vec![first.path().to_path_buf()]);
        let err = reg.load("nope").unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert!(err.to_string().contains("unknown skill"));
    }

    #[test]
    fn load_rejects_path_traversal_names() {
        let (first, _second) = fixture();
        let reg = SkillRegistry::new(vec![first.path().to_path_buf()]);
        assert!(reg.load("../alpha").is_err());
        assert!(reg.load("a/b").is_err());
    }

    #[test]
    fn skill_dirs_order() {
        let home = Path::new("/home/moses");
        assert_eq!(
            skill_dirs(home),
            vec![
                PathBuf::from("/home/moses/skills"),
                PathBuf::from("./skills")
            ]
        );
    }

    #[test]
    fn from_home_uses_skill_dirs() {
        let home = TempDir::new().unwrap();
        let skills = home.path().join("skills");
        write_skill(&skills, "home-skill", "From home.", "Home body.");
        let reg = SkillRegistry::from_home(home.path());
        assert_eq!(reg.load("home-skill").unwrap().name, "home-skill");
    }
}
