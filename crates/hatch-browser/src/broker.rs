//! The broker: one browser per profile, one session per tab.
//!
//! * Profiles (`main`, `scheduler`, `research`) are separate Chromium
//!   processes with separate `user-data-dir`s — cookies and logins never
//!   cross between them.
//! * A session whose browser has gone away (process restart) is reopened
//!   lazily at its last URL in a fresh tab; typed form input is not kept.
//! * A session waiting for the user (login / captcha) refuses every action
//!   except reading, screenshots and closing until it is resumed.
//! * Tabs idle longer than the configured timeout are closed; browsers this
//!   process started are stopped when it exits (and die with it).

use crate::config::{profile_for_origin, BrowserConfig};
use crate::detect::{classify_page, PageCondition};
use crate::error::{BResult, BrowserError};
use crate::http::local_json;
use crate::launch::{kill_tree, launch, pid_alive, running_endpoint, Endpoint};
use crate::page::{Settled, Tab};
use crate::session::{SessionRecord, SessionState, SessionStore};
use crate::snapshot::{Element, FormField, Snapshot};
use crate::urlpolicy::{RequestKind, UrlPolicy};
use crate::view::{takeover_hint, PageView};
use hatch_core::CancelToken;
use hatch_store::Db;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Running {
    child: Option<Child>,
    endpoint: Endpoint,
}

struct LiveTab {
    tab: Tab,
    last_used: Instant,
    last: Option<Snapshot>,
    reopened: bool,
}

/// What a submission approval recorded; re-checked right before the click.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClickExpect {
    pub page_url: String,
    pub element: Element,
    #[serde(default)]
    pub form: Vec<FormField>,
}

/// Per-profile status for `hatch browser status`.
#[derive(Clone, Debug, Serialize)]
pub struct ProfileStatus {
    pub profile: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub owned_by_this_process: bool,
    pub open_sessions: usize,
}

pub struct Broker {
    pub cfg: BrowserConfig,
    pub policy: Arc<UrlPolicy>,
    browsers: Mutex<HashMap<String, Running>>,
    tabs: Mutex<HashMap<String, Arc<Mutex<LiveTab>>>>,
}

fn now() -> hatch_core::time::Timestamp {
    hatch_core::time::now()
}

