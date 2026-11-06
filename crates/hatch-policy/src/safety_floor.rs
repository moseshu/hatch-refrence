//! Safety floor: six shell command classes that are never auto-approved.
//!
//! A hit does not refuse the action — it sends it to the approval
//! queue, where only a person can approve it. A grant never waives the
//! floor. A command the shell lexer cannot read is treated the same
//! way: never clear, never auto-approved. The floor is always on.
//!
//! The six classes and the lexer semantics follow OpenMausBot's
//! `server/safety-floor.ts` (POSIX reading only; hatch has no Windows
//! host). Words used as data — an echo, a commit message, a heredoc
//! body — are never commands, and a variable's value is unknown, so
//! `rm -rf $DIR` is not a hit; `~` and `$HOME` do mean home.

use crate::command_parse::{command_index, inline_script, program_name};
use crate::shell_lex::{lex, LexedCommand, DYNAMIC_MARKER, HOME_MARKER};

mod bomb;
mod devices;
mod rm;

/// The six never-auto-approve classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloorRule {
    DeleteRoot,
    DeleteHome,
    DeleteSystemDirectory,
    FormatDisk,
    OverwriteDisk,
    ForkBomb,
}

impl FloorRule {
    /// Stable name for the decision log.
    pub fn as_str(&self) -> &'static str {
        match self {
            FloorRule::DeleteRoot => "delete-root",
            FloorRule::DeleteHome => "delete-home",
            FloorRule::DeleteSystemDirectory => "delete-system-directory",
            FloorRule::FormatDisk => "format-disk",
            FloorRule::OverwriteDisk => "overwrite-disk",
            FloorRule::ForkBomb => "fork-bomb",
        }
    }
}

/// What the floor read: clear, a hit, or "could not read".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FloorReading {
    Clear,
    /// Which rule matched, and words a person reads — never the command
    /// itself, which may hold a credential.
    Hit {
        rule: FloorRule,
        label: String,
    },
    Unread,
}

/// A longer command line is reported as not read, without being scanned.
const MAX_COMMAND_LENGTH: usize = 32 * 1024;

/// How deep wrappers are followed (`sh -c "bash -c …"`).
const MAX_DEPTH: usize = 4;

pub(crate) fn delete_root() -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::DeleteRoot,
        label: "recursive delete of the filesystem root".into(),
    }
}

pub(crate) fn delete_home() -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::DeleteHome,
        label: "recursive delete of the home directory".into(),
    }
}

pub(crate) fn delete_system(name: &str) -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::DeleteSystemDirectory,
        label: format!("recursive delete of {name}"),
    }
}

pub(crate) fn format_disk() -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::FormatDisk,
        label: "formatting a disk".into(),
    }
}

pub(crate) fn overwrite_disk() -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::OverwriteDisk,
        label: "overwriting a disk device".into(),
    }
}

pub(crate) fn fork_bomb() -> FloorVerdict {
    FloorVerdict {
        rule: FloorRule::ForkBomb,
        label: "a fork bomb".into(),
    }
}

pub(crate) struct FloorVerdict {
    rule: FloorRule,
    label: String,
}

pub(crate) struct Host {
    /// macOS finds `/ETC` where Linux does not.
    pub(crate) fold_case: bool,
    pub(crate) mac: bool,
    pub(crate) home: Option<String>,
}

struct Pass {
    host: Host,
    synced: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Posix,
    Powershell,
}

