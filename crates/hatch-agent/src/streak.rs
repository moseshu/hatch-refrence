//! Loop-level streak detection: two per-turn counters that turn
//! silent spinning into a signal the model can act on.
//!
//! * **Read-only streak** — consecutive steps whose calls are all
//!   read-only (search/fetch/read) with no state-changing call in
//!   between. At the threshold the loop injects a soft convergence
//!   note; nothing is blocked.
//! * **Same-source error streak** — the same (tool, target, error
//!   kind) failing repeatedly. Keyed by *target*, not by error class
//!   alone: four different hosts failing in a row is source-switching
//!   (healthy), the same host failing three times is a wall. At 2 a
//!   note is injected; at 3 the turn is forced into a text-only
//!   wrap-up so the model reports the block instead of guessing.
//!
//! Error kinds come from the structured tag dispatch renders
//! (`ERROR: [kind] msg`); untagged result text falls back to coarse
//! heuristics in `error_kind_of`.

use std::collections::HashMap;

/// Consecutive read-only steps before the convergence note fires.
pub const READ_ONLY_STREAK_NOTE_AT: usize = 5;
/// Same-source failures before the block note fires.
pub const ERROR_STREAK_NOTE_AT: usize = 2;
/// Same-source failures before the turn is forced to wrap up.
pub const ERROR_STREAK_WRAPUP_AT: usize = 3;

/// Step note used when the error breaker forces a text-only wrap-up.
pub const WRAPUP_NOTE: &str = "## Blocked wrap-up\nThe same target has failed repeatedly with the same kind of error; tools are disabled. Report to the user: the alternatives you tried, the confirmed blocking cause (with the original error), and the conditions or actions you need from the user. Do not fabricate results, and do not just say it cannot be done.";

/// One executed call as the tracker sees it.
pub struct ObservedCall<'a> {
    pub name: &'a str,
    pub arguments: &'a serde_json::Value,
    pub content: &'a str,
    /// True when the tool's action class cannot change any state.
    pub read_only: bool,
}

#[derive(Default)]
pub struct StreakTracker {
    read_only_streak: usize,
    read_only_note_pending: bool,
    /// (tool, target, kind) -> consecutive failures.
    error_streaks: HashMap<(String, String, String), usize>,
    error_note_pending: Option<String>,
    wrapup: bool,
}

impl StreakTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one step's executed calls (after their results are in).
    pub fn observe_step(&mut self, calls: &[ObservedCall<'_>]) {
        if calls.is_empty() {
            return;
        }
        if calls.iter().all(|c| c.read_only) {
            self.read_only_streak += 1;
            if self.read_only_streak >= READ_ONLY_STREAK_NOTE_AT {
                self.read_only_note_pending = true;
                self.read_only_streak = 0;
            }
        } else {
            self.read_only_streak = 0;
        }
        for call in calls {
            let target = target_of(call.name, call.arguments);
            // A browser page the site refused (WAF / 403 wall) comes
            // back as a successful execution carrying the ⚠ [blocked]
            // banner — the model still gets the page context, but for
            // breaker purposes it is the same environment wall as an
            // HTTP 403 error, so it counts under kind "blocked".
            let is_error = hatch_tools::verification::result_reports_error(call.content);
            let blocked = !is_error && call.content.contains("⚠ [blocked]");
            if is_error || blocked {
                let kind = if is_error { error_kind_of(call.content) } else { "blocked".to_string() };
                let key = (call.name.to_string(), target.clone(), kind.clone());
                let n = self.error_streaks.entry(key).or_insert(0);
                *n += 1;
                if *n >= ERROR_STREAK_WRAPUP_AT {
                    self.wrapup = true;
                } else if *n == ERROR_STREAK_NOTE_AT {
                    self.error_note_pending = Some(format!(
                        "## Blocked reminder\nTool {} on {} has failed {} times in a row with the same kind of error ({}). Retrying with different arguments is unlikely to help. Switch to a different target or source, or report the blocking cause and what you need to the user.",
                        call.name, target, n, kind
                    ));
                }
            } else {
                // A success on this (tool, target) clears its error streak.
                self.error_streaks
                    .retain(|(t, tg, _), _| !(t == call.name && tg == &target));
            }
        }
    }

    /// The note to inject into the next step, if one fired. Consumed
    /// on read; the error note wins over the read-only note.
    pub fn take_note(&mut self) -> Option<String> {
        if let Some(note) = self.error_note_pending.take() {
            self.read_only_note_pending = false;
            return Some(note);
        }
        if self.read_only_note_pending {
            self.read_only_note_pending = false;
            return Some(format!(
                "## Convergence reminder\n{} steps in a row have been read-only queries with no actual progress. Organize a conclusion from the evidence you have and prepare to answer; if core facts are still missing, change strategy (a different data source, fetching the target site or its API directly) instead of repeating searches with tweaked keywords.",
                READ_ONLY_STREAK_NOTE_AT
            ));
        }
        None
    }

    /// True once a same-source error streak hit the hard threshold:
    /// the loop should run text-only and wrap up.
    pub fn force_wrapup(&self) -> bool {
        self.wrapup
    }
}

