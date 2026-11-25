//! Eval fixtures must not leak into the product.
//!
//! A harness fix that hard-codes an evaluation's data (a ticket id, a
//! contact address) passes that world without making the agent better
//! anywhere else. This test makes the "fix the capability, not the
//! scenario" rule mechanical for the clearest case:
//!
//! * markers are extracted automatically from every file under
//!   `eval/worlds/` with generic patterns (email addresses and
//!   ticket-style ids such as `AB-1234`), so a new world is covered the
//!   day it lands, with no per-scenario list to maintain;
//! * the scan covers everything that can reach the model or steer the
//!   agent at runtime: Rust sources of every product crate (all crates
//!   except `hatch-eval`) and the bundled prompts, skills, config
//!   example and verification rules;
//! * code that never ships is skipped by what the compiler skips, not
//!   by file name: comments, items gated by `#[cfg(test)]` /
//!   `#[cfg(all(test, …))]`, modules declared under such a gate, and
//!   files carrying `#![cfg(test)]`.
//!
//! Markers from a private fixture set (hidden rule packs kept outside
//! the repository) can be added with `HATCH_LEAK_PRIVATE_FIXTURES=<dir>`;
//! their hits are reported by file only, never by the marker text.
//!
//! This catches hard-coded data. Overfitting to a task's *structure*
//! is out of its reach — that is what held-out tasks are for.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

struct Patterns {
    email: Regex,
    ident: Regex,
}

impl Patterns {
    fn new() -> Self {
        Patterns {
            email: Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+").unwrap(),
            // ASCII word boundaries: an id glued to CJK text (`发给HT-2291`) still counts.
            ident: Regex::new(r"(?-u:\b)[A-Za-z]{2,}-\d{3,}(?-u:\b)").unwrap(),
        }
    }

    /// Distinctive identifiers in a fixture text.
    fn markers(&self, text: &str) -> BTreeSet<String> {
        self.email
            .find_iter(text)
            .chain(self.ident.find_iter(text))
            .map(|m| m.as_str().to_string())
            .collect()
    }
}

fn markers_under(dir: &Path, pats: &Patterns) -> BTreeSet<String> {
    let mut files = Vec::new();
    walk(dir, &mut files);
    files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|t| pats.markers(&t))
        .collect()
}

// ------------------------------------------------------------ Rust lexing