/// Reads a shell command line against the never-list; never panics. A
/// caller enforcing the floor must not answer for [`FloorReading::Unread`].
/// `home` lets a spelled-out home path match; a home under a temp
/// directory is not matched by path (deleting it is cleanup).
pub fn floor_reading(command: &str, home: Option<&str>) -> FloorReading {
    if command.trim().is_empty() {
        return FloorReading::Clear;
    }
    if command.len() > MAX_COMMAND_LENGTH {
        return FloorReading::Unread;
    }
    // A command cannot forge the lexer's markers.
    let line: String = command
        .chars()
        .map(|c| {
            if c == HOME_MARKER || c == DYNAMIC_MARKER {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    let mut pass = Pass {
        host: Host {
            fold_case: cfg!(target_os = "macos"),
            mac: cfg!(target_os = "macos"),
            home: home.and_then(|h| {
                let h = h.trim_end_matches('/');
                if h.is_empty() || is_home_scratch(h) {
                    None
                } else {
                    Some(h.to_string())
                }
            }),
        },
        synced: true,
    };
    match scan(&line, &mut pass, Mode::Posix, 0) {
        Some(v) => FloorReading::Hit {
            rule: v.rule,
            label: v.label,
        },
        None if pass.synced => FloorReading::Clear,
        None => FloorReading::Unread,
    }
}

/// A home under a temp directory (HOME=/tmp in a container) is not
/// matched by path: deleting it is cleanup. `~` and `$HOME` still mean home.
fn is_home_scratch(home: &str) -> bool {
    let h = home.to_ascii_lowercase();
    let h = h.trim_end_matches('/');
    [
        "/tmp",
        "/var/tmp",
        "/var/folders",
        "/private/tmp",
        "/private/var/tmp",
        "/private/var/folders",
        "/dev/shm",
    ]
    .iter()
    .any(|p| h == *p || h.starts_with(&format!("{p}/")))
}

fn scan(source: &str, pass: &mut Pass, mode: Mode, depth: usize) -> Option<FloorVerdict> {
    let reading = lex(source);
    if !reading.synced {
        pass.synced = false;
    }
    if mode == Mode::Posix && bomb::is_fork_bomb(&reading.bare) {
        return Some(fork_bomb());
    }
    for cmd in &reading.commands {
        if let Some(v) = judge(cmd, pass, mode, depth) {
            return Some(v);
        }
    }
    None
}

fn run(line: Option<String>, mode: Mode, pass: &mut Pass, depth: usize) -> Option<FloorVerdict> {
    let line = line.filter(|l| !l.is_empty())?;
    if depth >= MAX_DEPTH {
        // Not followed deeper, so unread: never clear.
        pass.synced = false;
        return None;
    }
    scan(&line, pass, mode, depth + 1)
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash", "fish"];

fn judge(cmd: &LexedCommand, pass: &mut Pass, mode: Mode, depth: usize) -> Option<FloorVerdict> {
    if mode == Mode::Posix && cmd.writes.iter().any(|w| devices::is_disk_device(w)) {
        return Some(overwrite_disk());
    }
    let mut words = cmd.words.clone();
    let index = command_index(&mut words, &mut || pass.synced = false)?;
    let program = program_name(&words[index]);
    let args = &words[index + 1..];
    if SHELLS.contains(&program.as_str()) {
        return run(inline_script(args), Mode::Posix, pass, depth);
    }
    // PowerShell 7 off Windows: `rm` is the real rm and Remove-Item
    // takes POSIX paths, so its script is read both ways.
    if program == "pwsh" || program == "powershell" {
        let flag = args
            .iter()
            .position(|a| a.len() > 1 && "-command".starts_with(&a.to_lowercase()));
        let inner = flag.map(|f| args[f + 1..].join(" ")).unwrap_or_default();
        return run(Some(inner.clone()), Mode::Posix, pass, depth)
            .or_else(|| run(Some(inner), Mode::Powershell, pass, depth));
    }
    if mode == Mode::Posix {
        if program == "eval" {
            return run(Some(args.join(" ")), Mode::Posix, pass, depth);
        }
        if program == "rm" {
            if let Some(targets) = rm::rm_targets(args) {
                for target in &targets {
                    if let Some(v) = rm::posix_target(target, &pass.host) {
                        return Some(v);
                    }
                }
            }
            return None;
        }
        if program == "dd" {
            if args
                .iter()
                .any(|a| a.starts_with("of=") && devices::is_disk_device(&a[3..]))
            {
                return Some(overwrite_disk());
            }
            return None;
        }
        if program == "mkfs" || program.starts_with("mkfs.") {
            if args.iter().any(|a| devices::formats_device(a)) {
                return Some(format_disk());
            }
            return None;
        }
        if program == "diskutil" {
            let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            let verb = match operands.first().map(|s| s.to_lowercase()).as_deref() {
                Some("apfs") => operands
                    .get(1)
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            // The disk comes last; one from `$(hdiutil attach …)` is a fresh RAM disk.
            if ["erasedisk", "erasevolume", "reformat"].contains(&verb.as_str())
                && !args.last().is_some_and(|a| a.contains(DYNAMIC_MARKER))
            {
                return Some(format_disk());
            }
            return None;
        }
        return None;
    }
    if mode == Mode::Powershell {
        // Remove-Item's aliases off Windows, where `rm` and `rmdir`
        // are the real programs the POSIX reading judges.
        if !["remove-item", "ri", "rd", "del", "erase"].contains(&program.as_str()) {
            return None;
        }
        let is_switch = |a: &String| a.len() > 1 && a.starts_with('-');
        if !args
            .iter()
            .any(|a| is_switch(a) && "-recurse".starts_with(&a.to_lowercase()))
        {
            return None;
        }
        for target in args {
            if is_switch(target) {
                continue;
            }
            // PowerShell takes either slash in a path.
            let posix = target.replace('\\', "/");
            if let Some(v) = rm::posix_target(&posix, &pass.host) {
                return Some(v);
            }
        }
        return None;
    }
    None
}
