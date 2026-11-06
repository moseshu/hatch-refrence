//! Disk devices: which `/dev` paths are disks, which are scratch
//! devices a formatter may touch, and which words a formatter
//! would write to.

use crate::shell_lex::{DYNAMIC_MARKER, HOME_MARKER};

/// Collapse repeated slashes so `/dev//sda` reads as `/dev/sda`.
fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    out
}

pub(crate) fn is_disk_device(word: &str) -> bool {
    if !word.is_ascii() {
        return false;
    }
    let path = collapse_slashes(word);
    let Some(rest) = path.strip_prefix("/dev/") else {
        return false;
    };
    let b = rest.as_bytes();
    let digits = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let letters = |mut i: usize, max: usize| {
        let mut n = 0;
        while i < b.len() && b[i].is_ascii_lowercase() && n < max {
            i += 1;
            n += 1;
        }
        i
    };
    // `r?disk` + digits + (`s` + digits)*.
    let mut j = if b.first() == Some(&b'r') { 1 } else { 0 };
    if rest[j..].starts_with("disk") {
        j += 4;
        let k = digits(j);
        if k > j {
            j = k;
            while rest[j..].starts_with('s') {
                let k = digits(j + 1);
                if k > j + 1 {
                    j = k;
                } else {
                    break;
                }
            }
            if j == b.len() {
                return true;
            }
        }
    }
    // `disk/` + anything.
    if rest.starts_with("disk/") && rest.len() > 5 {
        return true;
    }
    // `sd` + 1-3 letters + digits.
    if rest.starts_with("sd") {
        let k = letters(2, 3);
        if (3..=5).contains(&k) && digits(k) == b.len() {
            return true;
        }
    }
    // `hd` + 1 letter + digits.
    if rest.starts_with("hd") {
        let k = letters(2, 1);
        if k == 3 && digits(k) == b.len() {
            return true;
        }
    }
    // `x?vd` + 1-2 letters + digits.
    let mut j = if b.first() == Some(&b'x') { 1 } else { 0 };
    if rest[j..].starts_with("vd") {
        j += 2;
        let k = letters(j, 2);
        if k > j && k <= j + 2 && digits(k) == b.len() {
            return true;
        }
    }
    // `nvme` + digits + (`n` + digits)? + (`p` + digits)?.
    if rest.starts_with("nvme") {
        let mut k = digits(4);
        if k > 4 {
            if rest[k..].starts_with('n') {
                let k2 = digits(k + 1);
                if k2 > k + 1 {
                    k = k2;
                }
            }
            if rest[k..].starts_with('p') {
                let k2 = digits(k + 1);
                if k2 > k + 1 {
                    k = k2;
                }
            }
            if k == b.len() {
                return true;
            }
        }
    }
    // `mmcblk` + digits + (`p` + digits)?.
    if rest.starts_with("mmcblk") {
        let mut k = digits(6);
        if k > 6 {
            if rest[k..].starts_with('p') {
                let k2 = digits(k + 1);
                if k2 > k + 1 {
                    k = k2;
                }
            }
            if k == b.len() {
                return true;
            }
        }
    }
    // `md` + digits + (`p` + digits)?, or `md/` + a name.
    if let Some(md) = rest.strip_prefix("md") {
        if !md.is_empty() && md.starts_with('/') {
            if md.len() > 1 && !md[1..].contains('/') {
                return true;
            }
        } else {
            let mut k = digits(2);
            if k > 2 {
                if rest[k..].starts_with('p') {
                    let k2 = digits(k + 1);
                    if k2 > k + 1 {
                        k = k2;
                    }
                }
                if k == b.len() {
                    return true;
                }
            }
        }
    }
    // `dm-` + digits.
    if rest.starts_with("dm-") {
        let k = digits(3);
        if k > 3 && k == b.len() {
            return true;
        }
    }
    // `mapper/` + a name that is not a loop partition.
    if let Some(m) = rest.strip_prefix("mapper/") {
        if !m.is_empty()
            && !m.contains('/')
            && !(m.starts_with("loop")
                && m[4..].chars().next().is_some_and(|c| c.is_ascii_digit()))
        {
            return true;
        }
    }
    false
}

/// Devices a disk image is built on: loop, RAM, network block, zram,
/// and loop partitions under /dev/mapper.
fn is_scratch_device(word: &str) -> bool {
    let path = collapse_slashes(word);
    let Some(rest) = path.strip_prefix("/dev/") else {
        return false;
    };
    let rest = rest.strip_prefix("mapper/").unwrap_or(rest);
    ["loop", "ram", "nbd", "zram"].iter().any(|p| {
        rest.starts_with(p)
            && rest[p.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
    })
}

/// `mkfs` on any device but a scratch one erases it; an image file or
/// a variable's value is not known to be a device.
pub(crate) fn formats_device(word: &str) -> bool {
    let path = collapse_slashes(word);
    path.starts_with("/dev/")
        && path.len() > 5
        && !path.contains(HOME_MARKER)
        && !path.contains(DYNAMIC_MARKER)
        && !is_scratch_device(word)
}
