//! SSE streaming for chat turns.
//!
//! One event is `event: <name>\ndata: <json>\n\n`. All writes go through
//! a single mutex so a heartbeat thread can share the socket with the
//! turn thread without interleaving frames. The stream has no
//! `Content-Length`; the client reads until the connection closes.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// `: ping` heartbeat interval for SSE streams.
const HEARTBEAT_SECS: u64 = 15;

/// An SSE stream over a socket. Cloneable: the heartbeat thread holds
/// a clone while the turn thread writes events.
#[derive(Clone)]
pub struct SseWriter {
    stream: Arc<Mutex<TcpStream>>,
    last_write: Arc<Mutex<Instant>>,
}

impl SseWriter {
    /// Write the SSE response headers on `stream` and take ownership of
    /// a clone for event writes.
    pub fn start(stream: &mut TcpStream) -> std::io::Result<Self> {
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        )?;
        stream.flush()?;
        let clone = stream.try_clone()?;
        Ok(SseWriter {
            stream: Arc::new(Mutex::new(clone)),
            last_write: Arc::new(Mutex::new(Instant::now())),
        })
    }

    /// One `event: <name>` frame with a JSON payload.
    pub fn event(&self, name: &str, data: &str) -> std::io::Result<()> {
        let frame = format!("event: {name}\ndata: {data}\n\n");
        let mut stream = self.stream.lock().unwrap();
        stream.write_all(frame.as_bytes())?;
        stream.flush()?;
        *self.last_write.lock().unwrap() = Instant::now();
        Ok(())
    }

    /// One SSE comment line (used for the `: connected` opener).
    pub fn comment(&self, text: &str) -> std::io::Result<()> {
        let frame = format!(": {text}\n\n");
        let mut stream = self.stream.lock().unwrap();
        stream.write_all(frame.as_bytes())?;
        stream.flush()?;
        *self.last_write.lock().unwrap() = Instant::now();
        Ok(())
    }

    /// Spawn a heartbeat thread: `: ping` whenever no event has been
    /// written for [`HEARTBEAT_SECS`]. The thread exits when `stop` is
    /// set; a failed write (client gone) also ends it.
    pub fn heartbeat(&self, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        let writer = self.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let idle = writer.last_write.lock().unwrap().elapsed();
                if idle >= Duration::from_secs(HEARTBEAT_SECS) {
                    let mut stream = writer.stream.lock().unwrap();
                    if stream.write_all(b": ping\n\n").is_err() {
                        break;
                    }
                    if stream.flush().is_err() {
                        break;
                    }
                    *writer.last_write.lock().unwrap() = Instant::now();
                }
            }
        })
    }
}

/// Escape a string for embedding in an SSE `data:` line (newlines in
/// the payload would otherwise break the frame). SSE `data:` lines
/// must not contain raw `\n`; JSON encoding already escapes them, so
/// this is a safety net for hand-built payloads.
pub fn sse_safe(data: &str) -> String {
    data.replace('\n', "\\n").replace('\r', "\\r")
}
