//! A small POSIX shell lexer for the safety floor.
//!
//! It splits a command line into the simple commands a shell would run,
//! with quotes removed and substitutions replaced by markers: `~` and
//! `$HOME` become [`HOME_MARKER`], anything else substituted at run time
//! becomes [`DYNAMIC_MARKER`]. Words used as data (an echo, a commit
//! message, a heredoc body) are never commands. `synced` is false when
//! the line could not be read to its end (an unclosed quote or heredoc,
//! nesting too deep): the rest was not read, so the caller must not
//! treat it as clear.
//!
//! Semantics follow OpenMausBot's `server/safety-floor.ts` (POSIX
//! reading only; hatch has no Windows host).

/// Lexed marker for the home directory: an unquoted `~` (a quoted `'~'`
/// is only a name), `$HOME`.
pub(crate) const HOME_MARKER: char = '\u{E000}';
/// Lexed marker for anything else substituted at run time; a word
/// holding it is unknown.
pub(crate) const DYNAMIC_MARKER: char = '\u{E001}';

pub(crate) const MAX_NESTING: usize = 40;

/// One simple command: the words the shell would run, and the targets
/// of output redirections.
#[derive(Clone, Debug, Default)]
pub(crate) struct LexedCommand {
    pub words: Vec<String>,
    pub writes: Vec<String>,
}

/// The reading of a whole command line.
#[derive(Clone, Debug)]
pub(crate) struct Reading {
    pub commands: Vec<LexedCommand>,
    /// The text outside every quote, heredoc and comment.
    pub bare: String,
    /// False when something never closed or nested too deep: the rest
    /// was not read.
    pub synced: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Next {
    #[default]
    Argument,
    Write,
    Read,
    Heredoc,
    HeredocTabs,
}

pub(crate) struct Heredoc {
    pub(crate) end: String,
    pub(crate) tabs: bool,
}

#[derive(Default)]
pub(crate) struct Script {
    pub(crate) command: LexedCommand,
    pub(crate) word: Option<String>,
    pub(crate) next: Next,
    pub(crate) depth: usize,
}

impl Script {
    pub(crate) fn add(&mut self, text: &str) {
        if self.word.is_none() {
            self.word = Some(String::new());
        }
        self.word.as_mut().unwrap().push_str(text);
    }

    pub(crate) fn add_char(&mut self, c: char) {
        if self.word.is_none() {
            self.word = Some(String::new());
        }
        self.word.as_mut().unwrap().push(c);
    }
}

pub(crate) struct Lexer {
    pub(crate) chars: Vec<char>,
    pub(crate) at: usize,
    pub(crate) nesting: usize,
    pub(crate) synced: bool,
    pub(crate) bare: String,
    pub(crate) commands: Vec<LexedCommand>,
    pub(crate) heredocs: Vec<Heredoc>,
}

/// Split a command line into every simple command a shell would run.
pub(crate) fn lex(source: &str) -> Reading {
    let mut lx = Lexer {
        chars: source.chars().collect(),
        at: 0,
        nesting: 0,
        synced: true,
        bare: String::new(),
        commands: Vec::new(),
        heredocs: Vec::new(),
    };
    lx.script(None);
    if !lx.heredocs.is_empty() {
        lx.synced = false;
    }
    Reading {
        commands: lx.commands,
        bare: lx.bare,
        synced: lx.synced,
    }
}
