//! A small line diff for approval previews ("what exactly will be written").

/// Render a unified-style diff of `old` → `new` with `context` lines around
/// each change. Falls back to a summary when the inputs are too large for the
/// quadratic LCS table.
pub fn unified(path: &str, old: Option<&str>, new: &str, context: usize) -> String {
    let a: Vec<&str> = old.map(|o| o.lines().collect()).unwrap_or_default();
    let b: Vec<&str> = new.lines().collect();
    let header = match old {
        None => format!("--- /dev/null\n+++ {path}（新文件）\n"),
        Some(_) => format!("--- {path}\n+++ {path}\n"),
    };
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        return format!("{header}（文件过大，不逐行比较：{} 行 → {} 行）\n", a.len(), b.len());
    }
    let ops = lcs_ops(&a, &b);
    if ops.iter().all(|o| matches!(o, Op::Same(_))) {
        return format!("{header}（内容没有变化）\n");
    }
    let mut out = header;
    let mut i = 0;
    while i < ops.len() {
        if matches!(ops[i], Op::Same(_)) {
            i += 1;
            continue;
        }
        // A hunk: back up `context` lines, run until `context` equal lines follow.
        let start = i.saturating_sub(context);
        let mut end = i;
        let mut same_run = 0;
        while end < ops.len() {
            if matches!(ops[end], Op::Same(_)) {
                same_run += 1;
                if same_run > context * 2 {
                    end -= context;
                    break;
                }
            } else {
                same_run = 0;
            }
            end += 1;
        }
        let end = end.min(ops.len());
        out.push_str("@@\n");
        for op in &ops[start..end] {
            match op {
                Op::Same(l) => out.push_str(&format!(" {l}\n")),
                Op::Del(l) => out.push_str(&format!("-{l}\n")),
                Op::Add(l) => out.push_str(&format!("+{l}\n")),
            }
        }
        i = end;
    }
    out
}

enum Op<'a> {
    Same(&'a str),
    Del(&'a str),
    Add(&'a str),
}

fn lcs_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let (n, m) = (a.len(), b.len());
    let mut t = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i][j] = if a[i] == b[j] { t[i + 1][j + 1] + 1 } else { t[i + 1][j].max(t[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Same(a[i]));
            i += 1;
            j += 1;
        } else if t[i + 1][j] >= t[i][j + 1] {
            ops.push(Op::Del(a[i]));
            i += 1;
        } else {
            ops.push(Op::Add(b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| Op::Del(l)));
    ops.extend(b[j..].iter().map(|l| Op::Add(l)));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_only_changed_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\ni\nj\nk\n";
        let d = unified("x.txt", Some(old), new, 1);
        assert!(d.contains("-d\n+D\n"), "{d}");
        assert!(d.contains("+k\n"));
        assert!(!d.contains(" a\n"), "far context is omitted: {d}");
        assert_eq!(d.matches("@@").count(), 2);
    }

    #[test]
    fn new_file_and_no_change() {
        assert!(unified("n.txt", None, "x\n", 2).contains("+x"));
        assert!(unified("n.txt", Some("x\n"), "x\n", 2).contains("没有变化"));
    }
}
