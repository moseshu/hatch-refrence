//! Deterministic evaluation of task requirements against captured evidence.
//! Supply final answers and tool calls from the run record, and artifact or
//! external state from an independent readback. A write receipt alone is
//! evidence for submission, never for the resulting state.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs, io,
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;

use crate::Check;

/// Independent readback can return usable data, known absence, or observed
/// data with an invalid format. Unavailable means the readback did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence<T> {
    Observed(T),
    Absent,
    Invalid,
    Unavailable,
}

impl<T> From<io::Result<T>> for Evidence<T> {
    fn from(result: io::Result<T>) -> Self {
        match result {
            Ok(value) => Self::Observed(value),
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound => Self::Absent,
                io::ErrorKind::InvalidData => Self::Invalid,
                _ => Self::Unavailable,
            },
        }
    }
}

impl<T> Evidence<T> {
    pub fn evaluate(self, predicate: impl FnOnce(T) -> bool) -> Verdict {
        match self {
            Self::Observed(value) => Verdict::from(predicate(value)),
            Self::Absent | Self::Invalid => Verdict::Fail,
            Self::Unavailable => Verdict::InsufficientEvidence,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    InsufficientEvidence,
}

impl From<bool> for Verdict {
    fn from(passed: bool) -> Self {
        if passed {
            Self::Pass
        } else {
            Self::Fail
        }
    }
}

impl Verdict {
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Fail, _) | (_, Self::Fail) => Self::Fail,
            (Self::Pass, Self::Pass) => Self::Pass,
            _ => Self::InsufficientEvidence,
        }
    }

    /// The requirement becomes the stable check name. The detail should
    /// identify the observed evidence or explain why it could not be read.
    pub fn report(self, requirement: &str, detail: impl Into<String>) -> Check {
        Check {
            name: requirement.into(),
            passed: self == Self::Pass,
            detail: detail.into(),
            insufficient_evidence: self == Self::InsufficientEvidence,
        }
    }
}

/// A single independent snapshot of the workspace. An unreadable directory
/// makes any missing entry beneath it inconclusive.
#[derive(Clone, Debug)]
pub struct TextFiles {
    files: BTreeMap<PathBuf, CapturedFile>,
    unavailable: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CapturedFile {
    Bytes(Vec<u8>),
    Invalid,
    InvalidLink(PathBuf),
}

impl TextFiles {
    pub fn capture(root: &Path) -> Self {
        let mut snapshot = Self {
            files: BTreeMap::new(),
            unavailable: BTreeSet::new(),
        };
        match root.canonicalize() {
            Ok(root) => snapshot.walk(&root, &root, Path::new(""), &mut BTreeSet::new()),
            Err(_) => {
                snapshot.unavailable.insert(PathBuf::new());
            }
        }
        snapshot
    }

