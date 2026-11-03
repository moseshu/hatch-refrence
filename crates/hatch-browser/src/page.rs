//! Actions on one tab: navigate, snapshot, click, type, select, scroll,
//! screenshot, back. Destination checks run inside the CDP reader thread for
//! every request the page makes while this connection is open.

use crate::cdp::{method_of, CdpConn, Interceptor};
use crate::error::{BResult, BrowserError};
use crate::snapshot::{element_js, form_fields_js, page_info_js, snapshot_js, Element, FormField, Snapshot};
use crate::urlpolicy::{RequestKind, UrlPolicy};
use base64::Engine;
use hatch_core::CancelToken;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Things the reader thread observed on our behalf.
#[derive(Default)]
pub struct Observed {
    /// `url — reason` for every request refused by the destination policy.
    pub blocked: Vec<String>,
    /// JavaScript dialogs that were dismissed.
    pub dialogs: Vec<String>,
}

pub struct Tab {
    conn: CdpConn,
    pub target_id: String,
    main_frame: String,
    observed: Arc<Mutex<Observed>>,
    nav_timeout: Duration,
}

fn interceptor(policy: Arc<UrlPolicy>, observed: Arc<Mutex<Observed>>) -> Interceptor {
    Arc::new(move |method: &str, params: &Value| match method {
        "Fetch.requestPaused" => {
            let id = params.get("requestId").cloned().unwrap_or(Value::Null);
            let url = params.pointer("/request/url").and_then(Value::as_str).unwrap_or("");
            let kind = if params.get("resourceType").and_then(Value::as_str) == Some("Document") { RequestKind::Navigation } else { RequestKind::Subresource };
            match policy.check(url, kind) {
                Ok(()) => Some(("Fetch.continueRequest".into(), json!({"requestId": id}))),
                Err(e) => {
                    observed.lock().unwrap_or_else(|e| e.into_inner()).blocked.push(format!("{} — {e}", url.chars().take(200).collect::<String>()));
                    Some(("Fetch.failRequest".into(), json!({"requestId": id, "errorReason": "BlockedByClient"})))
                }
            }
        }
        "Page.javascriptDialogOpening" => {
            let msg = params.get("message").and_then(Value::as_str).unwrap_or("");
            observed.lock().unwrap_or_else(|e| e.into_inner()).dialogs.push(msg.chars().take(200).collect());
            Some(("Page.handleJavaScriptDialog".into(), json!({"accept": false})))
        }
        _ => None,
    })
}

/// Result of an action that may load a new page.
#[derive(Clone, Debug, Default)]
pub struct Settled {
    pub navigated: bool,
    /// HTTP status of the main document, if one loaded.
    pub status: Option<u16>,
    /// Requests refused by the destination policy during the action.
    pub blocked: Vec<String>,
}