/// Per character: the source with comments blanked out, and whether the
/// character sits inside a string or char literal.
fn lex(src: &str) -> (Vec<char>, Vec<bool>) {
    let s: Vec<char> = src.chars().collect();
    let mut code = s.clone();
    let mut lit = vec![false; s.len()];
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let next = s.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < s.len() && s[i] != '\n' {
                code[i] = ' ';
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < s.len() {
                if s[i] == '/' && s.get(i + 1) == Some(&'*') {
                    depth += 1;
                    code[i] = ' ';
                    code[i + 1] = ' ';
                    i += 2;
                } else if s[i] == '*' && s.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    code[i] = ' ';
                    code[i + 1] = ' ';
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    code[i] = ' ';
                    i += 1;
                }
            }
        } else if c == 'r'
            && (i == 0 || !ident(s[i - 1]) || (s[i - 1] == 'b' && (i < 2 || !ident(s[i - 2]))))
            && matches!(next, Some('"') | Some('#'))
        {
            // Raw string: r"…", r#"…"#, br"…".
            let mut j = i + 1;
            let mut hashes = 0;
            while j < s.len() && s[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < s.len() && s[j] == '"' {
                j += 1;
                loop {
                    if j >= s.len() {
                        break;
                    }
                    if s[j] == '"' && (1..=hashes).all(|k| s.get(j + k) == Some(&'#')) {
                        j += hashes + 1;
                        break;
                    }
                    j += 1;
                }
                for flag in lit.iter_mut().take(j.min(s.len())).skip(i) {
                    *flag = true;
                }
                i = j;
            } else {
                i += 1;
            }
        } else if c == '"' {
            let mut j = i + 1;
            while j < s.len() && s[j] != '"' {
                if s[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(s.len());
            for flag in lit.iter_mut().take(end).skip(i) {
                *flag = true;
            }
            i = end;
        } else if c == '\'' {
            // Char literal ('x', '\n', '\u{..}') or a lifetime ('a).
            let end = if next == Some('\\') {
                // Skip the escaped character itself (it may be a quote).
                let mut j = i + 3;
                while j < s.len() && s[j] != '\'' {
                    j += 1;
                }
                Some(j)
            } else if s.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match end {
                Some(e) => {
                    let e = (e + 1).min(s.len());
                    for flag in lit.iter_mut().take(e).skip(i) {
                        *flag = true;
                    }
                    i = e;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    (code, lit)
}

fn starts_at(code: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    code.len() >= i + p.len() && code[i..i + p.len()] == p[..]
}

const TEST_GATES: [&str; 2] = ["#[cfg(test)]", "#[cfg(all(test"];
const FILE_TEST_GATES: [&str; 2] = ["#![cfg(test)]", "#![cfg(all(test"];

/// End (inclusive) of the item that follows a test gate at `from`: the
/// first `;` outside brackets, or the block opened by the first `{`.
fn item_end(code: &[char], lit: &[bool], from: usize) -> usize {
    let (mut paren, mut bracket) = (0i32, 0i32);
    let mut j = from;
    while j < code.len() {
        if !lit[j] {
            match code[j] {
                '(' => paren += 1,
                ')' => paren -= 1,
                '[' => bracket += 1,
                ']' => bracket -= 1,
                ';' if paren == 0 && bracket == 0 => return j,
                // A gated field or arm that is the last in its block.
                '}' if paren == 0 && bracket == 0 => return j.saturating_sub(1),
                '{' if paren == 0 && bracket == 0 => {
                    let mut depth = 0i32;
                    while j < code.len() {
                        if !lit[j] {
                            match code[j] {
                                '{' => depth += 1,
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        return j;
                                    }
                                }
                                _ => {}
                            }
                        }
                        j += 1;
                    }
                    return code.len().saturating_sub(1);
                }
                _ => {}
            }
        }
        j += 1;
    }
    code.len().saturating_sub(1)
}

#[allow(clippy::needless_range_loop)] // indexes code, lit and removed in step
/// The parts of a Rust source that ship: comments and test-gated items
/// removed, string literals kept (prompts live in them). `None` when the
/// whole file is test-only.
fn runtime_rust(src: &str) -> Option<String> {
    let (code, lit) = lex(src);
    if file_gated(&code, &lit) {
        return None;
    }
    let mut removed = vec![false; code.len()];
    for i in 0..code.len() {
        if lit[i] || removed[i] || code[i] != '#' {
            continue;
        }
        if let Some(g) = TEST_GATES.iter().find(|g| starts_at(&code, i, g)) {
            // Skip past the gate attribute itself before looking for the item.
            let after = i + g.chars().count();
            let gate_end = if g.ends_with(']') { after } else { item_end_bracket(&code, &lit, after) };
            let end = item_end(&code, &lit, gate_end);
            for flag in removed.iter_mut().take(end + 1).skip(i) {
                *flag = true;
            }
        }
    }
    Some(
        code.iter()
            .zip(&removed)
            .filter(|(_, r)| !**r)
            .map(|(c, _)| *c)
            .collect(),
    )
}

/// The file opens (after comments and other inner attributes) with an
/// inner test gate: `#![cfg(test)]` before any item.
fn file_gated(code: &[char], lit: &[bool]) -> bool {
    let mut i = 0;
    loop {
        while i < code.len() && code[i].is_whitespace() {
            i += 1;
        }
        if !starts_at(code, i, "#![") {
            return false;
        }
        if FILE_TEST_GATES.iter().any(|g| starts_at(code, i, g)) {
            return true;
        }
        // Skip this inner attribute up to its closing `]`.
        let mut depth = 0i32;
        while i < code.len() {
            if !lit[i] {
                match code[i] {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
    }
}

/// For a gate written as an open prefix (`#[cfg(all(test`), the index just
/// past its closing `]`.
fn item_end_bracket(code: &[char], lit: &[bool], from: usize) -> usize {
    // The prefix already opened one `[` and two `(`.
    let (mut paren, mut bracket) = (2i32, 1i32);
    let mut j = from;
    while j < code.len() {
        if !lit[j] {
            match code[j] {
                '(' => paren += 1,
                ')' => paren -= 1,
                '[' => bracket += 1,
                ']' => {
                    bracket -= 1;
                    if bracket == 0 && paren == 0 {
                        return j + 1;
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    code.len()
}

/// Module files declared under a test gate (`#[cfg(test)] mod x;`) in
/// `file`: `x.rs` and the `x/` directory, by rustc's default module
/// layout (`#[path]` overrides are not followed).
fn test_module_paths(file: &Path, src: &str) -> Vec<PathBuf> {
    let re = Regex::new(r"#\[cfg\((?:test|all\(test[^\]]*)\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").unwrap();
    let (code, _) = lex(src);
    let code: String = code.into_iter().collect();
    let dir = file.parent().unwrap_or(Path::new("."));
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let base = if matches!(stem, "mod" | "lib" | "main") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    re.captures_iter(&code)
        .flat_map(|c| {
            let name = c[1].to_string();
            [base.join(format!("{name}.rs")), base.join(name)]
        })
        .collect()
}

// --------------------------------------------------------------- the scan

/// (path relative to the repo, shipped text) of every runtime file.
fn shipped_files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for dir in ["prompts", "skills", "config", "verification/rules"] {
        walk(&root.join(dir), &mut files);
    }
    let mut rust = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("crates")) {
        for e in rd.flatten() {
            if e.file_name() != "hatch-eval" {
                walk(&e.path().join("src"), &mut rust);
            }
        }
    }
    let mut test_only: Vec<PathBuf> = Vec::new();
    for p in rust.iter().filter(|p| p.extension().is_some_and(|e| e == "rs")) {
        if let Ok(src) = std::fs::read_to_string(p) {
            test_only.extend(test_module_paths(p, &src));
        }
    }
    files.extend(rust);
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    files
        .iter()
        .filter(|p| !test_only.iter().any(|t| p.starts_with(t)))
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            if p.extension().is_some_and(|e| e == "rs") {
                runtime_rust(&text).map(|t| (rel(p), t))
            } else {
                Some((rel(p), text))
            }
        })
        .collect()
}

fn hits(markers: &BTreeSet<String>, files: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in files {
        for m in markers {
            if text.contains(m.as_str()) {
                out.entry(m.clone()).or_default().push(path.clone());
            }
        }
    }
    out
}

#[test]
fn product_code_contains_no_eval_fixture_identifiers() {
    let root = repo_root();
    let pats = Patterns::new();
    let markers = markers_under(&root.join("eval/worlds"), &pats);
    assert!(markers.len() >= 20, "marker extraction looks broken: {markers:?}");
    let files = shipped_files(&root);
    assert!(files.len() >= 100, "scan found only {} shipped files", files.len());
    assert!(
        files.iter().any(|(p, _)| p.starts_with("prompts/")),
        "bundled prompts must be scanned"
    );

    let found = hits(&markers, &files);
    assert!(
        found.is_empty(),
        "eval fixture identifiers appear in shipped code (fix the capability, not the scenario):\n{}",
        found
            .iter()
            .map(|(m, ps)| format!("  {m}: {}", ps.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    );

    if let Ok(dir) = std::env::var("HATCH_LEAK_PRIVATE_FIXTURES") {
        let private = markers_under(Path::new(&dir), &pats);
        assert!(!private.is_empty(), "no markers found under HATCH_LEAK_PRIVATE_FIXTURES");
        let found = hits(&private, &files);
        let places: BTreeSet<&String> = found.values().flatten().collect();
        assert!(
            found.is_empty(),
            "{} private fixture marker(s) appear in shipped code, in: {}",
            found.len(),
            places.into_iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
}

#[test]
fn markers_are_emails_and_ticket_ids() {
    let m = Patterns::new().markers("把HT-2291的修订稿发给 zhou.ning@corp.example。utf-8 与 v2 不算。");
    assert_eq!(
        m.into_iter().collect::<Vec<_>>(),
        vec!["HT-2291".to_string(), "zhou.ning@corp.example".to_string()]
    );
}

#[test]
fn runtime_rust_drops_comments_and_test_items_but_keeps_literals() {
    let src = r##"
// HT-1001 in a comment
/* HT-1002 /* nested */ still comment */
const PROMPT: &str = "keep HT-1003";
const RAW: &str = r#"keep { HT-1004"#;
fn lifetimes<'a>(x: &'a str) -> char { let _ = x; '{' }
#[cfg(test)]
fn only_in_tests() { let s = "}"; let _ = "HT-1005"; }
#[cfg(all(test, unix))]
#[allow(dead_code)]
mod gated { const X: &str = "HT-1006"; }
#[cfg(test)]
mod tests;
fn after() -> &'static str { "keep HT-1007" }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = ("HT-1008", '}'); }
}
"##;
    let out = runtime_rust(src).unwrap();
    for keep in ["HT-1003", "HT-1004", "HT-1007", "fn lifetimes", "fn after"] {
        assert!(out.contains(keep), "{keep} dropped:\n{out}");
    }
    for drop in ["HT-1001", "HT-1002", "HT-1005", "HT-1006", "HT-1008", "only_in_tests"] {
        assert!(!out.contains(drop), "{drop} kept:\n{out}");
    }
    assert!(runtime_rust("//! doc\n#![allow(dead_code)]\n#![cfg(test)]\nconst X: &str = \"HT-1009\";").is_none());
    // An inner gate inside a nested module does not hide the whole file.
    let nested = runtime_rust("mod m { #![cfg(test)] }\nconst Y: &str = \"HT-1010\";").unwrap();
    assert!(nested.contains("HT-1010"));
    // Escaped quote char literal does not swallow what follows.
    let quote = runtime_rust("const Q: char = '\\'';\nconst Z: &str = \"HT-1011\";").unwrap();
    assert!(quote.contains("HT-1011"), "{quote}");
}

#[test]
fn test_gated_module_declarations_resolve_to_files() {
    let src = "pub mod live;\n#[cfg(test)]\nmod p1_drill;\n#[cfg(test)]\n#[path = \"x\"]\npub(crate) mod other_drill;\n";
    let paths = test_module_paths(Path::new("crates/x/src/commands/mod.rs"), src);
    assert!(paths.contains(&PathBuf::from("crates/x/src/commands/p1_drill.rs")));
    assert!(paths.contains(&PathBuf::from("crates/x/src/commands/other_drill")));
    assert!(!paths.iter().any(|p| p.to_string_lossy().contains("live")));
    let nested = test_module_paths(Path::new("crates/x/src/memory.rs"), "#[cfg(test)]\nmod tests;\n");
    assert_eq!(nested[0], PathBuf::from("crates/x/src/memory/tests.rs"));
}