    fn walk(&mut self, root: &Path, dir: &Path, relative: &Path, active: &mut BTreeSet<PathBuf>) {
        active.insert(dir.to_owned());
        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => self.capture_entry(
                            root,
                            entry.path(),
                            relative.join(entry.file_name()),
                            active,
                        ),
                        Err(_) => {
                            self.unavailable.insert(relative.to_owned());
                        }
                    }
                }
            }
            Err(_) => {
                self.unavailable.insert(relative.to_owned());
            }
        }
        active.remove(dir);
    }

    fn capture_entry(
        &mut self,
        root: &Path,
        mut path: PathBuf,
        relative: PathBuf,
        active: &mut BTreeSet<PathBuf>,
    ) {
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink()) {
            let Ok(link) = fs::read_link(&path) else {
                self.unavailable.insert(relative);
                return;
            };
            match path.canonicalize() {
                Ok(target) if target.starts_with(root) && !active.contains(&target) => {
                    path = target
                }
                Ok(_) => {
                    self.files.insert(relative, CapturedFile::InvalidLink(link));
                    return;
                }
                Err(error) if symlink_loop(&error) => {
                    self.files.insert(relative, CapturedFile::InvalidLink(link));
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let target = path
                        .parent()
                        .expect("workspace entry has a parent")
                        .join(&link);
                    let outside = target
                        .ancestors()
                        .find_map(|part| part.canonicalize().ok())
                        .is_some_and(|ancestor| !ancestor.starts_with(root));
                    if outside {
                        self.files.insert(relative, CapturedFile::InvalidLink(link));
                    }
                    return;
                }
                Err(_) => {
                    self.unavailable.insert(relative);
                    return;
                }
            }
        }
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => self.walk(root, &path, &relative, active),
            Ok(metadata) if metadata.is_file() => match fs::read(path) {
                Ok(bytes) => {
                    self.files.insert(relative, CapturedFile::Bytes(bytes));
                }
                Err(_) => {
                    self.unavailable.insert(relative);
                }
            },
            Ok(_) => {
                self.files.insert(relative, CapturedFile::Invalid);
            }
            Err(_) => {
                self.unavailable.insert(relative);
            }
        }
    }

    fn entry(&self, path: &Path) -> Evidence<&CapturedFile> {
        if let Some(file) = self.files.get(path) {
            return Evidence::Observed(file);
        }
        for ancestor in path.ancestors() {
            if matches!(self.files.get(ancestor), Some(CapturedFile::InvalidLink(_))) {
                return Evidence::Invalid;
            }
            if self.unavailable.contains(ancestor) {
                return Evidence::Unavailable;
            }
        }
        Evidence::Absent
    }

    pub fn read(&self, path: &str) -> Evidence<&str> {
        match self.entry(Path::new(path)) {
            Evidence::Observed(CapturedFile::Bytes(bytes)) => match std::str::from_utf8(bytes) {
                Ok(text) => Evidence::Observed(text),
                Err(_) => Evidence::Invalid,
            },
            Evidence::Observed(_) | Evidence::Invalid => Evidence::Invalid,
            Evidence::Absent => Evidence::Absent,
            Evidence::Unavailable => Evidence::Unavailable,
        }
    }

    pub(crate) fn changed_since(&self, before: &Self) -> Evidence<bool> {
        for path in self.files.keys().chain(before.files.keys()) {
            let new = self.entry(path);
            let old = before.entry(path);
            if new != Evidence::Unavailable && old != Evidence::Unavailable && new != old {
                return Evidence::Observed(true);
            }
        }
        if self.unavailable.is_empty() && before.unavailable.is_empty() {
            Evidence::Observed(false)
        } else {
            Evidence::Unavailable
        }
    }

    pub fn into_files(self) -> BTreeMap<String, String> {
        self.files
            .into_iter()
            .filter_map(|(path, file)| {
                let CapturedFile::Bytes(bytes) = file else {
                    return None;
                };
                let text = String::from_utf8(bytes).ok()?;
                Some((path.to_string_lossy().into_owned(), text))
            })
            .collect()
    }
}

fn symlink_loop(_error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        _error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn file_terms(
    name: &str,
    path: &str,
    file: Evidence<&str>,
    include: &[String],
    exclude: &[String],
) -> Check {
    match file {
        Evidence::Observed(text) => {
            let terms = terms(text, strings(include), strings(exclude));
            Verdict::from(terms.matches()).report(
                name,
                format!("缺少：{:?}；不该出现：{:?}", terms.missing, terms.forbidden),
            )
        }
        Evidence::Absent => Verdict::Fail.report(name, format!("{path} 不存在")),
        Evidence::Invalid => invalid_text(name, path),
        Evidence::Unavailable => {
            Verdict::InsufficientEvidence.report(name, format!("无法读取 {path}"))
        }
    }
}

pub(crate) fn invalid_text(name: &str, path: &str) -> Check {
    Verdict::Fail.report(name, format!("{path} 不是有效的 UTF-8 文本文件"))
}

pub fn file_exists(file: Evidence<()>, expected: bool) -> Verdict {
    match file {
        Evidence::Observed(()) | Evidence::Invalid => expected.into(),
        Evidence::Absent => (!expected).into(),
        Evidence::Unavailable => Verdict::InsufficientEvidence,
    }
}

pub struct Terms<'a> {
    pub missing: Vec<&'a str>,
    pub forbidden: Vec<&'a str>,
}

impl Terms<'_> {
    pub fn matches(&self) -> bool {
        self.missing.is_empty() && self.forbidden.is_empty()
    }
}

