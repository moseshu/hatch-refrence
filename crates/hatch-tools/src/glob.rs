//! Glob matching and a bounded workspace walk.

use std::path::{Path, PathBuf};

/// Directories never descended into by list/glob/grep.
pub const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".hatch", "__pycache__", ".venv"];

/// Match a `/`-separated relative path against a glob with `**`, `*` and `?`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.trim_start_matches("./").split('/').filter(|s| !s.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| match_segments(rest, &s[i..])),
        Some((seg, rest)) => !s.is_empty() && match_one(seg.as_bytes(), s[0].as_bytes()) && match_segments(rest, &s[1..]),
    }
}

fn match_one(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => match_one(&p[1..], s) || (!s.is_empty() && match_one(p, &s[1..])),
        (Some(b'?'), Some(_)) => match_one(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => match_one(&p[1..], &s[1..]),
        _ => false,
    }
}

/// Walk `dir` (not following symlinks), returning regular files and
/// directories up to `limit` entries. The bool is `true` when truncated.
pub fn walk(dir: &Path, max_depth: usize, limit: usize) -> (Vec<(PathBuf, bool)>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = e.path();
            if ft.is_dir() {
                if SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()) {
                    continue;
                }
                out.push((path.clone(), true));
                if depth + 1 < max_depth {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file() {
                out.push((path, false));
            }
            if out.len() >= limit {
                return (out, true);
            }
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/a/lib.rs"));
        assert!(glob_match("**/*.rs", "src/a/lib.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"));
        assert!(glob_match("docs/**", "docs/a/b.md"));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("*.md", "notes.txt"));
    }

    #[test]
    fn walk_skips_noise_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/sub")).unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join("src/sub/a.rs"), "x").unwrap();
        std::fs::write(d.path().join(".git/HEAD"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", d.path().join("etc-link")).unwrap();
        let (all, truncated) = walk(d.path(), 10, 100);
        assert!(!truncated);
        let names: Vec<String> = all.iter().map(|(p, _)| p.strip_prefix(d.path()).unwrap().display().to_string()).collect();
        assert!(names.contains(&"src/sub/a.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains(".git") || n.contains("etc-link")));
        assert!(walk(d.path(), 10, 1).1);
    }
}
