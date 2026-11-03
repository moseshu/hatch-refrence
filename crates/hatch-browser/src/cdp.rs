//! Chrome DevTools Protocol over a local WebSocket.
//!
//! One connection per tab. A reader thread receives every message: command
//! responses are routed to the waiting caller, and events go through an
//! optional *interceptor* first — so `Fetch.requestPaused` (destination
//! checks) and JavaScript dialogs are answered at once, even while no hatch
//! operation is running. All remaining events are queued for `wait_event`.

use crate::error::{BResult, BrowserError};
use hatch_core::CancelToken;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

/// Decides what to send back immediately for an event (`method`, `params`).
pub type Interceptor = Arc<dyn Fn(&str, &Value) -> Option<(String, Value)> + Send + Sync>;

const MAX_QUEUED_EVENTS: usize = 2000;
const SLICE: Duration = Duration::from_millis(40);

struct Shared {
    pending: Mutex<HashMap<u64, mpsc::Sender<Value>>>,
    events: Mutex<VecDeque<Value>>,
    cond: Condvar,
    closed: AtomicBool,
    next_id: AtomicU64,
    writer: Mutex<WebSocket<TcpStream>>,
}

impl Shared {
    fn send_raw(&self, id: u64, method: &str, params: Value) -> BResult<()> {
        let text = json!({"id": id, "method": method, "params": params}).to_string();
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.send(Message::Text(text)).map_err(|e| BrowserError::Unavailable(format!("CDP 发送失败：{e}")))
    }
}

pub struct CdpConn {
    shared: Arc<Shared>,
    tcp: TcpStream,
    reader: Option<JoinHandle<()>>,
}

fn reader_loop(mut ws: WebSocket<TcpStream>, shared: Arc<Shared>, interceptor: Option<Interceptor>) {
    loop {
        let msg = match ws.read() {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&msg) else { continue };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if let Some(tx) = shared.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                let _ = tx.send(v);
            }
            continue;
        }
        let method = v.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        if let Some(icpt) = &interceptor {
            if let Some((m, p)) = icpt(&method, v.get("params").unwrap_or(&Value::Null)) {
                let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
                let _ = shared.send_raw(id, &m, p);
            }
        }
        let mut q = shared.events.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= MAX_QUEUED_EVENTS {
            q.pop_front();
        }
        q.push_back(v);
        shared.cond.notify_all();
    }
    shared.closed.store(true, Ordering::SeqCst);
    shared.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
    shared.cond.notify_all();
}

fn cancelled(cancel: Option<&CancelToken>) -> bool {
    cancel.is_some_and(CancelToken::is_cancelled)
}

impl CdpConn {
    /// Connect to `ws://127.0.0.1:<port>/devtools/...`.
    pub fn connect(ws_url: &str, interceptor: Option<Interceptor>) -> BResult<CdpConn> {
        let addr = ws_url
            .strip_prefix("ws://")
            .and_then(|r| r.split('/').next())
            .ok_or_else(|| BrowserError::Protocol(format!("unexpected DevTools URL {ws_url}")))?;
        let sock = addr.parse().map_err(|_| BrowserError::Protocol(format!("bad DevTools address {addr}")))?;
        let tcp = TcpStream::connect_timeout(&sock, Duration::from_secs(5)).map_err(|e| BrowserError::Unavailable(format!("连接 DevTools 失败：{e}")))?;
        let _ = tcp.set_nodelay(true);
        let (read_half, _) = tungstenite::client::client(ws_url, tcp.try_clone().map_err(|e| BrowserError::Unavailable(e.to_string()))?)
            .map_err(|e| BrowserError::Unavailable(format!("DevTools 握手失败：{e}")))?;
        let write_half = WebSocket::from_raw_socket(tcp.try_clone().map_err(|e| BrowserError::Unavailable(e.to_string()))?, Role::Client, None);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            writer: Mutex::new(write_half),
        });
        let s2 = shared.clone();
        let reader = std::thread::Builder::new()
            .name("hatch-cdp-reader".into())
            .spawn(move || reader_loop(read_half, s2, interceptor))
            .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
        Ok(CdpConn { shared, tcp, reader: Some(reader) })
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Send a command and wait for its result.
    pub fn call(&self, method: &str, params: Value, timeout: Duration, cancel: Option<&CancelToken>) -> BResult<Value> {
        if self.is_closed() {
            return Err(BrowserError::SessionUnavailable("浏览器连接已关闭".into()));
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        if let Err(e) = self.shared.send_raw(id, method, params) {
            self.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + timeout;
        loop {
            match rx.recv_timeout(SLICE) {
                Ok(v) => {
                    if let Some(err) = v.get("error") {
                        let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                        return Err(BrowserError::Protocol(format!("{method}: {msg}")));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(BrowserError::SessionUnavailable("浏览器连接已关闭".into()));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancelled(cancel) {
                        self.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        return Err(BrowserError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        self.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        return Err(BrowserError::Timeout(format!("{method} 超过 {} 秒没有响应", timeout.as_secs())));
                    }
                }
            }
        }
    }

    /// Fire-and-forget command.
    pub fn send(&self, method: &str, params: Value) -> BResult<()> {
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        self.shared.send_raw(id, method, params)
    }

    /// Drop all queued events.
    pub fn clear_events(&self) {
        self.shared.events.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Remove and return queued events.
    pub fn drain_events(&self) -> Vec<Value> {
        self.shared.events.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }

    /// Wait for the first queued event matching `pred` (removing it). `Ok(None)` on timeout.
    pub fn wait_event(&self, pred: impl Fn(&Value) -> bool, timeout: Duration, cancel: Option<&CancelToken>) -> BResult<Option<Value>> {
        let deadline = Instant::now() + timeout;
        let mut q = self.shared.events.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(pos) = q.iter().position(&pred) {
                return Ok(q.remove(pos));
            }
            if cancelled(cancel) {
                return Err(BrowserError::Cancelled);
            }
            if self.is_closed() {
                return Err(BrowserError::SessionUnavailable("浏览器连接已关闭".into()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (g, _) = self.shared.cond.wait_timeout(q, SLICE.min(deadline - now)).unwrap_or_else(|e| e.into_inner());
            q = g;
        }
    }
}

impl Drop for CdpConn {
    fn drop(&mut self) {
        let _ = self.tcp.shutdown(Shutdown::Both);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Event method name.
pub fn method_of(v: &Value) -> &str {
    v.get("method").and_then(Value::as_str).unwrap_or("")
}