pub fn terms<'a>(
    text: &str,
    include: impl IntoIterator<Item = &'a str>,
    exclude: impl IntoIterator<Item = &'a str>,
) -> Terms<'a> {
    Terms {
        missing: include
            .into_iter()
            .filter(|value| !text.contains(*value))
            .collect(),
        forbidden: exclude
            .into_iter()
            .filter(|value| text.contains(*value))
            .collect(),
    }
}

pub(crate) fn strings(values: &[String]) -> impl Iterator<Item = &str> {
    values.iter().map(String::as_str)
}

pub enum TextScope<'a> {
    Whole,
    /// One-based paragraph, separated by one or more blank lines.
    Paragraph(usize),
    /// A uniquely named ATX Markdown heading, including its subsections.
    Section(&'a str),
}

pub enum TextMatch<'a> {
    Contains(&'a str),
    Omits(&'a str),
    Regex(&'a Regex),
}

pub fn text(evidence: Evidence<&str>, scope: TextScope<'_>, expected: TextMatch<'_>) -> Verdict {
    evidence.evaluate(|text| {
        let selected = match scope {
            TextScope::Whole => Some(Cow::Borrowed(text)),
            TextScope::Paragraph(index) => paragraph(text, index).map(Cow::Owned),
            TextScope::Section(title) => section(text, title).map(Cow::Owned),
        };
        selected.is_some_and(|text| match expected {
            TextMatch::Contains(value) => text.contains(value),
            TextMatch::Omits(value) => !text.contains(value),
            TextMatch::Regex(pattern) => pattern.is_match(&text),
        })
    })
}

fn paragraph(text: &str, index: usize) -> Option<String> {
    let lines: Vec<_> = text.lines().collect();
    lines
        .split(|line| line.trim().is_empty())
        .filter(|lines| !lines.is_empty())
        .nth(index.checked_sub(1)?)
        .map(|lines| lines.join("\n"))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let line = line.trim_start_matches(' ');
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    let rest = line.get(level..)?;
    if !(1..=6).contains(&level) || !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim();
    let without_closing = title.trim_end_matches('#');
    let title = if without_closing.ends_with([' ', '\t']) {
        without_closing.trim_end()
    } else {
        title
    };
    Some((level, title))
}

fn section(text: &str, title: &str) -> Option<String> {
    let mut fences: Option<(char, usize)> = None;
    let mut headings = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start_matches(' ');
        let Some(marker) = trimmed.chars().next() else {
            continue;
        };
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let count = trimmed.chars().take_while(|ch| *ch == marker).count();
        if let Some((open, length)) = fences {
            if marker == open && count >= length && trimmed[count..].trim().is_empty() {
                fences = None;
            }
        } else if matches!(marker, '`' | '~') && count >= 3 {
            fences = Some((marker, count));
        } else if let Some((level, heading)) = heading(line) {
            headings.push((index, level, heading));
        }
    }
    let mut matches = headings
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.2 == title);
    let (position, &(start, level, _)) = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let end = headings[position + 1..]
        .iter()
        .find(|entry| entry.1 <= level)
        .map(|entry| entry.0);
    Some(
        text.lines()
            .skip(start + 1)
            .take(end.map_or(usize::MAX, |end| end - start - 1))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub fn json_value(evidence: Evidence<&Value>, pointer: &str, expected: &Value) -> Verdict {
    evidence.evaluate(|value| value.pointer(pointer) == Some(expected))
}

pub struct Csv {
    columns: csv::StringRecord,
    rows: Vec<csv::StringRecord>,
}

impl Csv {
    /// A header row is required. Rows must have the same width as the header.
    pub fn parse(text: &str) -> csv::Result<Self> {
        let mut reader = csv::Reader::from_reader(text.as_bytes());
        let columns = reader.headers()?.clone();
        if columns.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing CSV header").into());
        }
        let rows = reader.records().collect::<csv::Result<_>>()?;
        Ok(Self { columns, rows })
    }
}

pub enum CsvMatch<'a> {
    Cell {
        row: usize,
        column: usize,
        value: &'a str,
    },
    Rows(usize),
    Columns(&'a [&'a str]),
}

