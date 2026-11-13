//! SSE stream for task events (task 5): `GET /v1/tasks/{id}/events`
//! with `text/event-stream`. Replays every event after the cursor,
//! then polls for new events until the client disconnects. Split out
//! of `tasks.rs` (line-count discipline); the JSON endpoints stay
//! there.

use super::http::SseWriter;
use super::tasks::require_task;
use super::ApiCtx;
use hatch_mcp::http_proto::write_json;
use hatch_tasks::EventLog;
use serde_json::json;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `GET /v1/tasks/{id}/events` — SSE. Replays every event after the
/// cursor, then polls for new events until the client disconnects.
/// Each frame carries its sequence as the SSE `id`, so a
/// reconnecting client resumes exactly where it left off: no loss,
/// no duplication.
pub fn handle_events_stream(ctx: &ApiCtx, stream: &mut TcpStream, id: &str, after: i64) -> u16 {
    // The task must exist before the stream starts; a missing task
    // is a plain 404, not an SSE error frame.
    if let Err(e) = require_task(&ctx.app.db, id) {
        let reason = if e.status == 404 {
            "Not Found"
        } else {
            "Internal Server Error"
        };
        let _ = write_json(stream, e.status, reason, &[], &e.to_json().to_string());
        return e.status;
    }
    let writer = match SseWriter::start(stream) {
        Ok(w) => w,
        Err(_) => return 200,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let _heartbeat = writer.heartbeat(Arc::clone(&stop));
    let _ = writer.comment("connected");
    let log = EventLog::new(&ctx.app.db);
    let mut cursor = after;
    let mut last_write = Instant::now();
    while let Ok(events) = log.events_since(id, cursor, 100) {
        let mut wrote = false;
        for ev in events {
            let data = json!({
                "seq": ev.seq,
                "kind": ev.kind,
                "step_id": ev.step_id,
                "detail": ev.detail,
                "created_at": ev.created_at,
            })
            .to_string();
            if writer.event_with_id(ev.seq, "task_event", &data).is_err() {
                stop.store(true, Ordering::SeqCst);
                return 200;
            }
            cursor = ev.seq;
            wrote = true;
        }
        if wrote {
            last_write = Instant::now();
        } else if last_write.elapsed() >= Duration::from_secs(2) {
            // Keepalive + disconnect probe: the loop only learns a
            // client is gone when a write fails, so an idle stream
            // must write something. Without this, a client that
            // disconnects while idle leaves this handler thread
            // running forever — and the daemon's shutdown join never
            // returns (the pid file is already gone by then, so even
            // `daemon stop` cannot help). A failed ping ends the
            // stream here, within ~2s of the disconnect.
            if writer.comment("ping").is_err() {
                stop.store(true, Ordering::SeqCst);
                return 200;
            }
            last_write = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::SeqCst);
    200
}