fn strip_fragment(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Broker {
    pub fn new(cfg: BrowserConfig) -> Broker {
        let proxied = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
        let policy = Arc::new(UrlPolicy::new(cfg.allow_loopback, cfg.blocklist.clone(), proxied));
        Broker { cfg, policy, browsers: Mutex::new(HashMap::new()), tabs: Mutex::new(HashMap::new()) }
    }

    /// DevTools port of the browser for `profile`, starting one if `start`.
    fn port_for(&self, profile: &str, start: bool) -> BResult<u16> {
        let mut map = lock(&self.browsers);
        if let Some(r) = map.get_mut(profile) {
            let alive = match r.child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(None)),
                None => pid_alive(r.endpoint.pid),
            };
            if alive {
                return Ok(r.endpoint.port);
            }
            map.remove(profile);
        }
        if let Some(ep) = running_endpoint(&self.cfg, profile) {
            let port = ep.port;
            map.insert(profile.to_string(), Running { child: None, endpoint: ep });
            return Ok(port);
        }
        if !start {
            return Err(BrowserError::SessionUnavailable(format!("profile {profile} 的浏览器没有在运行")));
        }
        let l = launch(&self.cfg, profile, self.cfg.headless, "about:blank")?;
        let port = l.endpoint.port;
        map.insert(profile.to_string(), Running { child: Some(l.child), endpoint: l.endpoint });
        Ok(port)
    }

    fn new_target(&self, port: u16) -> BResult<(String, String)> {
        let v = local_json(port, "PUT", "/json/new?about:blank", Duration::from_secs(10))?;
        let id = v["id"].as_str().unwrap_or("").to_string();
        let ws = v["webSocketDebuggerUrl"].as_str().unwrap_or("").to_string();
        if id.is_empty() || ws.is_empty() {
            return Err(BrowserError::Protocol("新建标签页失败".into()));
        }
        Ok((id, ws))
    }

    /// Get (or re-establish) the live tab of a session.
    fn attach(&self, db: &Db, rec: &SessionRecord, cancel: Option<&CancelToken>) -> BResult<Arc<Mutex<LiveTab>>> {
        if let Some(t) = lock(&self.tabs).get(&rec.id).cloned() {
            if !lock(&t).tab.is_closed() {
                return Ok(t);
            }
        }
        if !rec.target_id.is_empty() {
            if let Ok(port) = self.port_for(&rec.profile, false) {
                let ws = format!("ws://127.0.0.1:{port}/devtools/page/{}", rec.target_id);
                if let Ok(tab) = Tab::attach(&ws, &rec.target_id, self.policy.clone(), self.cfg.nav_timeout) {
                    let lt = Arc::new(Mutex::new(LiveTab { tab, last_used: Instant::now(), last: None, reopened: false }));
                    lock(&self.tabs).insert(rec.id.clone(), lt.clone());
                    return Ok(lt);
                }
            }
        }
        // The tab is gone (browser restarted, or the user took the session over): reopen it.
        let port = self.port_for(&rec.profile, true)?;
        let (target, ws) = self.new_target(port)?;
        let tab = Tab::attach(&ws, &target, self.policy.clone(), self.cfg.nav_timeout)?;
        if !rec.url.is_empty() && rec.url != "about:blank" {
            self.policy.check(&rec.url, RequestKind::Navigation)?;
            tab.navigate(&rec.url, cancel)?;
        }
        db.conn()
            .execute("UPDATE browser_sessions SET target_id = ?2 WHERE id = ?1", hatch_store::params![rec.id, target])
            .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        let lt = Arc::new(Mutex::new(LiveTab { tab, last_used: Instant::now(), last: None, reopened: true }));
        lock(&self.tabs).insert(rec.id.clone(), lt.clone());
        Ok(lt)
    }

    /// Load a session for an action. `acting` actions are refused while the
    /// session waits for the user.
    fn session(&self, db: &Db, id: &str, acting: bool) -> BResult<SessionRecord> {
        let rec = SessionStore::new(db).get(id)?;
        match rec.state {
            SessionState::Closed => Err(BrowserError::SessionUnavailable(format!("会话 {id} 已关闭（{}）", rec.reason))),
            SessionState::NeedsUser if acting => {
                Err(BrowserError::SessionUnavailable(format!("会话 {id} 正在等待你接管（{}）。{}", rec.reason, takeover_hint(id))))
            }
            _ => Ok(rec),
        }
    }

    /// Snapshot + classification after an action; updates the session state.
    fn finish(&self, db: &Db, rec: &SessionRecord, lt: &mut LiveTab, settled: Settled, cancel: Option<&CancelToken>) -> BResult<PageView> {
        let store = SessionStore::new(db);
        let snap = lt.tab.snapshot(cancel)?;
        if let Err(e) = self.policy.check(&snap.url, RequestKind::Navigation) {
            let _ = lt.tab.navigate("about:blank", cancel);
            let _ = store.transition(&rec.id, SessionState::Ready, &format!("离开了被禁止的地址：{e}"), &now());
            return Err(e);
        }
        let condition = classify_page(&snap, settled.status);
        let mut blocked = settled.blocked;
        if lt.reopened {
            blocked.insert(0, "（浏览器曾重启：已在新标签页重新打开此页面，之前填写的内容没有保留）".into());
            lt.reopened = false;
        }
        let view = PageView {
            session: rec.id.clone(),
            profile: rec.profile.clone(),
            status: settled.status,
            condition: condition.clone(),
            snapshot: snap.clone(),
            blocked,
            dialogs: lt.tab.take_dialogs(),
            navigated: settled.navigated,
        };
        store.set_page(&rec.id, &snap.url, &snap.title, &now())?;
        let cur = store.get(&rec.id)?.state;
        match &condition {
            PageCondition::LoginRequired(d) | PageCondition::Captcha(d) if cur != SessionState::NeedsUser => {
                let what = if matches!(condition, PageCondition::Captcha(_)) { "人机验证" } else { "需要登录" };
                store.transition(&rec.id, SessionState::NeedsUser, &format!("{what}：{d}"), &now())?;
            }
            _ if matches!(cur, SessionState::Busy | SessionState::Navigating) => {
                store.transition(&rec.id, SessionState::Ready, "", &now())?;
            }
            _ => {}
        }
        if view.needs_user() {
            let _ = db.audit("browser", "browser_needs_user", &format!("{} {} {}", rec.id, snap.url, view.condition_label()));
        }
        lt.last = Some(snap);
        lt.last_used = Instant::now();
        Ok(view)
    }

    fn begin(&self, db: &Db, rec: &SessionRecord) -> BResult<()> {
        if rec.state == SessionState::Ready {
            SessionStore::new(db).transition(&rec.id, SessionState::Busy, "", &now())?;
        }
        Ok(())
    }

    /// Put a session back to Ready after a failed action.
    fn recover(&self, db: &Db, id: &str, e: &BrowserError) {
        let store = SessionStore::new(db);
        if let Ok(r) = store.get(id) {
            if matches!(r.state, SessionState::Busy | SessionState::Navigating) {
                let _ = store.transition(id, SessionState::Ready, e.category(), &now());
            }
        }
    }

    /// Open `url` in a new session of the profile for `origin`.
    pub fn open(&self, db: &Db, origin: &str, url: &str, cancel: Option<&CancelToken>) -> BResult<PageView> {
        self.policy.check(url, RequestKind::Navigation)?;
        let profile = profile_for_origin(origin);
        self.reap_idle(db);
        let store = SessionStore::new(db);
        if self.port_for(profile, false).is_err() {
            // No browser: sessions that were never resumed cannot come back.
            db.conn()
                .execute(
                    "UPDATE browser_sessions SET state='closed', reason='浏览器已退出' WHERE profile=?1 AND state!='closed' AND target_id!=''",
                    hatch_store::params![profile],
                )
                .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        }
        let open = store.list(false)?.into_iter().filter(|s| s.profile == profile).count();
        if open >= self.cfg.max_tabs {
            return Err(BrowserError::PolicyDenied(format!(
                "profile {profile} 已有 {open} 个打开的会话（上限 {}），请先用 browser_close 关闭不用的会话",
                self.cfg.max_tabs
            )));
        }
        let port = self.port_for(profile, true)?;
        let (target, ws) = self.new_target(port)?;
        let id = store.create(profile, origin, &target, &now())?;
        let tab = match Tab::attach(&ws, &target, self.policy.clone(), self.cfg.nav_timeout) {
            Ok(t) => t,
            Err(e) => {
                let _ = store.transition(&id, SessionState::Closed, e.category(), &now());
                return Err(e);
            }
        };
        let lt = Arc::new(Mutex::new(LiveTab { tab, last_used: Instant::now(), last: None, reopened: false }));
        lock(&self.tabs).insert(id.clone(), lt.clone());
        store.transition(&id, SessionState::Navigating, "", &now())?;
        let rec = store.get(&id)?;
        let mut g = lock(&lt);
        let result = g.tab.navigate(url, cancel).and_then(|s| self.finish(db, &rec, &mut g, s, cancel));
        drop(g);
        let _ = db.audit("browser", "browser_open", &format!("{id} [{profile}] {url}"));
        if let Err(e) = &result {
            // A page that never loaded leaves nothing worth keeping open.
            let _ = self.close(db, &id);
            return Err(e.clone());
        }
        result
    }

    pub fn navigate(&self, db: &Db, id: &str, url: &str, cancel: Option<&CancelToken>) -> BResult<PageView> {
        self.policy.check(url, RequestKind::Navigation)?;
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        self.begin(db, &rec)?;
        SessionStore::new(db).transition(id, SessionState::Navigating, "", &now())?;
        let mut g = lock(&lt);
        let r = g.tab.navigate(url, cancel).and_then(|s| self.finish(db, &rec, &mut g, s, cancel));
        if let Err(e) = &r {
            self.recover(db, id, e);
        }
        r
    }

    /// Current page (optionally waiting until `wait_for` appears).
    pub fn read(&self, db: &Db, id: &str, wait_for: Option<&str>, cancel: Option<&CancelToken>) -> BResult<PageView> {
        let rec = self.session(db, id, false)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        if let Some(w) = wait_for.filter(|w| !w.trim().is_empty()) {
            g.tab.wait_for_text(w.trim(), self.cfg.nav_timeout, cancel)?;
        }
        self.finish(db, &rec, &mut g, Settled::default(), cancel)
    }

    pub fn find(&self, db: &Db, id: &str, query: &str, cancel: Option<&CancelToken>) -> BResult<Vec<String>> {
        let rec = self.session(db, id, false)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        let snap = g.tab.snapshot(cancel)?;
        let hits = snap.find(query, 30);
        g.last = Some(snap);
        g.last_used = Instant::now();
        Ok(hits)
    }

    /// The element `reference` and the page it is on (for classifying a click).
    pub fn describe(&self, db: &Db, id: &str, reference: u32, cancel: Option<&CancelToken>) -> BResult<(Element, Snapshot)> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        let g = lock(&lt);
        let el = g.tab.element(reference, cancel)?;
        let page = g.tab.page_info(cancel)?;
        Ok((el, page))
    }

    pub fn form_fields(&self, db: &Db, id: &str, reference: u32, cancel: Option<&CancelToken>) -> BResult<Vec<FormField>> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        let g = lock(&lt);
        g.tab.form_fields(reference, cancel)
    }

    /// Click `reference`. With `expect`, the page, element and form must
    /// still be exactly what was approved.
    pub fn click(&self, db: &Db, id: &str, reference: u32, expect: Option<&ClickExpect>, cancel: Option<&CancelToken>) -> BResult<PageView> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        if let Some(x) = expect {
            let page = g.tab.page_info(cancel)?;
            let el = g.tab.element(reference, cancel)?;
            let same_el = el.tag == x.element.tag && el.text == x.element.text && el.name == x.element.name && el.href == x.element.href;
            let form = if x.form.is_empty() { Vec::new() } else { g.tab.form_fields(reference, cancel)? };
            if strip_fragment(&page.url) != strip_fragment(&x.page_url) || !same_el || form != x.form {
                return Err(BrowserError::PolicyDenied("页面、按钮或表单内容在审批之后发生了变化，没有点击；请重新查看页面并重新提交".into()));
            }
        }
        self.begin(db, &rec)?;
        let r = g.tab.click(reference, cancel).and_then(|s| self.finish(db, &rec, &mut g, s, cancel));
        if let Err(e) = &r {
            self.recover(db, id, e);
        }
        r
    }

    pub fn type_text(&self, db: &Db, id: &str, reference: u32, text: &str, cancel: Option<&CancelToken>) -> BResult<Element> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        let el = g.tab.element(reference, cancel)?;
        if el.is_password() {
            return Err(BrowserError::PolicyDenied(format!("hatch 不会在密码框中输入任何内容。{}", takeover_hint(id))));
        }
        if !(el.is_text_input() || el.role == "textbox") {
            return Err(BrowserError::ElementNotFound(format!("编号 {reference} 不是文本输入框（{}）", el.render())));
        }
        g.tab.type_text(reference, text, cancel)?;
        g.last_used = Instant::now();
        g.tab.element(reference, cancel)
    }

    pub fn select(&self, db: &Db, id: &str, reference: u32, option: &str, cancel: Option<&CancelToken>) -> BResult<String> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        g.last_used = Instant::now();
        g.tab.select(reference, option, cancel)
    }

    pub fn scroll(&self, db: &Db, id: &str, dy: i64, cancel: Option<&CancelToken>) -> BResult<PageView> {
        let rec = self.session(db, id, false)?;
        let lt = self.attach(db, &rec, cancel)?;
        let mut g = lock(&lt);
        g.tab.scroll(dy, cancel)?;
        self.finish(db, &rec, &mut g, Settled::default(), cancel)
    }

    pub fn back(&self, db: &Db, id: &str, cancel: Option<&CancelToken>) -> BResult<PageView> {
        let rec = self.session(db, id, true)?;
        let lt = self.attach(db, &rec, cancel)?;
        self.begin(db, &rec)?;
        let mut g = lock(&lt);
        let r = g.tab.back(cancel).and_then(|s| self.finish(db, &rec, &mut g, s, cancel));
        if let Err(e) = &r {
            self.recover(db, id, e);
        }
        r
    }

    pub fn screenshot(&self, db: &Db, id: &str, cancel: Option<&CancelToken>) -> BResult<PathBuf> {
        let rec = self.session(db, id, false)?;
        let lt = self.attach(db, &rec, cancel)?;
        let g = lock(&lt);
        let path = self.cfg.screenshots_dir.join(format!("{id}-{}.png", now().format("%Y%m%d-%H%M%S%3f")));
        g.tab.screenshot(&path, cancel)?;
        Ok(path)
    }

    /// Close the tab (if its browser is still running) and the session.
    pub fn close(&self, db: &Db, id: &str) -> BResult<()> {
        let store = SessionStore::new(db);
        let rec = store.get(id)?;
        lock(&self.tabs).remove(id);
        if !rec.target_id.is_empty() {
            if let Ok(port) = self.port_for(&rec.profile, false) {
                let _ = crate::http::local_request(port, "GET", &format!("/json/close/{}", rec.target_id), Duration::from_secs(5));
            }
        }
        if rec.state != SessionState::Closed {
            store.transition(id, SessionState::Closed, "已关闭", &now())?;
        }
        Ok(())
    }

    /// After the user took over: let the next action reopen the page.
    pub fn resume(&self, db: &Db, id: &str) -> BResult<()> {
        let store = SessionStore::new(db);
        let rec = store.get(id)?;
        if rec.state != SessionState::NeedsUser {
            return Err(BrowserError::SessionUnavailable(format!("会话 {id} 当前是 {}，不需要恢复", rec.state.as_str())));
        }
        lock(&self.tabs).remove(id);
        db.conn()
            .execute("UPDATE browser_sessions SET target_id = '' WHERE id = ?1", hatch_store::params![id])
            .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        store.transition(id, SessionState::Ready, "用户已接管完成", &now())
    }

    /// Close tabs idle longer than the timeout; stop own browsers without sessions.
    pub fn reap_idle(&self, db: &Db) {
        let idle: Vec<String> = lock(&self.tabs)
            .iter()
            .filter(|(_, t)| t.try_lock().map(|g| g.last_used.elapsed() > self.cfg.idle_timeout).unwrap_or(false))
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            let _ = self.close(db, &id);
            let _ = SessionStore::new(db).transition(&id, SessionState::Closed, "空闲超时自动关闭", &now());
        }
        let open: Vec<String> = SessionStore::new(db).list(false).map(|v| v.into_iter().map(|s| s.profile).collect()).unwrap_or_default();
        let mut map = lock(&self.browsers);
        let stop: Vec<String> = map.iter().filter(|(p, r)| r.child.is_some() && !open.contains(p)).map(|(p, _)| p.clone()).collect();
        for p in stop {
            if let Some(mut r) = map.remove(&p) {
                if let Some(c) = r.child.as_mut() {
                    kill_tree(c);
                }
            }
        }
    }

    pub fn status(&self, db: &Db) -> Vec<ProfileStatus> {
        let sessions = SessionStore::new(db).list(false).unwrap_or_default();
        crate::config::PROFILES
            .iter()
            .map(|p| {
                let ep = running_endpoint(&self.cfg, p);
                ProfileStatus {
                    profile: p.to_string(),
                    running: ep.is_some(),
                    pid: ep.as_ref().map(|e| e.pid),
                    owned_by_this_process: ep.as_ref().is_some_and(|e| e.owner_pid == std::process::id()),
                    open_sessions: sessions.iter().filter(|s| s.profile == *p).count(),
                }
            })
            .collect()
    }
}

impl PageView {
    fn condition_label(&self) -> &'static str {
        match self.condition {
            PageCondition::Ok => "ok",
            PageCondition::LoginRequired(_) => "login_required",
            PageCondition::Captcha(_) => "captcha",
            PageCondition::Blocked(_) => "blocked",
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        lock(&self.tabs).clear();
        for (_, mut r) in lock(&self.browsers).drain() {
            if let Some(c) = r.child.as_mut() {
                kill_tree(c);
            }
        }
    }
}