pub fn csv(evidence: Evidence<&Csv>, expected: CsvMatch<'_>) -> Verdict {
    evidence.evaluate(|csv| match expected {
        // Zero-based data row and column indexes. The header is excluded.
        CsvMatch::Cell { row, column, value } => {
            csv.rows.get(row).and_then(|row| row.get(column)) == Some(value)
        }
        CsvMatch::Rows(count) => csv.rows.len() == count,
        CsvMatch::Columns(names) => csv.columns.iter().eq(names.iter().copied()),
    })
}

pub(crate) fn payload_recipients(payload: &Value) -> Option<Vec<String>> {
    payload
        .get("to")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(|address| address.trim().to_lowercase()))
        .collect()
}

pub enum RecipientMatch<'a> {
    Include(&'a [&'a str]),
    Exclude(&'a [&'a str]),
    Exact(&'a [&'a str]),
}

pub fn recipients(evidence: Evidence<&[String]>, expected: RecipientMatch<'_>) -> Verdict {
    evidence.evaluate(|actual| {
        let actual: BTreeSet<_> = actual
            .iter()
            .map(|address| address.trim().to_lowercase())
            .collect();
        let wanted = |addresses: &[&str]| -> BTreeSet<String> {
            addresses
                .iter()
                .map(|address| address.trim().to_lowercase())
                .collect()
        };
        match expected {
            RecipientMatch::Include(addresses) => wanted(addresses).is_subset(&actual),
            RecipientMatch::Exclude(addresses) => wanted(addresses).is_disjoint(&actual),
            RecipientMatch::Exact(addresses) => wanted(addresses) == actual,
        }
    })
}

#[derive(Default)]
pub struct Event<'a> {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub location: Option<&'a str>,
}

impl Event<'_> {
    pub fn local_start(&self, zone: &chrono_tz::Tz) -> Option<String> {
        self.start.map(|start| {
            start
                .with_timezone(zone)
                .format("%Y-%m-%dT%H:%M")
                .to_string()
        })
    }
}

pub fn event_local_start(
    evidence: Evidence<&Event<'_>>,
    zone: &chrono_tz::Tz,
    expected: &str,
) -> Verdict {
    match evidence {
        Evidence::Observed(event) => field(event.local_start(zone).as_deref(), Some(expected)),
        Evidence::Absent | Evidence::Invalid => Verdict::Fail,
        Evidence::Unavailable => Verdict::InsufficientEvidence,
    }
}

fn field<T: PartialEq>(actual: Option<T>, expected: Option<T>) -> Verdict {
    match (actual, expected) {
        (_, None) => Verdict::Pass,
        (Some(actual), Some(expected)) => (actual == expected).into(),
        (None, Some(_)) => Verdict::Fail,
    }
}

pub fn event(evidence: Evidence<&Event<'_>>, expected: &Event<'_>) -> Verdict {
    match evidence {
        Evidence::Observed(actual) => field(actual.start, expected.start)
            .and(field(actual.end, expected.end))
            .and(field(actual.location, expected.location)),
        Evidence::Absent | Evidence::Invalid => Verdict::Fail,
        Evidence::Unavailable => Verdict::InsufficientEvidence,
    }
}

/// Compare every captured claim (group 1) against a value read independently.
/// When a value exists, the answer must claim that value. When the state is
/// confirmed absent, an observed answer may not claim a value.
pub fn answer_matches_state(
    answer: Evidence<&str>,
    claim: &Regex,
    state: Evidence<&str>,
) -> Verdict {
    match state {
        Evidence::Observed(actual) => answer.evaluate(|answer| {
            let mut claims = claim.captures_iter(answer).peekable();
            claims.peek().is_some()
                && claims.all(|claim| claim.get(1).is_some_and(|value| value.as_str() == actual))
        }),
        Evidence::Absent => answer.evaluate(|text| {
            !claim
                .captures_iter(text)
                .any(|capture| capture.get(1).is_some())
        }),
        Evidence::Invalid => Verdict::Fail,
        Evidence::Unavailable => match answer {
            Evidence::Absent | Evidence::Invalid => Verdict::Fail,
            _ => Verdict::InsufficientEvidence,
        },
    }
}

