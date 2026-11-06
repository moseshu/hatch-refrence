fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '_' | ':' | '.')
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-')
}

fn skip_ws(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// An identifier: up to 64 characters. Empty when `chars[i]` cannot
/// start one.
fn read_ident(chars: &[char], start: usize) -> (String, usize) {
    if start >= chars.len() || !is_ident_start(chars[start]) {
        return (String::new(), start);
    }
    let mut end = start + 1;
    while end < chars.len() && end - start < 64 && is_ident_char(chars[end]) {
        end += 1;
    }
    (chars[start..end].iter().collect(), end)
}

/// A function that pipes itself into itself in the background, then
/// runs: `name() { name | name & }; name`. Both the plain and the
/// `function name` spellings count.
pub(crate) fn is_fork_bomb(bare: &str) -> bool {
    let chars: Vec<char> = bare.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_ident_start(chars[i]) {
            i += 1;
            continue;
        }
        let boundary = i == 0
            || matches!(
                chars[i - 1],
                ' ' | '\t' | '\n' | '\r' | ';' | '&' | '|' | '(' | ')' | '{' | '}'
            );
        let (name, mut j) = read_ident(&chars, i);
        if boundary {
            // Plain form: `name() { … }`.
            if try_bomb(&chars, j, &name) {
                return true;
            }
            // `function name … { … }`.
            if name == "function" {
                let after_ws = skip_ws(&chars, j);
                if after_ws > j {
                    j = after_ws;
                    let (fname, j2) = read_ident(&chars, j);
                    if !fname.is_empty() {
                        j = skip_ws(&chars, j2);
                        if chars.get(j) == Some(&'(') {
                            j = skip_ws(&chars, j + 1);
                            if chars.get(j) == Some(&')') {
                                j = skip_ws(&chars, j + 1);
                            } else {
                                j = usize::MAX;
                            }
                        }
                        if j != usize::MAX && try_bomb_body(&chars, j, &fname) {
                            return true;
                        }
                    }
                }
            }
        }
        i = j.max(i + 1);
    }
    false
}

/// `name() { name | name & }` (the `()` part), then the body.
fn try_bomb(chars: &[char], mut j: usize, name: &str) -> bool {
    j = skip_ws(chars, j);
    if chars.get(j) != Some(&'(') {
        return false;
    }
    j = skip_ws(chars, j + 1);
    if chars.get(j) != Some(&')') {
        return false;
    }
    try_bomb_body(chars, skip_ws(chars, j + 1), name)
}

/// `{ name | name & }; name`.
fn try_bomb_body(chars: &[char], mut j: usize, name: &str) -> bool {
    j = skip_ws(chars, j);
    if chars.get(j) != Some(&'{') {
        return false;
    }
    j = skip_ws(chars, j + 1);
    let (n1, j2) = read_ident(chars, j);
    if n1 != name {
        return false;
    }
    j = skip_ws(chars, j2);
    if chars.get(j) != Some(&'|') {
        return false;
    }
    j = skip_ws(chars, j + 1);
    let (n2, j2) = read_ident(chars, j);
    if n2 != name {
        return false;
    }
    j = skip_ws(chars, j2);
    if chars.get(j) != Some(&'&') {
        return false;
    }
    j = skip_ws(chars, j + 1);
    if chars.get(j) != Some(&'}') {
        return false;
    }
    j = skip_ws(chars, j + 1);
    if chars.get(j) == Some(&';') {
        j = skip_ws(chars, j + 1);
    }
    let (n3, j2) = read_ident(chars, j);
    if n3 != name {
        return false;
    }
    // The invocation must end the name: a longer name does not count.
    !(j2 < chars.len() && is_ident_char(chars[j2]))
}
