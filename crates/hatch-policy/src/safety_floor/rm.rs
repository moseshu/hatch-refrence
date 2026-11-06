//! `rm` targets: what a recursive `rm` deletes, and whether the
//! spelled-out path is the root, the home directory or a system
//! directory.

use super::{delete_home, delete_root, delete_system, FloorVerdict, Host};
use crate::shell_lex::{DYNAMIC_MARKER, HOME_MARKER};

pub(crate) fn rm_targets(args: &[String]) -> Option<Vec<String>> {
    let mut targets = Vec::new();
    let mut recursive = false;
    let mut operands_only = false;
    for arg in args {
        if operands_only || !arg.starts_with('-') || arg.len() < 2 {
            targets.push(arg.clone());
        } else if arg == "--" {
            operands_only = true;
        } else if is_rm_recursive_flag(arg) {
            recursive = true;
        }
    }
    if recursive {
        Some(targets)
    } else {
        None
    }
}

fn is_rm_recursive_flag(arg: &str) -> bool {
    // `-r` or `-R`, alone or in a cluster of rm's letters; `--recursive`
    // or any start of it GNU rm accepts.
    if arg.len() > 2 && "--recursive".starts_with(arg) {
        return true;
    }
    let body = &arg[1..];
    let mut seen_r = false;
    for c in body.chars() {
        if !seen_r {
            if c == 'r' || c == 'R' {
                seen_r = true;
            } else if !"dfiIPvWx".contains(c) {
                return false;
            }
        } else if !"dfiIPRrvWx".contains(c) {
            return false;
        }
    }
    seen_r
}

/// Resolves `.` and `..` by spelling and drops a final `*`.
fn resolved(path: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            parts.pop();
        } else {
            parts.push(seg.to_string());
        }
    }
    if matches!(parts.last().map(|s| s.as_str()), Some("*") | Some("*.*")) {
        parts.pop();
    }
    parts
}

const SYSTEM_DIRECTORIES: &[&str] = &[
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/var",
    "/boot",
    "/home",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/lib*",
    "/System",
    "/Library",
    "/Applications",
    "/Users",
];
/// macOS keeps `/etc` and `/var` under `/private`.
const MAC_DIRECTORIES: &[&str] = &["/private", "/private/etc", "/private/var"];

/// The rule a recursive delete of this POSIX path meets, or `None`.
pub(crate) fn posix_target(word: &str, host: &Host) -> Option<FloorVerdict> {
    if word.contains(DYNAMIC_MARKER) {
        return None;
    }
    let from_home = word.starts_with(HOME_MARKER);
    let path = if from_home {
        &word[HOME_MARKER.len_utf8()..]
    } else {
        word
    };
    if path.contains(HOME_MARKER) || !(path.starts_with('/') || (from_home && path.is_empty())) {
        return None;
    }
    let parts = resolved(path);
    // `~/..` holds home; `~/../name` cannot be placed, so it is allowed.
    if from_home {
        return if parts.is_empty() {
            Some(delete_home())
        } else {
            None
        };
    }
    if parts.is_empty() {
        return Some(delete_root());
    }
    let spelled = format!("/{}", parts.join("/"));
    let same = |l: &str, r: &str| {
        if host.fold_case {
            l.eq_ignore_ascii_case(r)
        } else {
            l == r
        }
    };
    let mut system = SYSTEM_DIRECTORIES.to_vec();
    if host.mac {
        system.extend_from_slice(MAC_DIRECTORIES);
    }
    if let Some(dir) = system.iter().find(|d| same(d, &spelled)) {
        return Some(delete_system(dir));
    }
    if let Some(home) = &host.home {
        let home = home.trim_end_matches('/');
        if home.starts_with('/') && same(home, &spelled) {
            return Some(delete_home());
        }
    }
    None
}