#[derive(Default)]
pub struct ArtifactMetadata<'a> {
    pub format: Option<&'a str>,
    pub dimensions: Option<(u32, u32)>,
    pub duration_ms: Option<u64>,
    pub size_bytes: Option<u64>,
}

pub fn artifact(
    evidence: Evidence<&ArtifactMetadata<'_>>,
    expected: &ArtifactMetadata<'_>,
) -> Verdict {
    match evidence {
        Evidence::Observed(actual) => field(actual.format, expected.format)
            .and(field(actual.dimensions, expected.dimensions))
            .and(field(actual.duration_ms, expected.duration_ms))
            .and(field(actual.size_bytes, expected.size_bytes)),
        Evidence::Absent | Evidence::Invalid => Verdict::Fail,
        Evidence::Unavailable => Verdict::InsufficientEvidence,
    }
}

/// All constraints must hold on one submission to the named tool. This
/// verifies the request only. Pass independent state to the artifact checks.
pub fn tool_arguments<'a>(
    calls: Evidence<impl IntoIterator<Item = (&'a str, &'a Value)>>,
    tool: &str,
    constraints: &[(&str, &Value)],
) -> Verdict {
    calls.evaluate(|calls| {
        calls.into_iter().any(|(name, args)| {
            name == tool
                && constraints
                    .iter()
                    .all(|(pointer, expected)| args.pointer(pointer) == Some(*expected))
        })
    })
}

/// Count matching records in a complete approval or execution log. An empty
/// observed log is distinct from a missing or incomplete log.
pub fn action_count<T>(
    records: Evidence<&[T]>,
    mut matches: impl FnMut(&T) -> bool,
    expected: RangeInclusive<usize>,
) -> Verdict {
    records.evaluate(|records| {
        expected.contains(&records.iter().filter(|record| matches(record)).count())
    })
}

/// Report an L3 decision using Hatch's existing evidence-reference validation.
/// The caller supplies an isolated grader's verdict over independent readback.
pub fn semantic(
    item: &hatch_plan::acceptance::AcceptanceItem,
    decision: Option<&hatch_plan::acceptance::NlItemVerdict>,
    evidence: &[hatch_plan::acceptance::NlEvidence],
) -> Check {
    use hatch_plan::acceptance::{nl_checks, AcceptanceItem};
    use hatch_plan::verification::CheckVerdict as ProductVerdict;

    if !matches!(item, AcceptanceItem::Nl { .. }) || evidence.is_empty() {
        return Verdict::InsufficientEvidence.report(item.id(), "语义评分缺少条件或独立证据");
    }
    let ids = evidence.iter().map(|item| item.id.clone()).collect();
    let decisions = decision
        .filter(|decision| !decision.evidence.is_empty())
        .map_or(&[][..], std::slice::from_ref);
    let mut results = nl_checks(std::slice::from_ref(item), decisions, &ids, "", false);
    let result = results
        .pop()
        .expect("nl_checks returns one result per item");
    let verdict = match result.verdict {
        ProductVerdict::Pass => Verdict::Pass,
        ProductVerdict::Fail => Verdict::Fail,
        _ => Verdict::InsufficientEvidence,
    };
    verdict.report(item.id(), result.reason)
}

#[cfg(test)]
mod tests {
    use super::{Evidence::*, Verdict::*, *};
    use serde_json::json;

