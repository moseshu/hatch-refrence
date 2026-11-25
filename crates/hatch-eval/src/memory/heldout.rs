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

use super::{MemoryWorld, TargetCategory};
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

/// Targets whose declared category or placement contradicts what the
/// retrieval system will actually see. Warnings, not errors: a world may
/// deliberately test the fixed profile. Messages name targets by their
/// position and category only, never by content, so they are safe to
/// show for held-out packs.
///
/// Measured the way the product measures: the claim's index text
/// (`hatch_memory::doc_text`) and the session's first user message are
/// tokenized with `hatch_memory::tokenize`; a "word" is any index term
/// other than a single CJK character (those carry little weight in
/// ranking). Only the first message counts because recall runs on each
/// turn's own input and the held-out summary reports the first request.
/// `literal` is checked on whole identifiers in the raw text (see
/// [`identifiers`]), not on index terms, which split `HT-2291` into `ht`
/// and `2291`. The fixed profile is the oldest `PROFILE_LIMIT` claims
/// tagged with one of `PROFILE_TAGS`.
pub fn target_warnings(world: &MemoryWorld) -> Vec<String> {
    use hatch_agent::recall::{PROFILE_LIMIT, PROFILE_TAGS};
    use hatch_core::tokens::is_cjk;
    use std::collections::HashMap;

    let spec = &world.spec;
    let is_unigram = |t: &str| {
        let mut cs = t.chars();
        matches!((cs.next(), cs.next()), (Some(c), None) if is_cjk(c))
    };
    let mut tagged: Vec<(&str, usize)> = spec
        .seed_claims
        .iter()
        .enumerate()
        .filter(|(_, c)| c.tags.iter().any(|t| PROFILE_TAGS.contains(&t.as_str())))
        .map(|(i, c)| (c.at.as_str(), i))
        .collect();
    // By instant, like the store's `ORDER BY created_at` (RFC3339 strings
    // with different offsets do not sort as text).
    tagged.sort_by_key(|(at, _)| hatch_core::time::parse_rfc3339(at).ok());
    let profile: BTreeSet<usize> = tagged.iter().take(PROFILE_LIMIT).map(|(_, i)| *i).collect();
    let by_key: HashMap<&str, usize> = spec
        .seed_claims
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.key.as_deref().map(|k| (k, i)))
        .collect();
    // Loading rejects sessions without turns, so the first one exists.
    let requests: HashMap<&str, &str> = spec
        .days
        .iter()
        .flat_map(|d| d.sessions.iter())
        .filter_map(|s| s.turns.first().map(|t| (s.id.as_str(), t.as_str())))
        .collect();

    let mut out = Vec::new();
    for (n, t) in spec.targets.iter().enumerate() {
        let n = n + 1;
        let (Some(&i), Some(request)) = (by_key.get(t.key.as_str()), requests.get(t.session.as_str())) else {
            continue;
        };
        let c = &spec.seed_claims[i];
        let label = t.category.map(TargetCategory::label).unwrap_or("未标注");
        if profile.contains(&i) {
            out.push(format!(
                "目标 {n}（{label}）在固定注入里（最早的 {PROFILE_LIMIT} 条带标签记忆之一）：每轮都会注入，测不到召回。要测召回，把它的时间改晚或去掉标签。"
            ));
        }
        let doc = hatch_memory::doc_text(&c.subject, &c.text, &c.tags);
        let claim_terms: BTreeSet<String> = hatch_memory::tokenize(&doc).into_iter().collect();
        let shared: Vec<String> = hatch_memory::tokenize(request)
            .into_iter()
            .collect::<BTreeSet<String>>()
            .intersection(&claim_terms)
            .cloned()
            .collect();
        let words = shared.iter().filter(|w| !is_unigram(w)).count();
        let shared_ids = identifiers(request).intersection(&identifiers(&doc)).count();
        let problem = match t.category {
            Some(TargetCategory::Unmentioned) if words > 0 => Some(format!(
                "和请求有 {words} 个共同的词，检索看来属于词面交集；完全没提应当只剩零星单字"
            )),
            Some(TargetCategory::Paraphrase) if words > 0 => Some(format!(
                "和请求有 {words} 个共同的词，检索看来属于词面交集；换了说法应当只剩零星单字"
            )),
            Some(TargetCategory::Lexical) if words == 0 => {
                Some("和请求没有共同的词（只有单字），检索看来属于换了说法或完全没提".into())
            }
            Some(TargetCategory::Literal) if shared_ids == 0 => {
                Some("和首条请求没有共同的完整标识（字母和数字组成的编号、代码，或邮箱地址），检索看来不是字面标识".into())
            }
            _ => None,
        };
        if let Some(p) = problem {
            out.push(format!("目标 {n}（{label}）{p}。"));
        }
    }
    out
}

