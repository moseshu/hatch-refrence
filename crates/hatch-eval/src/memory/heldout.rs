//! Held-out packs: memory worlds written independently of the code under
//! test and kept outside the repository.
//!
//! A held-out pack uses the ordinary memory-world format (`memory.json` +
//! `project/`). What makes it held out is how it is produced and used:
//! someone who has not read the implementation writes it, it never enters
//! the repository, and the people changing the code only ever see the
//! aggregate summary of a run (see `MemoryReport::render_heldout_summary`).
//!
//! [`independence_problems`] is the pre-flight check run before any model
//! call: a pack that reuses the repository worlds' data, or that cannot be
//! summarised without its free text, is rejected.

use super::MemoryWorld;
use crate::markers;
use std::collections::BTreeSet;
use std::path::Path;

/// Reasons `pack` cannot serve as a held-out set, empty when it can.
/// `repo_worlds` is the repository's `eval/worlds` directory.
pub fn independence_problems(pack: &MemoryWorld, repo_worlds: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    let pack_dir = pack.dir.canonicalize().unwrap_or_else(|_| pack.dir.clone());
    if let Some(repo_root) = repo_worlds.canonicalize().ok().and_then(|w| w.parent()?.parent().map(Path::to_path_buf)) {
        if pack_dir.starts_with(&repo_root) {
            problems.push(format!(
                "留出集放在仓库目录里（{}）：实现代码的会话能读到它。请放到仓库之外。",
                pack_dir.display()
            ));
        }
    }

    let spec = &pack.spec;
    if spec.targets.is_empty() {
        problems.push("没有 targets：留出摘要按目标记忆汇总，至少要有一个。".into());
    }
    for (i, t) in spec.targets.iter().enumerate() {
        if t.category.is_none() {
            problems.push(format!("目标 {} 没有 category（lexical / literal / paraphrase / unmentioned / other）", i + 1));
        }
        if t.checks.is_empty() {
            problems.push(format!("目标 {} 没有关联 checks：摘要无法报告它的任务结果", i + 1));
        }
    }
    if spec.checks.is_empty() {
        problems.push("没有 checks".into());
    }

    // Shared identifiers with the repository worlds. The persona's own
    // address is shared by convention (every world's user is Alex) and is
    // not task data.
    let mut shared: BTreeSet<String> = markers::markers_under(&pack.dir)
        .intersection(&markers::markers_under(repo_worlds))
        .cloned()
        .collect();
    shared.remove(&spec.persona.email);
    if !shared.is_empty() {
        problems.push(format!(
            "和仓库里的评估世界共用了 {} 个标识（邮箱或编号）：{}",
            shared.len(),
            shared.into_iter().collect::<Vec<_>>().join("、")
        ));
    }

    // Verbatim reuse of repository memories or requests.
    let norm = |s: &str| s.split_whitespace().collect::<String>();
    let mut repo_claims = BTreeSet::new();
    let mut repo_turns = BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(repo_worlds) {
        for e in rd.flatten() {
            let dir = e.path();
            if !MemoryWorld::is_memory_world(&dir) {
                continue;
            }
            let Ok(w) = MemoryWorld::load(&dir) else {
                continue;
            };
            repo_claims.extend(w.spec.seed_claims.iter().map(|c| norm(&c.text)));
            repo_turns.extend(
                w.spec
                    .days
                    .iter()
                    .flat_map(|d| d.sessions.iter())
                    .flat_map(|s| s.turns.iter())
                    .map(|t| norm(t)),
            );
        }
    }
    let reused_claims = spec.seed_claims.iter().filter(|c| repo_claims.contains(&norm(&c.text))).count();
    if reused_claims > 0 {
        problems.push(format!("有 {reused_claims} 条预置记忆和仓库世界里的记忆一字不差"));
    }
    let reused_turns = spec
        .days
        .iter()
        .flat_map(|d| d.sessions.iter())
        .flat_map(|s| s.turns.iter())
        .filter(|t| repo_turns.contains(&norm(t)))
        .count();
    if reused_turns > 0 {
        problems.push(format!("有 {reused_turns} 条用户请求和仓库世界里的请求一字不差"));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_worlds() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds")
    }

    fn copy_dir(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap().flatten() {
            let to = dst.join(e.file_name());
            if e.path().is_dir() {
                copy_dir(&e.path(), &to);
            } else {
                std::fs::copy(e.path(), &to).unwrap();
            }
        }
    }

    #[test]
    fn the_template_is_a_valid_independent_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("pack");
        copy_dir(&repo_worlds().join("../heldout/template"), &pack);
        let w = MemoryWorld::load(&pack).unwrap();
        assert_eq!(independence_problems(&w, &repo_worlds()), Vec::<String>::new());
    }

    #[test]
    fn a_copy_of_a_repository_world_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("pack");
        copy_dir(&repo_worlds().join("memory-pressure"), &pack);
        let w = MemoryWorld::load(&pack).unwrap();
        let problems = independence_problems(&w, &repo_worlds());
        assert!(problems.iter().any(|p| p.contains("共用了")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("预置记忆")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("用户请求")), "{problems:?}");
    }

    #[test]
    fn a_pack_inside_the_repository_is_rejected() {
        let w = MemoryWorld::load(&repo_worlds().join("../heldout/template")).unwrap();
        let problems = independence_problems(&w, &repo_worlds());
        assert!(problems.iter().any(|p| p.contains("仓库之外")), "{problems:?}");
    }
}