    #[test]
    fn documents_use_independent_text_and_requested_scope() {
        let report = Observed("Intro\n\n# Results\nCount: 41\n```\n# Example\n```\n## Detail\nverified\n# Appendix\nprivate");
        assert_eq!(file_exists(Absent, true), Fail);
        assert_eq!(file_exists(Absent, false), Pass);
        assert_eq!(file_exists(Unavailable, false), InsufficientEvidence);
        assert_eq!(file_exists(Observed(()), true), Pass);
        assert_eq!(
            text(
                report,
                TextScope::Paragraph(1),
                TextMatch::Contains("Intro")
            ),
            Pass
        );
        assert_eq!(
            text(
                report,
                TextScope::Section("Results"),
                TextMatch::Contains("verified")
            ),
            Pass
        );
        assert_eq!(
            text(
                report,
                TextScope::Section("Results"),
                TextMatch::Omits("private")
            ),
            Pass
        );
        assert_eq!(
            text(
                report,
                TextScope::Section("missing"),
                TextMatch::Omits("private")
            ),
            Fail
        );
        assert_eq!(
            text(
                report,
                TextScope::Section("Example"),
                TextMatch::Contains("verified")
            ),
            Fail
        );
        assert_eq!(
            text(
                report,
                TextScope::Whole,
                TextMatch::Regex(&Regex::new(r"Count: \d+").unwrap())
            ),
            Pass
        );
        assert_eq!(
            text(Unavailable, TextScope::Whole, TextMatch::Omits("private")),
            InsufficientEvidence
        );
        let terms = terms("shipped confidential", ["delivered"], ["confidential"]);
        assert_eq!(terms.missing, ["delivered"]);
        assert_eq!(terms.forbidden, ["confidential"]);
        assert!(!terms.matches());
    }

    #[test]
    fn file_snapshot_keeps_missing_and_unreadable_entries_distinct() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.txt"), [0xff]).unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let files = TextFiles::capture(dir.path());
        assert_eq!(files.read("empty.txt"), Observed(""));
        assert_eq!(files.read("missing.txt"), Absent);
        assert_eq!(files.read("broken.txt"), Invalid);
        assert_eq!(
            Evidence::from(fs::read_to_string(dir.path().join("broken.txt"))),
            Invalid
        );
        assert_eq!(
            TextFiles::capture(&dir.path().join("missing")).read("nested/file"),
            Unavailable
        );
        let check = file_terms("report", "broken.txt", files.read("broken.txt"), &[], &[]);
        assert!(!check.insufficient_evidence);
        assert!(!check.passed);
        let incomplete = TextFiles::capture(&dir.path().join("missing"));
        assert_eq!(files.changed_since(&incomplete), Unavailable);
        std::fs::write(dir.path().join("broken.txt"), [0xfe]).unwrap();
        assert_eq!(
            TextFiles::capture(dir.path()).changed_since(&files),
            Observed(true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_reads_internal_links_and_stays_within_workspace() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("reports")).unwrap();
        fs::write(dir.path().join("reports/report.md"), "result").unwrap();
        fs::write(outside.path().join("outside.md"), "outside").unwrap();
        symlink("reports/report.md", dir.path().join("report.md")).unwrap();
        symlink("reports", dir.path().join("alias")).unwrap();
        symlink(".", dir.path().join("loop")).unwrap();
        symlink("missing", dir.path().join("dangling.md")).unwrap();
        symlink("loop-b", dir.path().join("loop-a")).unwrap();
        symlink("loop-a", dir.path().join("loop-b")).unwrap();
        symlink(
            outside.path().join("missing.md"),
            dir.path().join("missing-outside.md"),
        )
        .unwrap();
        symlink(
            outside.path().join("outside.md"),
            dir.path().join("outside.md"),
        )
        .unwrap();
        let files = TextFiles::capture(dir.path());
        assert_eq!(files.read("report.md"), Observed("result"));
        assert_eq!(files.read("alias/report.md"), Observed("result"));
        assert_eq!(files.read("loop"), Invalid);
        assert_eq!(files.read("loop/report.md"), Invalid);
        assert_eq!(files.read("loop-a"), Invalid);
        assert_eq!(files.read("dangling.md"), Absent);
        assert_eq!(files.read("outside.md"), Invalid);
        assert_eq!(files.read("missing-outside.md"), Invalid);
        assert_eq!(
            TextFiles::capture(dir.path()).changed_since(&files),
            Observed(false)
        );

        fs::remove_file(dir.path().join("outside.md")).unwrap();
        symlink(
            outside.path().join("other.md"),
            dir.path().join("outside.md"),
        )
        .unwrap();
        assert_eq!(
            TextFiles::capture(dir.path()).changed_since(&files),
            Observed(true)
        );
    }

