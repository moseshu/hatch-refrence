//! Distinctive identifiers in evaluation fixtures.
//!
//! One definition shared by the fixture-leak audit (no fixture data in
//! shipped code) and the held-out independence check (a held-out pack
//! must not reuse the repository worlds' data).

use regex::Regex;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

fn patterns() -> &'static (Regex, Regex) {
    static P: OnceLock<(Regex, Regex)> = OnceLock::new();
    P.get_or_init(|| {
        (
            Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+").expect("static regex"),
            // ASCII word boundaries: an id glued to CJK text (`发给HT-2291`) still counts.
            Regex::new(r"(?-u:\b)[A-Za-z]{2,}-\d{3,}(?-u:\b)").expect("static regex"),
        )
    })
}

/// Email addresses and ticket-style ids (`AB-1234`) in `text`.
pub fn fixture_markers(text: &str) -> BTreeSet<String> {
    let (email, ident) = patterns();
    email
        .find_iter(text)
        .chain(ident.find_iter(text))
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Every readable file under `dir`, recursively, as (relative path, text),
/// sorted by path.
pub fn text_files(dir: &Path) -> Vec<(String, String)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else if let Ok(t) = std::fs::read_to_string(&p) {
                let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                out.push((rel, t));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Markers in every text file under `dir`.
pub fn markers_under(dir: &Path) -> BTreeSet<String> {
    text_files(dir).iter().flat_map(|(_, t)| fixture_markers(t)).collect()
}

/// SHA-256 over every file's relative path and content: identifies the
/// exact fixture a run used without revealing it.
pub fn fingerprint(dir: &Path) -> String {
    let files = text_files(dir);
    let parts: Vec<&str> = files.iter().flat_map(|(p, t)| [p.as_str(), t.as_str()]).collect();
    hatch_core::ids::digest(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_emails_and_ticket_ids() {
        let m = fixture_markers("把HT-2291的修订稿发给 zhou.ning@corp.example。utf-8 与 v2 不算。");
        assert_eq!(
            m.into_iter().collect::<Vec<_>>(),
            vec!["HT-2291".to_string(), "zhou.ning@corp.example".to_string()]
        );
    }

    #[test]
    fn fingerprint_changes_with_content_and_paths() {
        let a = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.txt"), "one").unwrap();
        let first = fingerprint(a.path());
        assert_eq!(first, fingerprint(a.path()));
        std::fs::write(a.path().join("x.txt"), "two").unwrap();
        assert_ne!(first, fingerprint(a.path()));
        std::fs::rename(a.path().join("x.txt"), a.path().join("y.txt")).unwrap();
        assert_ne!(first, fingerprint(a.path()));
    }
}
