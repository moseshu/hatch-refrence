//! The web side of the real-web evaluation: a recorder around the real
//! `fetch_url` / `web_search` executors, offline fixture pages for
//! pipeline mode, and the harness's own lookup of ground-truth values.
//! Nothing here is used outside `hatch eval`.

use hatch_core::{Error, Result};
use hatch_tools::verification::{EffectContract, EvidenceCheck};
use hatch_tools::web::FetchLimits;
use hatch_tools::{ActionExecutor, ExecContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// One fetch or search the agent's executors ran.
#[derive(Clone, Debug, Serialize)]
pub struct WebRecord {
    pub task: String,
    /// `fetch_url` or `web_search`.
    pub kind: String,
    /// The requested URL, or the search query.
    pub target: String,
    pub ok: bool,
    /// Where a fetch ended up after redirects.
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// Characters of result text the agent saw.
    pub chars: usize,
    /// The result text itself (kept out of the JSON report: pages are big).
    #[serde(skip)]
    pub text: String,
}

#[derive(Default)]
struct Inner {
    task: String,
    log: Vec<WebRecord>,
}

/// Shared between the recorders and the runner.
#[derive(Default)]
pub struct WebLog {
    inner: Mutex<Inner>,
}

impl WebLog {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn begin(&self, task: &str) {
        self.lock().task = task.to_string();
    }

    pub fn records(&self) -> Vec<WebRecord> {
        self.lock().log.clone()
    }

    fn push(&self, mut r: WebRecord) {
        let mut g = self.lock();
        r.task = g.task.clone();
        g.log.push(r);
    }
}

/// Parse the head the fetch executor writes: `URL: <final>\nHTTP <code>，…`.
fn fetch_head(out: &str) -> (Option<String>, Option<u16>) {
    let mut lines = out.lines();
    let url = lines
        .next()
        .and_then(|l| l.strip_prefix("URL: "))
        .map(|s| s.trim().to_string());
    let status = lines
        .next()
        .and_then(|l| l.strip_prefix("HTTP "))
        .and_then(|l| l.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok());
    (url, status)
}

/// A real (or fixture) executor whose every run is recorded.
pub struct Recording {
    pub inner: Arc<dyn ActionExecutor>,
    pub log: Arc<WebLog>,
}

impl ActionExecutor for Recording {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let target = payload["url"]
            .as_str()
            .or_else(|| payload["query"].as_str())
            .unwrap_or("")
            .to_string();
        let res = self.inner.execute(payload, ctx);
        let kind = self.inner.kind().to_string();
        let rec = match &res {
            Ok(out) => {
                let (final_url, status) = if kind == "fetch_url" { fetch_head(out) } else { (None, None) };
                WebRecord {
                    task: String::new(),
                    kind,
                    target,
                    ok: status.is_none_or(|s| s < 400),
                    final_url,
                    status,
                    error: None,
                    chars: out.chars().count(),
                    text: out.clone(),
                }
            }
            Err(e) => WebRecord {
                task: String::new(),
                kind,
                target,
                ok: false,
                final_url: None,
                status: None,
                error: Some(e.to_string().chars().take(300).collect()),
                chars: 0,
                text: String::new(),
            },
        };
        self.log.push(rec);
        res
    }

    fn contract(&self) -> EffectContract {
        self.inner.contract()
    }

    fn verify(&self, payload: &Value, receipt: &str, ctx: &ExecContext) -> Result<EvidenceCheck> {
        self.inner.verify(payload, receipt, ctx)
    }

    fn observe(&self, payload: &Value, receipt: &str, ctx: &ExecContext) -> Result<BTreeMap<String, Value>> {
        self.inner.observe(payload, receipt, ctx)
    }
}

/// Lower-case host of an http(s) URL.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `host` is `site` or one of its subdomains (`www.sqlite.org` ~ `sqlite.org`).
pub fn host_matches(host: &str, site: &str) -> bool {
    let site = site.to_ascii_lowercase();
    let site = site.trim_start_matches("www.");
    let host = host.trim_start_matches("www.");
    host == site || host.ends_with(&format!(".{site}"))
}

/// URL key for fixture lookup: no fragment, no trailing slash.
fn url_key(url: &str) -> String {
    url.trim().split('#').next().unwrap_or("").trim_end_matches('/').to_string()
}

#[derive(Clone, Debug, Deserialize)]
pub struct FixturePage {
    #[serde(default = "ok_status")]
    pub status: u16,
    /// Text file under `fixtures/` (served as-is); none = empty body.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default = "plain")]
    pub content_type: String,
    #[serde(skip)]
    pub text: String,
}