    #[test]
    fn structured_checks_read_typed_json_and_quoted_csv() {
        let data = json!({"a/b": [42, null]});
        assert_eq!(json_value(Observed(&data), "/a~1b/0", &json!(42)), Pass);
        assert_eq!(json_value(Observed(&data), "/a~1b/0", &json!("42")), Fail);
        assert_eq!(json_value(Observed(&data), "/a~1b/1", &Value::Null), Pass);
        assert_eq!(json_value(Observed(&data), "/missing", &Value::Null), Fail);
        let table = Csv::parse("item,note\r\na,\"comma, newline\ncontinued\"\r\n").unwrap();
        assert_eq!(
            csv(Observed(&table), CsvMatch::Columns(&["item", "note"])),
            Pass
        );
        assert_eq!(
            csv(Observed(&table), CsvMatch::Columns(&["note", "item"])),
            Fail
        );
        assert_eq!(csv(Observed(&table), CsvMatch::Rows(1)), Pass);
        assert_eq!(
            csv(
                Observed(&table),
                CsvMatch::Cell {
                    row: 0,
                    column: 1,
                    value: "comma, newline\ncontinued"
                }
            ),
            Pass
        );
        assert_eq!(
            csv(
                Observed(&table),
                CsvMatch::Cell {
                    row: 1,
                    column: 1,
                    value: ""
                }
            ),
            Fail
        );
        assert!(Csv::parse("a,b\none\n").is_err());
        assert!(Csv::parse("").is_err());
    }

    #[test]
    fn message_and_event_checks_compare_observed_fields() {
        let to = [" a@example.com ".to_owned(), "b@example.com".to_owned()];
        assert_eq!(
            recipients(Observed(&to), RecipientMatch::Include(&["A@example.com"])),
            Pass
        );
        assert_eq!(
            recipients(Observed(&to), RecipientMatch::Exclude(&["b@example.com"])),
            Fail
        );
        assert_eq!(
            recipients(
                Observed(&to),
                RecipientMatch::Exact(&["b@example.com", "a@example.com"])
            ),
            Pass
        );
        assert_eq!(
            recipients(Observed(&to), RecipientMatch::Exact(&["b@example.com"])),
            Fail
        );
        assert_eq!(
            recipients(Unavailable, RecipientMatch::Exclude(&["b@example.com"])),
            InsufficientEvidence
        );
        assert_eq!(
            text(
                Observed("Please review the report"),
                TextScope::Whole,
                TextMatch::Contains("review")
            ),
            Pass
        );

        let start = "2026-11-28T01:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let end = "2026-11-28T02:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let want = Event {
            start: Some(start),
            end: Some(end),
            location: Some("R1"),
        };
        assert_eq!(event(Observed(&want), &want), Pass);
        assert_eq!(
            event_local_start(
                Observed(&want),
                &chrono_tz::Asia::Shanghai,
                "2026-11-28T09:00"
            ),
            Pass
        );
        for actual in [
            Event {
                start: Some(end),
                ..want
            },
            Event {
                end: Some(start),
                ..want
            },
            Event {
                location: Some("R2"),
                ..want
            },
        ] {
            assert_eq!(event(Observed(&actual), &want), Fail);
        }
        assert_eq!(
            event(
                Observed(&Event {
                    location: None,
                    ..want
                }),
                &Event {
                    location: Some("R1"),
                    ..Default::default()
                }
            ),
            Fail
        );
        assert_eq!(event(Unavailable, &want), InsufficientEvidence);
        assert_eq!(
            event_local_start(
                Observed(&Event::default()),
                &chrono_tz::Asia::Shanghai,
                "2026-11-28T09:00"
            ),
            Fail
        );
    }

    #[test]
    fn a_summary_that_contradicts_the_readback_fails() {
        let claim = Regex::new(r"total=(\d+)").unwrap();
        assert_eq!(
            answer_matches_state(Observed("total=42"), &claim, Observed("42")),
            Pass
        );
        assert_eq!(
            answer_matches_state(Observed("total=42"), &claim, Observed("41")),
            Fail
        );
        assert_eq!(
            answer_matches_state(Observed("total=42; total=41"), &claim, Observed("42")),
            Fail
        );
        assert_eq!(
            answer_matches_state(Observed("Done"), &claim, Observed("42")),
            Fail
        );
        assert_eq!(
            answer_matches_state(Observed("total=42"), &claim, Absent),
            Fail
        );
        assert_eq!(
            answer_matches_state(Observed("total=42"), &claim, Unavailable),
            InsufficientEvidence
        );
    }

