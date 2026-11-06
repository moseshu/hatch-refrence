//! Finding the real command inside wrappers and assignments.
//!
//! A model can hide `rm -rf /` behind `sudo`, `env`, `sh -c` or a
//! leading assignment; the safety floor must judge the command that
//! actually runs, not the first word. This module resolves those
//! wrappers the way the shell would, and reports "could not read"
//! (via `on_unread`) when an option might take a value it cannot see.
//!
//! Semantics follow OpenMausBot's `server/safety-floor.ts` (POSIX
//! reading only; hatch has no Windows host).

/// Shell words that precede a command without being one.
const KEYWORDS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!", "{"];

/// A command that runs the command after it. `values`: short options
/// taking a value; `named`: long options taking a value; `operands`:
/// positional arguments to skip (`timeout 5 …`); `asks`: options that
/// only report on the command (`sudo -l`, `command -v`); `split`: the
/// option whose value is more arguments (`env -S`).
struct PrefixSpec {
    values: &'static str,
    named: &'static [&'static str],
    operands: usize,
    asks: &'static str,
    asks_named: &'static [&'static str],
    split: Option<(char, &'static str)>,
}

const fn spec(
    values: &'static str,
    named: &'static [&'static str],
    operands: usize,
    asks: &'static str,
    asks_named: &'static [&'static str],
    split: Option<(char, &'static str)>,
) -> PrefixSpec {
    PrefixSpec { values, named, operands, asks, asks_named, split }
}

fn prefix_spec(program: &str) -> Option<PrefixSpec> {
    match program {
        // `sudo -l rm` lists whether rm may be run; it runs nothing.
        "sudo" => Some(spec(
            "CDghpRrtTUu",
            &["user", "group", "host", "prompt", "chdir", "chroot", "role", "type", "other-user", "close-from", "command-timeout"],
            0,
            "l",
            &["list"],
            None,
        )),
        "doas" => Some(spec("Cu", &[], 0, "", &[], None)),
        "env" => Some(spec("uCS", &["unset", "chdir", "split-string"], 0, "", &[], Some(('S', "split-string")))),
        "nice" => Some(spec("n", &["adjustment"], 0, "", &[], None)),
        "timeout" => Some(spec("ks", &["kill-after", "signal"], 1, "", &[], None)),
        "exec" => Some(spec("a", &[], 0, "", &[], None)),
        // `command -v rm` looks a command up; it does not run it.
        "command" => Some(spec("", &[], 0, "vV", &[], None)),
        "nohup" | "time" | "builtin" | "busybox" => Some(spec("", &[], 0, "", &[], None)),
        _ => None,
    }
}

/// The program name of a word: after the last `/` or `\`, lowercased,
/// without a Windows executable suffix.
pub(crate) fn program_name(word: &str) -> String {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let lower = base.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".com"))
        .or_else(|| lower.strip_suffix(".cmd"))
        .or_else(|| lower.strip_suffix(".bat"))
        .map(str::to_string)
        .unwrap_or(lower)
}

fn is_assignment(word: &str) -> bool {
    let mut parts = word.splitn(2, '=');
    let Some(name) = parts.next() else {
        return false;
    };
    if parts.next().is_none() {
        return false;
    }
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
        return false;
    }
    let name = name.strip_suffix('+').unwrap_or(name);
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty()
}

