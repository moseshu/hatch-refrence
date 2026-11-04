//! Usage ledger: one JSON object per proxied call, appended to
//! `<home>/inference/usage.jsonl`.
//!
//! The ledger is the proxy's source of truth for what model calls
//! cost: every entry carries the *provider-reported* usage (prompt /
//! completion / cached / reasoning tokens), never an estimate — the
//! chars/4 rule is for pre-call budgeting only. Cost is an estimate
//! computed at read time from configured per-million-token prices;
//! with no prices configured there is no estimate, and cached
//! tokens are billed at the input price (cached-token discounts are
//! not modelled, so an estimate is an upper bound).
//!
//! The agent's own `usage_log` table is untouched: it records what
//! one agent run saw, while this ledger records what the proxy
//! served across every client (chat, daemon, sub-agents, MCP).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// One proxied model call.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageEntry {
    /// RFC 3339 timestamp of the completed call.
    pub ts: String,
    /// Model name from the request body (`-` when the body had none).
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    /// Wall-clock milliseconds for the upstream call (retries included).
    pub duration_ms: u64,
    /// Retries actually performed (0 = first attempt succeeded).
    pub retries: u32,
    /// Upstream HTTP status.
    pub status: u16,
    /// Whether the call succeeded (2xx from upstream).
    pub ok: bool,
}

/// Prices for a cost estimate. `None` on either side means no
/// estimate is produced at all.
#[derive(Clone, Debug, Default)]
pub struct Pricing {
    pub input_per_million: Option<f64>,
    pub output_per_million: Option<f64>,
    pub currency: String,
}

impl Pricing {
    /// Estimated cost of `prompt` input + `completion` output tokens.
    /// `None` when either price is unconfigured.
    pub fn estimate(&self, prompt: u64, completion: u64) -> Option<f64> {
        let (i, o) = (self.input_per_million?, self.output_per_million?);
        Some(prompt as f64 / 1_000_000.0 * i + completion as f64 / 1_000_000.0 * o)
    }
}

/// Totals for one model inside a summary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelSummary {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

/// Aggregated usage over a set of entries.
#[derive(Clone, Debug, Default)]
pub struct UsageSummary {
    pub calls: u64,
    pub ok_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    /// Estimated cost, or `None` when prices are unconfigured.
    pub cost: Option<f64>,
    pub currency: String,
    pub per_model: BTreeMap<String, ModelSummary>,
}

/// Summarize entries, optionally filtered to one day (`YYYY-MM-DD`)
/// and/or one model. Entries whose timestamp does not start with the
/// day are excluded; a `day` of `None` takes all days.
pub fn summarize(
    entries: &[UsageEntry],
    day: Option<&str>,
    model: Option<&str>,
    pricing: &Pricing,
) -> UsageSummary {
    let mut s = UsageSummary {
        currency: pricing.currency.clone(),
        ..Default::default()
    };
    for e in entries {
        if let Some(d) = day {
            if !e.ts.starts_with(d) {
                continue;
            }
        }
        if let Some(m) = model {
            if e.model != m {
                continue;
            }
        }
        s.calls += 1;
        if e.ok {
            s.ok_calls += 1;
        }
        s.prompt_tokens += e.prompt_tokens;
        s.completion_tokens += e.completion_tokens;
        s.cached_tokens += e.cached_tokens;
        s.reasoning_tokens += e.reasoning_tokens;
        let m = s.per_model.entry(e.model.clone()).or_default();
        m.calls += 1;
        m.prompt_tokens += e.prompt_tokens;
        m.completion_tokens += e.completion_tokens;
        m.cached_tokens += e.cached_tokens;
    }
    s.cost = pricing.estimate(s.prompt_tokens, s.completion_tokens);
    s
}

/// Append-only usage ledger. One process (the proxy) writes; readers
/// (`hatch inference usage`, the API) only read.
pub struct UsageLedger {
    path: PathBuf,
    file: Mutex<File>,
}

impl UsageLedger {
    /// Open (creating) the ledger file: parent dir `0700`, file
    /// `0600` on Unix. Appends only; existing entries are never
    /// rewritten.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            #[cfg(unix)]
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(UsageLedger {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry (one JSON object per line). Never logs or
    /// returns the entry's contents beyond the I/O error.
    pub fn record(&self, entry: &UsageEntry) -> io::Result<()> {
        let line = serde_json::to_string(entry).map_err(io::Error::other)?;
        let mut f = self.file.lock().unwrap();
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()
    }
}

/// Read all entries from a ledger file. A missing file is an empty
/// ledger; a corrupt line is skipped (and counted in the returned
/// number) rather than failing the whole read.
pub fn read_all(path: &Path) -> (Vec<UsageEntry>, usize) {
    let Ok(file) = File::open(path) else {
        return (Vec::new(), 0);
    };
    let mut entries = Vec::new();
    let mut skipped = 0;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            skipped += 1;
            continue;
        };
        match serde_json::from_str::<UsageEntry>(&line) {
            Ok(e) => entries.push(e),
            Err(_) => skipped += 1,
        }
    }
    (entries, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn entry(ts: &str, model: &str, prompt: u64, completion: u64) -> UsageEntry {
        UsageEntry {
            ts: ts.into(),
            model: model.into(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: 0,
            reasoning_tokens: 0,
            duration_ms: 10,
            retries: 0,
            status: 200,
            ok: true,
        }
    }

    #[test]
    fn ledger_round_trips_and_summarizes_by_day_and_model() {
        let dir = std::env::temp_dir().join(format!("hatch-infer-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("usage.jsonl");
        let ledger = UsageLedger::open(&path).unwrap();
        ledger
            .record(&entry("2026-11-05T10:00:00Z", "gpt-6-luna", 100, 20))
            .unwrap();
        ledger
            .record(&entry("2026-11-05T11:00:00Z", "gpt-6-luna", 200, 30))
            .unwrap();
        ledger
            .record(&entry("2026-11-04T09:00:00Z", "other", 50, 5))
            .unwrap();
        let (entries, skipped) = read_all(&path);
        assert_eq!((entries.len(), skipped), (3, 0));
        // File is 0600 on Unix.
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let pricing = Pricing {
            input_per_million: Some(10.0),
            output_per_million: Some(40.0),
            currency: "CNY".into(),
        };
        let all = summarize(&entries, None, None, &pricing);
        assert_eq!(
            (all.calls, all.prompt_tokens, all.completion_tokens),
            (3, 350, 55)
        );
        assert!((all.cost.unwrap() - (350.0 / 1e6 * 10.0 + 55.0 / 1e6 * 40.0)).abs() < 1e-12);
        assert_eq!(all.per_model["gpt-6-luna"].calls, 2);
        let day = summarize(&entries, Some("2026-11-05"), None, &pricing);
        assert_eq!((day.calls, day.prompt_tokens), (2, 300));
        let one = summarize(&entries, None, Some("other"), &pricing);
        assert_eq!((one.calls, one.prompt_tokens), (1, 50));
        // No prices configured: no estimate, never an invented number.
        let noprice = summarize(&entries, None, None, &Pricing::default());
        assert_eq!(noprice.cost, None);
        // A corrupt line is skipped, not fatal.
        let mut raw = String::new();
        File::open(&path).unwrap().read_to_string(&mut raw).unwrap();
        fs::write(&path, format!("{raw}not json\n")).unwrap();
        let (entries, skipped) = read_all(&path);
        assert_eq!((entries.len(), skipped), (3, 1));
        let _ = fs::remove_dir_all(&dir);
    }
}