    #[test]
    fn a_confirmed_absent_state_requires_an_observed_answer_without_a_claim() {
        let claim = Regex::new(r"total=(\d+)|no total").unwrap();
        assert_eq!(
            answer_matches_state(Observed("no total"), &claim, Absent),
            Pass
        );
        assert_eq!(
            answer_matches_state(Observed("no total; total=42"), &claim, Absent),
            Fail
        );
        assert_eq!(
            answer_matches_state(Unavailable, &claim, Absent),
            InsufficientEvidence
        );
        assert_eq!(answer_matches_state(Absent, &claim, Absent), Fail);
    }

    #[test]
    fn semantic_decisions_require_known_evidence_references() {
        use hatch_plan::acceptance::{AcceptanceItem, NlEvidence, NlItemVerdict};
        use hatch_plan::verification::CheckVerdict;

        let item = AcceptanceItem::Nl {
            id: "summary".into(),
            expected: "Explanation agrees with the report".into(),
        };
        let evidence = [NlEvidence {
            id: "report".into(),
            source: "report.md".into(),
            value: json!("Total: 42"),
        }];
        let mut decision = NlItemVerdict {
            id: "summary".into(),
            verdict: CheckVerdict::Pass,
            evidence: vec!["report".into()],
            reason: "Totals agree".into(),
        };
        let check = semantic(&item, Some(&decision), &evidence);
        assert!(check.passed && check.detail.contains("report"));
        decision.evidence = vec!["receipt".into()];
        assert!(semantic(&item, Some(&decision), &evidence).insufficient_evidence);
        assert!(semantic(&item, None, &evidence).insufficient_evidence);
    }

    #[test]
    fn a_successful_submission_cannot_replace_artifact_readback() {
        let args = json!({"width": 640, "height": 480});
        let other = json!({"height": 480});
        let partial = json!({"width": 640});
        let width = json!(640);
        let height = json!(480);
        let limits = [("/width", &width), ("/height", &height)];
        let submitted = tool_arguments(Observed([("render", &args)]), "render", &limits);
        assert_eq!(submitted, Pass);
        assert_eq!(
            tool_arguments(
                Observed([("render", &partial), ("render", &other)]),
                "render",
                &limits
            ),
            Fail
        );
        let receipt = json!({"status": "success"});
        let accepted = json_value(Observed(&receipt), "/status", &json!("success"));
        assert_eq!(accepted, Pass);
        let want = ArtifactMetadata {
            format: Some("mp4"),
            dimensions: Some((640, 480)),
            duration_ms: Some(2_000),
            size_bytes: Some(1234),
        };
        assert_eq!(artifact(Observed(&want), &want), Pass);
        for wrong in [
            ArtifactMetadata {
                format: Some("gif"),
                ..want
            },
            ArtifactMetadata {
                dimensions: Some((800, 600)),
                ..want
            },
            ArtifactMetadata {
                duration_ms: Some(4_000),
                ..want
            },
            ArtifactMetadata {
                size_bytes: Some(99),
                ..want
            },
        ] {
            assert_eq!(accepted.and(artifact(Observed(&wrong), &want)), Fail);
        }
        assert_eq!(
            artifact(Observed(&ArtifactMetadata::default()), &want),
            Fail
        );
        assert_eq!(
            submitted.and(artifact(Unavailable, &want)),
            InsufficientEvidence
        );

        let approvals = ["send", "read"];
        assert_eq!(
            action_count(Observed(&approvals), |kind| *kind == "send", 1..=1),
            Pass
        );
        assert_eq!(
            action_count(Observed(&approvals), |kind| *kind == "delete", 0..=0),
            Pass
        );
        assert_eq!(
            action_count(Observed(&["send", "send"]), |kind| *kind == "send", 1..=1),
            Fail
        );
        assert_eq!(
            action_count::<&str>(Unavailable, |_| true, 0..=0),
            InsufficientEvidence
        );
    }
}