fn ok_status() -> u16 {
    200
}

fn plain() -> String {
    "text/plain".into()
}

/// Offline pages: `fixtures/pages.json` maps URL → page.
#[derive(Clone, Debug, Default)]
pub struct Fixtures {
    pages: BTreeMap<String, FixturePage>,
}

impl Fixtures {
    pub fn load(dir: &Path) -> Result<Fixtures> {
        let path = dir.join("pages.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::NotFound(format!("{}: {e}", path.display())))?;
        let map: BTreeMap<String, FixturePage> = serde_json::from_str(&raw)?;
        let mut pages = BTreeMap::new();
        for (url, mut p) in map {
            if let Some(f) = &p.file {
                let fp = dir.join(f);
                p.text = std::fs::read_to_string(&fp)
                    .map_err(|e| Error::NotFound(format!("{}: {e}", fp.display())))?;
            }
            pages.insert(url_key(&url), p);
        }
        Ok(Fixtures { pages })
    }

    pub fn get(&self, url: &str) -> Option<&FixturePage> {
        self.pages.get(&url_key(url))
    }

    /// What a real fetch would do with this URL: the page, an HTTP error,
    /// or a host that does not resolve (`.invalid` never does).
    pub fn fetch(&self, url: &str) -> Result<(String, &FixturePage)> {
        let host = host_of(url).ok_or_else(|| Error::Invalid(format!("not an http(s) URL: {url}")))?;
        match self.get(url) {
            Some(p) if p.status < 400 => Ok((url_key(url), p)),
            Some(p) => Err(Error::Connector(format!("HTTP {} Not Found", p.status))),
            None if host.ends_with(".invalid") => {
                Err(Error::NotFound(format!("无法解析域名 {host}")))
            }
            None => Err(Error::Connector("HTTP 404 Not Found".into())),
        }
    }
}

/// `fetch_url` served from fixtures, in the real executor's output format.
pub struct FixtureFetch {
    pub pages: Arc<Fixtures>,
}

impl ActionExecutor for FixtureFetch {
    fn kind(&self) -> &'static str {
        "fetch_url"
    }

    fn contract(&self) -> EffectContract {
        EffectContract::ReadOnly
    }

    fn execute(&self, payload: &Value, _ctx: &ExecContext) -> Result<String> {
        let url = payload["url"].as_str().unwrap_or("");
        let (final_url, p) = self.pages.fetch(url)?;
        Ok(format!("URL: {final_url}\nHTTP {}，{}\n\n{}", p.status, p.content_type, p.text))
    }
}

/// Where the harness reads ground truth for `LiveValue` checks: the real
/// network in live mode, the fixtures in pipeline mode.
pub enum Truth {
    Live(FetchLimits),
    Fixture(Arc<Fixtures>),
}

impl Truth {
    pub fn text(&self, url: &str) -> Result<String> {
        match self {
            Truth::Live(limits) => Ok(hatch_tools::web::fetch(url, limits)?.text),
            Truth::Fixture(f) => Ok(f.fetch(url)?.1.text.clone()),
        }
    }

    /// The string (or number) at a JSON pointer in the page at `url`.
    pub fn json_value(&self, url: &str, pointer: &str) -> Result<String> {
        let v: Value = serde_json::from_str(&self.text(url)?)?;
        match v.pointer(pointer) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Number(n)) => Ok(n.to_string()),
            _ => Err(Error::NotFound(format!("{url}: nothing at {pointer}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_matching() {
        assert_eq!(host_of("https://www.SQLite.org/limits.html#max").as_deref(), Some("www.sqlite.org"));
        assert_eq!(host_of("https://pypi.org:443/pypi/x/json").as_deref(), Some("pypi.org"));
        assert_eq!(host_of("not a url"), None);
        assert!(host_matches("www.sqlite.org", "sqlite.org"));
        assert!(host_matches("blog.rust-lang.org", "blog.rust-lang.org"));
        assert!(!host_matches("rust-lang.org", "blog.rust-lang.org"));
        assert!(!host_matches("evilsqlite.org", "sqlite.org"));
    }

    #[test]
    fn fetch_head_parses_executor_output() {
        let (u, s) = fetch_head("URL: https://a.example/x\nHTTP 200，text/html\n\nbody");
        assert_eq!(u.as_deref(), Some("https://a.example/x"));
        assert_eq!(s, Some(200));
    }
}