/// The identity a failure is attributed to: the host for web tools,
/// the path for file tools, the program for exec, else the tool name.
fn target_of(name: &str, args: &serde_json::Value) -> String {
    if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        let rest = url.split("://").nth(1).unwrap_or(url);
        let host = rest.split(['/', ':', '?']).next().unwrap_or(rest);
        if !host.is_empty() {
            return host.to_string();
        }
    }
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        return path.to_string();
    }
    if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
        if let Some(first) = cmd.split_whitespace().next() {
            return first.to_string();
        }
    }
    name.to_string()
}

/// Error kind from rendered result text. Dispatch renders failures
/// as `ERROR: [kind] msg` with the structured `Error::kind` tag —
/// that tag wins. Untagged text (executor result strings, MCP
/// envelopes) falls back to coarse heuristics.
fn error_kind_of(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("ERROR: [") {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    let lower = content.to_ascii_lowercase();
    let kind = if lower.contains("policy_denied") || content.contains("被安全策略拒绝") || lower.contains("policy denied") {
        "policy"
    } else if lower.contains("permission denied") || content.contains("权限不足") || lower.contains("forbidden") {
        "permission"
    } else if lower.contains("notfound") || lower.contains("not found") || lower.contains("no such file") || content.contains("不存在") {
        "not_found"
    } else if lower.contains("timed out") || lower.contains("timeout") || content.contains("超时") {
        "timeout"
    } else if lower.contains("http 4") || lower.contains("http 5") || lower.contains("status: 4") || lower.contains("status: 5") {
        "http_status"
    } else if lower.contains("connection") || lower.contains("tls") || lower.contains("dns") || content.contains("网络") {
        "network"
    } else {
        "error"
    };
    kind.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call<'a>(name: &'a str, args: &'a serde_json::Value, content: &'a str, read_only: bool) -> ObservedCall<'a> {
        ObservedCall { name, arguments: args, content, read_only }
    }

    #[test]
    fn read_only_streak_notes_once_at_threshold() {
        let mut t = StreakTracker::new();
        let args = json!({"query": "x"});
        for _ in 0..READ_ONLY_STREAK_NOTE_AT - 1 {
            t.observe_step(&[call("web_search", &args, "results", true)]);
            assert!(t.take_note().is_none());
        }
        t.observe_step(&[call("web_search", &args, "results", true)]);
        let note = t.take_note().expect("note at threshold");
        assert!(note.contains("Convergence reminder"));
        assert!(t.take_note().is_none(), "note fires once");
        assert!(!t.force_wrapup());
    }

    #[test]
    fn progress_resets_read_only_streak() {
        let mut t = StreakTracker::new();
        let args = json!({"query": "x"});
        for _ in 0..READ_ONLY_STREAK_NOTE_AT - 1 {
            t.observe_step(&[call("web_search", &args, "results", true)]);
        }
        t.observe_step(&[call("write_file", &args, "ok", false)]);
        for _ in 0..READ_ONLY_STREAK_NOTE_AT - 1 {
            t.observe_step(&[call("web_search", &args, "results", true)]);
        }
        assert!(t.take_note().is_none(), "streak restarted after progress");
    }

    #[test]
    fn same_host_errors_note_then_wrapup() {
        let mut t = StreakTracker::new();
        let args = json!({"url": "https://blocked.example/page"});
        let err = "ERROR: policy denied: [policy_denied] 被安全策略拒绝";
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        assert!(t.take_note().is_none());
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        let note = t.take_note().expect("note at 2");
        assert!(note.contains("blocked.example"));
        assert!(!t.force_wrapup());
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        assert!(t.force_wrapup(), "wrapup at 3");
    }

    #[test]
    fn different_hosts_do_not_accumulate() {
        // The Qwen paper-trace shape: several sources fail in a row,
        // a later source works. Must never trip the breaker.
        let mut t = StreakTracker::new();
        let err = "ERROR: Connection Failed: tls connection init failed";
        for host in ["a.example", "b.example", "c.example", "d.example"] {
            let args = json!({"url": format!("https://{host}/p")});
            t.observe_step(&[call("fetch_url", &args, err, true)]);
            assert!(!t.force_wrapup());
        }
        let args = json!({"url": "https://e.example/p"});
        t.observe_step(&[call("fetch_url", &args, "HTTP 200 content", true)]);
        assert!(!t.force_wrapup());
    }

    #[test]
    fn success_clears_error_streak() {
        let mut t = StreakTracker::new();
        let args = json!({"url": "https://flaky.example/p"});
        let err = "ERROR: Connection Failed: reset";
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        let _ = t.take_note();
        t.observe_step(&[call("fetch_url", &args, "HTTP 200 ok", true)]);
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        t.observe_step(&[call("fetch_url", &args, err, true)]);
        assert!(!t.force_wrapup(), "streak restarted after success");
    }

    #[test]
    fn blocked_pages_count_toward_the_breaker() {
        // A browser refusal arrives as a successful result carrying
        // the ⚠ [blocked] banner; same host three times is a wall.
        let mut t = StreakTracker::new();
        let args = json!({"url": "https://walled.example/search"});
        let page = "浏览器会话 bs_1（profile main），HTTP 200\n⚠ [blocked] 网站拒绝了访问（access denied）。";
        t.observe_step(&[call("browser_open", &args, page, true)]);
        assert!(t.take_note().is_none());
        t.observe_step(&[call("browser_open", &args, page, true)]);
        let note = t.take_note().expect("note at 2");
        assert!(note.contains("blocked"), "{note}");
        assert!(!t.force_wrapup());
        t.observe_step(&[call("browser_open", &args, page, true)]);
        assert!(t.force_wrapup(), "wrapup at 3");
    }

    #[test]
    fn blocked_pages_on_different_hosts_do_not_accumulate() {
        let mut t = StreakTracker::new();
        let page = "⚠ [blocked] 网站拒绝了访问（access denied）。";
        for host in ["a.example", "b.example", "c.example"] {
            let args = json!({"url": format!("https://{host}/p")});
            t.observe_step(&[call("browser_open", &args, page, true)]);
        }
        assert!(t.take_note().is_none());
        assert!(!t.force_wrapup());
    }

    #[test]
    fn structured_kind_tag_wins_over_heuristics() {
        assert_eq!(error_kind_of("ERROR: [denied] policy denied: blocked"), "denied");
        assert_eq!(error_kind_of("ERROR: [connector] Connection Failed"), "connector");
        // Untagged text still classifies by heuristic.
        assert_eq!(error_kind_of("ERROR: Connection Failed: reset"), "network");
    }

    #[test]
    fn target_extraction() {
        assert_eq!(target_of("fetch_url", &json!({"url": "https://x.example:8443/a?b=1"})), "x.example");
        assert_eq!(target_of("fs_read", &json!({"path": "src/main.rs"})), "src/main.rs");
        assert_eq!(target_of("exec_command", &json!({"command": "curl -s https://x"})), "curl");
        assert_eq!(target_of("web_search", &json!({"query": "q"})), "web_search");
    }
}
