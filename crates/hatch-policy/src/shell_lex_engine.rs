//! The shell lexer engine: the `Lexer` state machine that walks a
//! command line one character at a time. The types it builds live in
//! [`super::shell_lex`].

use super::shell_lex::{Heredoc, Lexer, Next, Script, DYNAMIC_MARKER, HOME_MARKER, MAX_NESTING};
use std::mem;

impl Lexer {
    fn end_of(&self, mark: char, from: usize) -> usize {
        (from..self.chars.len())
            .find(|&i| self.chars[i] == mark)
            .unwrap_or(self.chars.len())
    }

    /// `$HOME` (and `${HOME:?}` / `${HOME?message}`) is the home
    /// directory; every other variable is unknown at read time.
    fn variable(name: &str) -> char {
        let Some(rest) = name.strip_prefix("HOME") else {
            return DYNAMIC_MARKER;
        };
        if rest.is_empty() {
            return HOME_MARKER;
        }
        let rest = rest.strip_prefix(':').unwrap_or(rest);
        if rest.starts_with('?') && !rest.contains('}') {
            return HOME_MARKER;
        }
        DYNAMIC_MARKER
    }

    /// Returns the text up to the matching `shut`. Inside `"${…}"`
    /// double quotes still quote but `'` is literal.
    fn balanced(&mut self, open: char, shut: char, in_double_quotes: bool) -> String {
        let from = self.at;
        let mut depth = 1;
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            if c == '\\' {
                self.at += 2;
            } else if c == '\'' && !in_double_quotes {
                self.at = self.end_of('\'', self.at + 1);
                self.at += 1;
            } else if c == '"' {
                self.at += 1;
                while self.at < self.chars.len() && self.chars[self.at] != '"' {
                    if self.chars[self.at] == '\\' {
                        self.at += 1;
                    }
                    self.at += 1;
                }
                self.at += 1;
            } else if c == open {
                depth += 1;
                self.at += 1;
            } else if c == shut {
                depth -= 1;
                if depth == 0 {
                    let text: String = self.chars[from..self.at].iter().collect();
                    self.at += 1;
                    return text;
                }
                self.at += 1;
            } else {
                self.at += 1;
            }
        }
        self.synced = false;
        self.chars[from..].iter().collect()
    }

    fn variable_at(&mut self, in_double_quotes: bool) -> char {
        let next = self.chars.get(self.at + 1).copied();
        if next == Some('{') {
            self.at += 2;
            let name = self.balanced('{', '}', in_double_quotes);
            return Self::variable(&name);
        }
        if next == Some('\'') && !in_double_quotes {
            // `$'…'` decodes escapes such as `\x2f`, so its text is not read here.
            self.at += 2;
            while self.at < self.chars.len() && self.chars[self.at] != '\'' {
                if self.chars[self.at] == '\\' {
                    self.at += 1;
                }
                self.at += 1;
            }
            if self.at >= self.chars.len() {
                self.synced = false;
            }
            self.at += 1;
            return DYNAMIC_MARKER;
        }
        if next.is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            let start = self.at + 1;
            let mut end = start + 1;
            while end < self.chars.len()
                && end - start < 129
                && (self.chars[end].is_ascii_alphanumeric() || self.chars[end] == '_')
            {
                end += 1;
            }
            let name: String = self.chars[start..end].iter().collect();
            self.at = end;
            return Self::variable(&name);
        }
        if let Some(n) = next {
            if n.is_ascii_digit() || matches!(n, '@' | '*' | '#' | '?' | '$' | '!' | '_' | '-') {
                self.at += 2;
                return DYNAMIC_MARKER;
            }
        }
        self.at += 1;
        '$'
    }

    fn dollar(&mut self, in_double_quotes: bool) -> char {
        if self.chars.get(self.at + 1) != Some(&'(') {
            return self.variable_at(in_double_quotes);
        }
        self.at += 2;
        self.nested(')');
        DYNAMIC_MARKER
    }

    fn nested(&mut self, shut: char) {
        self.nesting += 1;
        if self.nesting > MAX_NESTING {
            self.at = self.chars.len();
            self.synced = false;
        } else if !self.script(Some(shut)) {
            self.synced = false;
        }
        self.nesting -= 1;
    }

    fn quoted(&mut self, st: &mut Script) {
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            let after = self.chars.get(self.at + 1).copied();
            if c == '"' {
                self.at += 1;
                return;
            }
            if c == '\\' && after.is_some_and(|a| "$`\"\\\n".contains(a)) {
                if after != Some('\n') {
                    st.add_char(after.unwrap());
                }
                self.at += 2;
            } else if c == '$' {
                let m = self.dollar(true);
                st.add_char(m);
            } else if c == '`' {
                self.at += 1;
                self.nested('`');
                st.add_char(DYNAMIC_MARKER);
            } else {
                st.add_char(c);
                self.at += 1;
            }
        }
        self.synced = false;
    }

    fn end_word(&mut self, st: &mut Script) {
        let Some(w) = st.word.take() else {
            return;
        };
        match st.next {
            Next::Argument => st.command.words.push(w),
            Next::Write => st.command.writes.push(w),
            Next::Read => {}
            Next::Heredoc => self.heredocs.push(Heredoc {
                end: w,
                tabs: false,
            }),
            Next::HeredocTabs => self.heredocs.push(Heredoc { end: w, tabs: true }),
        }
        st.next = Next::Argument;
    }

    fn cut(&mut self, st: &mut Script, skip: usize) {
        self.end_word(st);
        st.next = Next::Argument;
        if !st.command.words.is_empty() || !st.command.writes.is_empty() {
            self.commands.push(mem::take(&mut st.command));
        }
        self.at += skip;
    }

    /// A `&n` / `&-` file-descriptor duplication after a redirection.
    fn duplicate(&mut self) -> bool {
        if self.chars.get(self.at) != Some(&'&') {
            return false;
        }
        let mut i = self.at + 1;
        while i < self.chars.len()
            && i < self.at + 12
            && (self.chars[i].is_ascii_digit() || self.chars[i] == '-')
        {
            i += 1;
        }
        if i > self.at + 1 {
            self.at = i;
            return true;
        }
        self.at += 1;
        false
    }

    /// Reads commands until `close`; false when it ended anywhere else.
    pub(crate) fn script(&mut self, close: Option<char>) -> bool {
        let mut st = Script::default();
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            let after = self.chars.get(self.at + 1).copied();
            if close == Some(c) && st.depth == 0 {
                self.cut(&mut st, 1);
                return true;
            }
            if c == '\\' {
                if let Some(a) = after {
                    if a != '\n' {
                        st.add_char(a);
                    }
                }
                self.at += 2;
            } else if c == '\'' {
                let end = self.end_of('\'', self.at + 1);
                if end == self.chars.len() {
                    self.synced = false;
                }
                let text: String = self.chars[self.at + 1..end].iter().collect();
                st.add(&text);
                self.at = end + 1;
            } else if c == '"' {
                st.add("");
                self.at += 1;
                self.quoted(&mut st);
            } else if c == '$' {
                let m = self.dollar(false);
                st.add_char(m);
            } else if c == '`' {
                self.at += 1;
                self.nested('`');
                st.add_char(DYNAMIC_MARKER);
            } else if c == ' ' || c == '\t' || c == '\r' {
                self.end_word(&mut st);
                self.bare.push(c);
                self.at += 1;
            } else if c == '\n' {
                self.cut(&mut st, 1);
                self.bare.push(c);
                // A heredoc body is input, not commands.
                let pending: Vec<Heredoc> = mem::take(&mut self.heredocs);
                for hd in pending {
                    let mut ended = false;
                    while self.at < self.chars.len() && !ended {
                        let end = self.end_of('\n', self.at);
                        let mut line: String = self.chars[self.at..end].iter().collect();
                        if line.ends_with('\r') {
                            line.pop();
                        }
                        self.at = (end + 1).min(self.chars.len());
                        let cmp = if hd.tabs {
                            line.trim_start_matches('\t').to_string()
                        } else {
                            line
                        };
                        ended = cmp == hd.end;
                    }
                    if !ended {
                        self.synced = false;
                    }
                }
            } else if c == '#' && st.word.is_none() {
                self.at = self.end_of('\n', self.at);
            } else if c == ';' || c == '|' || (c == '&' && after != Some('>')) {
                self.bare.push(c);
                self.cut(&mut st, 1);
            } else if c == '('
                && st.word.is_some()
                && !self.empty_parens_ahead()
                && !self.keyword_before_paren(&st)
            {
                // Inside a word `( … )` is data (`name=( … )`), except
                // `name()`, which defines a function, and a leading
                // keyword (`then(…)`), which is followed by a subshell.
                self.at += 1;
                self.balanced('(', ')', false);
                st.add_char(DYNAMIC_MARKER);
            } else if c == '(' || c == ')' {
                self.bare.push(c);
                if c == '(' {
                    st.depth += 1;
                } else {
                    st.depth = st.depth.saturating_sub(1);
                }
                self.cut(&mut st, 1);
            } else if (c == '<' || c == '>') && after == Some('(') {
                self.at += 2;
                self.nested(')');
                st.add_char(DYNAMIC_MARKER);
            } else if c == '<' || c == '>' || c == '&' {
                // Digits right before a redirection name a descriptor (2>).
                match &st.word {
                    Some(w) if !w.is_empty() && w.chars().all(|ch| ch.is_ascii_digit()) => {
                        st.word = None
                    }
                    _ => self.end_word(&mut st),
                }
                self.bare.push(c);
                self.at += if c == '&' { 2 } else { 1 };
                if c != '<' {
                    if self.chars.get(self.at) == Some(&'>')
                        || self.chars.get(self.at) == Some(&'|')
                    {
                        self.at += 1;
                    }
                    st.next = if self.duplicate() {
                        Next::Argument
                    } else {
                        Next::Write
                    };
                } else if self.chars.get(self.at) == Some(&'<') {
                    self.at += 1;
                    let here_string = self.chars.get(self.at) == Some(&'<');
                    let tabs = !here_string && self.chars.get(self.at) == Some(&'-');
                    if self.chars.get(self.at) == Some(&'<') || tabs {
                        self.at += 1;
                    }
                    st.next = if here_string {
                        Next::Read
                    } else if tabs {
                        Next::HeredocTabs
                    } else {
                        Next::Heredoc
                    };
                } else if self.chars.get(self.at) == Some(&'>') {
                    self.at += 1;
                    st.next = Next::Write;
                } else {
                    st.next = if self.duplicate() {
                        Next::Argument
                    } else {
                        Next::Read
                    };
                }
            } else if c == '~' && st.word.is_none() {
                // `~` and `~/…` are the home directory; `~name` is somebody else's.
                let is_home = after.is_none_or(|a| {
                    a.is_whitespace()
                        || matches!(a, '/' | ';' | '&' | '|' | '(' | ')' | '<' | '>' | '`')
                });
                st.add_char(if is_home { HOME_MARKER } else { DYNAMIC_MARKER });
                self.bare.push(c);
                self.at += 1;
            } else {
                st.add_char(c);
                self.bare.push(c);
                self.at += 1;
            }
        }
        self.cut(&mut st, 0);
        if st.depth > 0 {
            self.synced = false;
        }
        close.is_none()
    }

    /// The text after `(` is only whitespace and then `)` (an empty
    /// subshell `()`, as in a function definition).
    fn empty_parens_ahead(&self) -> bool {
        let mut i = self.at + 1;
        while i < self.chars.len() && i < self.at + 40 && self.chars[i].is_whitespace() {
            i += 1;
        }
        self.chars.get(i) == Some(&')')
    }

    /// No words yet and the word in progress is a shell keyword (or
    /// `time`): the `(` that follows starts a subshell, not data.
    fn keyword_before_paren(&self, st: &Script) -> bool {
        if !st.command.words.is_empty() {
            return false;
        }
        let Some(w) = &st.word else {
            return false;
        };
        matches!(
            w.as_str(),
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" | "time"
        )
    }
}
