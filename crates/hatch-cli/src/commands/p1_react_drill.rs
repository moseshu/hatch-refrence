//! P1 acceptance drill (c): an unseen web page read through a
//! bounded ReAct step inside a durable task. The page is served
//! locally by an in-test TCP listener, with a fact the scripted
//! model cannot know in advance (a fresh token baked into the page
//! per run). The fetch goes through `web_fetch` with the test-only
//! local seam (`HATCH_WEB_FETCH_ALLOW_LOCAL=1`); the research
//! artifact must contain the fact and the page URL as evidence.
//! No real site, no real login, no real model.

use super::p1_submit_drill::{only_task, run_chat_submit, setup, sweep_until, EnvGuard};
use crate::commands::task_list_drill::{approve_write, artifact_approval_id};
use crate::commands::task_resident::ResidentWorker;
use hatch_core::time::now;
use hatch_tasks::{TaskStatus, TaskStore};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Serve `body` on 127.0.0.1 (an ephemeral port) until `stop` is
/// set or the request budget runs out. Returns the URL. Each
/// request is answered with a minimal HTTP/1.1 200 + Content-Length.
fn serve_local(body: String, stop: Arc<AtomicBool>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let mut served = 0;
        while !stop.load(Ordering::SeqCst) && served < 8 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    served += 1;
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 4096];
                    let mut req = Vec::new();
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    format!("http://127.0.0.1:{port}/special.html")
}

#[test]
fn react_reads_unseen_local_page_and_reports_the_fact() {
    let token = format!(
        "杏仁酥-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    );
    let page = format!(
        "<html><body><h1>本周特价</h1><p>本周特价甜点是{token}，售价 18 元。</p></body></html>"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let url = serve_local(page, stop.clone());
    let _guard = EnvGuard::set(&[("HATCH_WEB_FETCH_ALLOW_LOCAL", std::path::Path::new("1"))]);
    let (app, _home) = setup();
    let report = run_chat_submit(
        &app,
        &format!("打开 {url}，找出本周特价甜点的名称和价格，写成调研简报"),
        json!({
            "goal": format!("打开 {url}，找出本周特价甜点的名称和价格，写成调研简报"),
            "observe": true,
        }),
    );
    assert_eq!(report.stop, hatch_agent::StopReason::Answered);
    let task_id = only_task(&app);
    let task = TaskStore::new(&app.db).get(&task_id).unwrap().unwrap();
    assert_eq!(task.kind, "react");
    let mut worker = ResidentWorker::new(&app);
    assert!(
        sweep_until(&app, &mut worker, &task_id, TaskStatus::Blocked, 30_000),
        "任务应在调研简报审批上阻塞"
    );
    approve_write(&app.db, &artifact_approval_id(&app.db, &task_id), now());
    assert!(
        sweep_until(&app, &mut worker, &task_id, TaskStatus::Done, 30_000),
        "批准后任务应完成"
    );
    let out_dir = app.home.join("task-out").join(&task_id);
    let research = std::fs::read_to_string(out_dir.join("research-v1.md")).unwrap();
    assert!(
        research.contains(&token),
        "调研简报应含页面上的事实（{token}）：\n{research}"
    );
    assert!(
        research.contains(&url),
        "调研简报应以页面 URL 作证据：\n{research}"
    );
    stop.store(true, Ordering::SeqCst);
}