/// Whole identifiers in `text`, lowercased: runs of ASCII letters and
/// digits joined by `-` `_` `.` `/` `@` that mix letters with digits
/// (`HT-2291`, `v2.3`) or are email addresses. Bare numbers (years,
/// amounts, extensions) are left out: two texts sharing `2026` or `30`
/// do not name the same thing.
fn identifiers(text: &str) -> BTreeSet<String> {
    const JOINERS: &[char] = &['-', '_', '.', '/', '@'];
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || JOINERS.contains(&ch)))
        .map(|run| run.trim_matches(JOINERS))
        .filter(|run| {
            let email = run.split_once('@').is_some_and(|(user, host)| !user.is_empty() && host.contains('.'));
            let code = run.chars().any(|ch| ch.is_ascii_alphabetic()) && run.chars().any(|ch| ch.is_ascii_digit());
            email || code
        })
        .map(str::to_lowercase)
        .collect()
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

    #[test]
    fn the_pressure_world_targets_match_their_categories() {
        let w = MemoryWorld::load(&repo_worlds().join("memory-pressure")).unwrap();
        assert_eq!(target_warnings(&w), Vec::<String>::new());
    }

    #[test]
    fn mislabelled_and_profile_targets_are_flagged_by_position_only() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("pack");
        copy_dir(&repo_worlds().join("memory-pressure"), &pack);
        let path = pack.join("memory.json");
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The lexical target (signature) relabelled as unmentioned.
        v["targets"][0]["category"] = "unmentioned".into();
        // The unmentioned target (peanut) moved before every other tagged claim.
        for c in v["seed_claims"].as_array_mut().unwrap() {
            if c["key"] == "peanut" {
                c["at"] = "2026-08-01T09:00:00+08:00".into();
            }
        }
        std::fs::write(&path, v.to_string()).unwrap();
        let w = MemoryWorld::load(&pack).unwrap();
        let warnings = target_warnings(&w);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("目标 1（完全没提）") && warnings[0].contains("共同的词"), "{warnings:?}");
        assert!(warnings[1].starts_with("目标 4（完全没提）") && warnings[1].contains("固定注入"), "{warnings:?}");
        for t in &w.spec.targets {
            assert!(warnings.iter().all(|m| !m.contains(&t.key) && !m.contains(&t.session)));
        }
    }

    #[test]
    fn literal_targets_are_judged_on_the_first_message_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = tmp.path().join("pack");
        copy_dir(&repo_worlds().join("memory-pressure"), &pack);
        let path = pack.join("memory.json");
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The contract number only arrives in a second message.
        for d in v["days"].as_array_mut().unwrap() {
            for s in d["sessions"].as_array_mut().unwrap() {
                if s["id"] == "contract-revision" {
                    let original = s["turns"][0].clone();
                    s["turns"] = serde_json::json!(["帮我把合同修订稿发给对接人。", original]);
                }
            }
        }
        std::fs::write(&path, v.to_string()).unwrap();
        let warnings = target_warnings(&MemoryWorld::load(&pack).unwrap());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("目标 2（字面标识）") && warnings[0].contains("标识"), "{warnings:?}");
    }

    #[test]
    fn identifiers_are_whole_codes_and_addresses_not_shared_numbers() {
        let ids = identifiers("合同HT-2291（2026 年）的对接人 zhou.ning@corp.example，分机 8021。");
        let want: BTreeSet<String> = ["ht-2291", "zhou.ning@corp.example"].map(String::from).into();
        assert_eq!(ids, want);
        // Same digits, different code: not the same identifier.
        assert_eq!(identifiers("PO-2291 在 2026 年").intersection(&identifiers("HT-2291 2026")).count(), 0);
        assert!(identifiers("HT-22910").is_disjoint(&identifiers("HT-2291")));
    }
}