impl Tab {
    /// Attach to a page target and enable events and request interception.
    pub fn attach(ws_url: &str, target_id: &str, policy: Arc<UrlPolicy>, nav_timeout: Duration) -> BResult<Tab> {
        let observed = Arc::new(Mutex::new(Observed::default()));
        let conn = CdpConn::connect(ws_url, Some(interceptor(policy, observed.clone())))?;
        conn.call("Page.enable", json!({}), CALL_TIMEOUT, None)?;
        conn.call("Network.enable", json!({}), CALL_TIMEOUT, None)?;
        conn.call("Fetch.enable", json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}), CALL_TIMEOUT, None)?;
        let tree = conn.call("Page.getFrameTree", json!({}), CALL_TIMEOUT, None)?;
        let main_frame = tree.pointer("/frameTree/frame/id").and_then(Value::as_str).unwrap_or("").to_string();
        Ok(Tab { conn, target_id: target_id.to_string(), main_frame, observed, nav_timeout })
    }

    pub fn is_closed(&self) -> bool {
        self.conn.is_closed()
    }

    fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.observed.lock().unwrap_or_else(|e| e.into_inner()).blocked)
    }

    pub fn take_dialogs(&self) -> Vec<String> {
        std::mem::take(&mut self.observed.lock().unwrap_or_else(|e| e.into_inner()).dialogs)
    }

    fn main_frame_event(&self, v: &Value, method: &str) -> bool {
        method_of(v) == method && {
            let f = v.pointer("/params/frameId").or_else(|| v.pointer("/params/frame/id")).and_then(Value::as_str).unwrap_or("");
            f.is_empty() || f == self.main_frame
        }
    }

    /// Evaluate one of our fixed scripts and return its value.
    fn eval(&self, expr: &str, cancel: Option<&CancelToken>) -> BResult<Value> {
        let r = self.conn.call("Runtime.evaluate", json!({"expression": expr, "returnByValue": true, "awaitPromise": false}), CALL_TIMEOUT, cancel)?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex.pointer("/exception/description").or_else(|| ex.get("text")).and_then(Value::as_str).unwrap_or("script error");
            return Err(BrowserError::Protocol(msg.chars().take(200).collect()));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// Wait for the page to finish loading after a navigation was started.
    fn wait_loaded(&self, cancel: Option<&CancelToken>) -> BResult<bool> {
        Ok(self.conn.wait_event(|v| method_of(v) == "Page.loadEventFired", self.nav_timeout, cancel)?.is_some())
    }

    fn main_status(&self, events: &[Value]) -> Option<u16> {
        events
            .iter()
            .filter(|v| method_of(v) == "Network.responseReceived")
            .filter(|v| v.pointer("/params/type").and_then(Value::as_str) == Some("Document"))
            .filter(|v| match v.pointer("/params/frameId").and_then(Value::as_str) {
                None => true,
                Some(f) => f == self.main_frame,
            })
            .filter_map(|v| v.pointer("/params/response/status").and_then(Value::as_f64))
            .map(|s| s as u16)
            .next_back()
    }

    pub fn navigate(&self, url: &str, cancel: Option<&CancelToken>) -> BResult<Settled> {
        self.take_blocked();
        self.conn.clear_events();
        let r = self.conn.call("Page.navigate", json!({"url": url}), self.nav_timeout, cancel)?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            let blocked = self.take_blocked();
            if let Some(b) = blocked.first() {
                return Err(BrowserError::PolicyDenied(format!("导航被拦截：{b}")));
            }
            return Err(BrowserError::Network(format!("{err}（{url}）")));
        }
        let loaded = self.wait_loaded(cancel)?;
        let events = self.conn.drain_events();
        let blocked = self.take_blocked();
        if !loaded && events.iter().all(|v| method_of(v) != "Page.domContentEventFired") {
            return Err(BrowserError::Timeout(format!("{url} 在 {} 秒内没有加载完成", self.nav_timeout.as_secs())));
        }
        Ok(Settled { navigated: true, status: self.main_status(&events), blocked })
    }

    /// After an input event: did a navigation start? If so wait for it.
    fn settle(&self, cancel: Option<&CancelToken>) -> BResult<Settled> {
        let started = self.conn.wait_event(
            |v| {
                ["Page.frameRequestedNavigation", "Page.frameStartedNavigating", "Page.frameStartedLoading", "Page.frameNavigated"]
                    .iter()
                    .any(|m| self.main_frame_event(v, m))
            },
            Duration::from_millis(800),
            cancel,
        )?;
        let mut navigated = false;
        if started.is_some() {
            navigated = true;
            self.wait_loaded(cancel)?;
        }
        let events = self.conn.drain_events();
        Ok(Settled { navigated, status: self.main_status(&events), blocked: self.take_blocked() })
    }

    pub fn snapshot(&self, cancel: Option<&CancelToken>) -> BResult<Snapshot> {
        let v = self.eval(&snapshot_js(), cancel)?;
        v.as_str().and_then(Snapshot::parse).ok_or_else(|| BrowserError::Protocol("页面快照解析失败".into()))
    }

    /// URL and payment markers only (does not re-tag elements).
    pub fn page_info(&self, cancel: Option<&CancelToken>) -> BResult<Snapshot> {
        let v = self.eval(&page_info_js(), cancel)?;
        v.as_str().and_then(Snapshot::parse).ok_or_else(|| BrowserError::Protocol("页面信息解析失败".into()))
    }

    pub fn element(&self, reference: u32, cancel: Option<&CancelToken>) -> BResult<Element> {
        let v = self.eval(&element_js(reference), cancel)?;
        v.as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(|| BrowserError::ElementNotFound(format!("编号 {reference}")))
    }

    pub fn form_fields(&self, reference: u32, cancel: Option<&CancelToken>) -> BResult<Vec<FormField>> {
        let v = self.eval(&form_fields_js(reference), cancel)?;
        v.as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(|| BrowserError::ElementNotFound(format!("编号 {reference}")))
    }

    pub fn click(&self, reference: u32, cancel: Option<&CancelToken>) -> BResult<Settled> {
        let js = format!(
            "(() => {{ const el = document.querySelector('[data-hatch-ref=\"{reference}\"]'); if (!el) return null;
              el.removeAttribute('target'); if (el.form) el.form.removeAttribute('target');
              el.scrollIntoView({{block: 'center', inline: 'center'}}); const r = el.getBoundingClientRect();
              return JSON.stringify({{x: r.left + r.width / 2, y: r.top + r.height / 2}}); }})()"
        );
        let v = self.eval(&js, cancel)?;
        let pos: Value = v.as_str().and_then(|s| serde_json::from_str(s).ok()).ok_or_else(|| BrowserError::ElementNotFound(format!("编号 {reference}")))?;
        let (x, y) = (pos["x"].as_f64().unwrap_or(0.0), pos["y"].as_f64().unwrap_or(0.0));
        self.take_blocked();
        self.conn.clear_events();
        for (ty, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let mut p = json!({"type": ty, "x": x, "y": y});
            if ty != "mouseMoved" {
                p["button"] = json!("left");
                p["clickCount"] = json!(1);
                p["buttons"] = json!(buttons);
            }
            self.conn.call("Input.dispatchMouseEvent", p, CALL_TIMEOUT, cancel)?;
        }
        self.settle(cancel)
    }

    pub fn type_text(&self, reference: u32, text: &str, cancel: Option<&CancelToken>) -> BResult<()> {
        let js = format!(
            "(() => {{ const el = document.querySelector('[data-hatch-ref=\"{reference}\"]'); if (!el) return false;
              el.focus(); if ('value' in el) {{ el.value = ''; el.dispatchEvent(new Event('input', {{bubbles: true}})); }} return true; }})()"
        );
        if self.eval(&js, cancel)? != Value::Bool(true) {
            return Err(BrowserError::ElementNotFound(format!("编号 {reference}")));
        }
        self.conn.call("Input.insertText", json!({"text": text}), CALL_TIMEOUT, cancel)?;
        let done = format!(
            "(() => {{ const el = document.querySelector('[data-hatch-ref=\"{reference}\"]'); if (el) el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()"
        );
        self.eval(&done, cancel)?;
        Ok(())
    }

    /// Choose an option by its visible text (exact, then unique case-insensitive substring).
    pub fn select(&self, reference: u32, option: &str, cancel: Option<&CancelToken>) -> BResult<String> {
        let want = serde_json::to_string(option.trim()).unwrap_or_default();
        let js = format!(
            "(() => {{ const el = document.querySelector('[data-hatch-ref=\"{reference}\"]'); if (!el || el.tagName !== 'SELECT') return null;
              const opts = Array.from(el.options).map(o => o.text.replace(/\\s+/g, ' ').trim()); const w = {want};
              let i = opts.findIndex(o => o === w);
              if (i < 0) {{ const m = opts.map((o, k) => [o, k]).filter(([o]) => o.toLowerCase().includes(w.toLowerCase())); if (m.length === 1) i = m[0][1]; }}
              if (i >= 0) {{ el.selectedIndex = i; el.dispatchEvent(new Event('input', {{bubbles: true}})); el.dispatchEvent(new Event('change', {{bubbles: true}})); }}
              return JSON.stringify({{chosen: i >= 0 ? opts[i] : null, options: opts}}); }})()"
        );
        let v = self.eval(&js, cancel)?;
        let r: Value = v.as_str().and_then(|s| serde_json::from_str(s).ok()).ok_or_else(|| BrowserError::ElementNotFound(format!("编号 {reference} 不是下拉框")))?;
        match r["chosen"].as_str() {
            Some(c) => Ok(c.to_string()),
            None => Err(BrowserError::NoSuchOption {
                wanted: option.trim().to_string(),
                available: r["options"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
            }),
        }
    }

    pub fn scroll(&self, dy: i64, cancel: Option<&CancelToken>) -> BResult<i64> {
        let v = self.eval(&format!("(() => {{ window.scrollBy(0, {dy}); return Math.round(window.scrollY); }})()"), cancel)?;
        Ok(v.as_i64().unwrap_or(0))
    }

    pub fn screenshot(&self, path: &Path, cancel: Option<&CancelToken>) -> BResult<usize> {
        let r = self.conn.call("Page.captureScreenshot", json!({"format": "png"}), CALL_TIMEOUT, cancel)?;
        let data = r.get("data").and_then(Value::as_str).ok_or_else(|| BrowserError::Protocol("截图没有数据".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| BrowserError::Protocol(e.to_string()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        }
        std::fs::write(path, &bytes).map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        Ok(bytes.len())
    }

    pub fn back(&self, cancel: Option<&CancelToken>) -> BResult<Settled> {
        let h = self.conn.call("Page.getNavigationHistory", json!({}), CALL_TIMEOUT, cancel)?;
        let idx = h["currentIndex"].as_i64().unwrap_or(0);
        if idx <= 0 {
            return Err(BrowserError::ElementNotFound("没有可以返回的上一页".into()));
        }
        let entry = h["entries"].get((idx - 1) as usize).and_then(|e| e.get("id")).cloned().unwrap_or(Value::Null);
        self.take_blocked();
        self.conn.clear_events();
        self.conn.call("Page.navigateToHistoryEntry", json!({"entryId": entry}), CALL_TIMEOUT, cancel)?;
        self.wait_loaded(cancel)?;
        let events = self.conn.drain_events();
        Ok(Settled { navigated: true, status: self.main_status(&events), blocked: self.take_blocked() })
    }

    /// Poll the page until its text contains `needle`.
    pub fn wait_for_text(&self, needle: &str, timeout: Duration, cancel: Option<&CancelToken>) -> BResult<Snapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.snapshot(cancel)?;
            if s.text.contains(needle) {
                return Ok(s);
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!("{} 秒内页面上没有出现「{needle}」", timeout.as_secs())));
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(BrowserError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