/// The words `env -S` makes of its value, or `None`: env itself
/// expands `$NAME`, backslash escapes and `#` comments.
fn split_words(value: &str) -> Option<Vec<String>> {
    if value.contains('$') || value.contains('\\') || value.contains('#') || value.contains(crate::shell_lex::DYNAMIC_MARKER) {
        return None;
    }
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = '\0';
    for ch in value.chars() {
        if quote != '\0' {
            if ch == quote {
                quote = '\0';
            } else {
                word.as_mut().unwrap().push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = ch;
            if word.is_none() {
                word = Some(String::new());
            }
        } else if ch.is_whitespace() {
            if let Some(w) = word.take() {
                words.push(w);
            }
        } else {
            if word.is_none() {
                word = Some(String::new());
            }
            word.as_mut().unwrap().push(ch);
        }
    }
    if quote != '\0' {
        return None;
    }
    if let Some(w) = word {
        words.push(w);
    }
    Some(words)
}

/// Index of the command word past assignments, keywords and prefix
/// commands, or `None`. Rewrites `env -S LINE` in `words` into the
/// arguments env makes of it, and calls `on_unread` when it cannot.
pub(crate) fn command_index(words: &mut Vec<String>, on_unread: &mut dyn FnMut()) -> Option<usize> {
    let mut index = 0;
    while index < words.len() {
        let word = words[index].clone();
        if is_assignment(&word) || KEYWORDS.contains(&word.as_str()) {
            index += 1;
            continue;
        }
        if word == "function" {
            index += 2;
            continue;
        }
        let Some(spec) = prefix_spec(&program_name(&word)) else {
            return Some(index);
        };
        index += 1;
        while index < words.len() && words[index].len() > 1 && words[index].starts_with('-') {
            let option = words[index].clone();
            index += 1;
            if option == "--" {
                break;
            }
            if let Some(long) = option.strip_prefix("--") {
                if spec.asks_named.contains(&long) {
                    return None;
                }
                let (name, equals) = match long.find('=') {
                    Some(eq) => (&long[..eq], Some(long[eq + 1..].to_string())),
                    None => (long, None),
                };
                if let Some((_, split_name)) = spec.split {
                    if !name.is_empty() && split_name.starts_with(name) {
                        let (line, spliced) = match &equals {
                            Some(v) => (v.clone(), 1),
                            None => (words.get(index).cloned().unwrap_or_default(), 2),
                        };
                        let Some(parts) = split_words(&line) else {
                            on_unread();
                            return None;
                        };
                        let pos = index - 1;
                        let end = (pos + spliced).min(words.len());
                        words.splice(pos..end, parts);
                        index = pos;
                        continue;
                    }
                }
                if spec.named.contains(&long) {
                    index += 1;
                } else if !name.is_empty() && equals.is_none() && spec.named.iter().any(|known| known.starts_with(name)) {
                    // getopt_long accepts any unambiguous start
                    // (`sudo --us root`), so the next word may be its
                    // value: never clear.
                    on_unread();
                }
                continue;
            }
            // A short-option cluster: the first letter taking a value
            // ends it; the value is the rest (`-uroot`) or the next
            // word (`-nu root`).
            let letters: Vec<char> = option[1..].chars().collect();
            let value_at = letters.iter().position(|l| spec.values.contains(*l));
            let before = &letters[..value_at.unwrap_or(letters.len())];
            if before.iter().any(|l| spec.asks.contains(*l)) {
                return None;
            }
            if let Some(vi) = value_at {
                if Some(letters[vi]) == spec.split.map(|(letter, _)| letter) {
                    let rest: String = letters[vi + 1..].iter().collect();
                    let (line, spliced) = if !rest.is_empty() {
                        (rest, 1)
                    } else {
                        (words.get(index).cloned().unwrap_or_default(), 2)
                    };
                    let Some(parts) = split_words(&line) else {
                        on_unread();
                        return None;
                    };
                    let pos = index - 1;
                    let end = (pos + spliced).min(words.len());
                    words.splice(pos..end, parts);
                    index = pos;
                    continue;
                }
                if vi == letters.len() - 1 {
                    index += 1;
                }
            }
        }
        index += spec.operands;
    }
    None
}

/// The command line a shell is handed with `-c`, or `None`.
pub(crate) fn inline_script(args: &[String]) -> Option<String> {
    let mut inline = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            continue;
        }
        if arg.len() > 2 && arg.starts_with("--") && arg[2..].chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            if arg == "--rcfile" || arg == "--init-file" {
                i += 1;
            }
            i += 1;
            continue;
        }
        if arg.len() >= 2 && (arg.starts_with('-') || arg.starts_with('+')) && arg[1..].chars().all(|c| c.is_ascii_alphabetic()) {
            if arg.starts_with('-') && arg.contains('c') {
                inline = true;
            }
            // `-o pipefail` and `-O extglob` take a value.
            if arg.ends_with('o') || arg.ends_with('O') {
                i += 1;
            }
            i += 1;
            continue;
        }
        return if inline { Some(arg.clone()) } else { None };
    }
    None
}
