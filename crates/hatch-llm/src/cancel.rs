//! Cancellable model calls.
//!
//! Blocking HTTP calls cannot be interrupted from outside, so a cancellable
//! call runs on a worker thread while the caller waits on a channel and polls
//! the [`CancelToken`]. On cancellation the caller returns immediately with a
//! `Cancelled` error; the worker is told to abort (the SSE reader checks the
//! flag between chunks and drops the connection) and finishes on its own.
//! A buffered request that is already in flight completes server-side — its
//! usage is not recorded, because the answer is discarded.

use crate::stream::{StreamEvent, StreamSink};
use crate::types::{ChatModel, ChatRequest, ChatResponse};
use crate::LlmError;
use hatch_core::CancelToken;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

thread_local! {
    static ABORT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// True when the current (worker) thread's call was abandoned.
pub(crate) fn aborted() -> bool {
    ABORT.with(|a| a.borrow().as_ref().is_some_and(|f| f.load(Ordering::SeqCst)))
}

enum Msg {
    Event(StreamEvent),
    Done(Result<ChatResponse, LlmError>),
}

fn cancelled(token: &CancelToken) -> LlmError {
    LlmError::cancelled(token.reason().unwrap_or_else(|| "cancelled".into()))
}

/// Run one model attempt (buffered when `sink` is `None`, streaming otherwise)
/// so that `token` can stop it at any time.
pub(crate) fn call(
    model: &Arc<dyn ChatModel>,
    req: &ChatRequest,
    token: &CancelToken,
    mut sink: Option<&mut StreamSink<'_>>,
) -> Result<ChatResponse, LlmError> {
    if token.is_cancelled() {
        return Err(cancelled(token));
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let abort = Arc::new(AtomicBool::new(false));
    let (m, r, flag, streaming) = (model.clone(), req.clone(), abort.clone(), sink.is_some());
    std::thread::Builder::new()
        .name("hatch-model-call".into())
        .spawn(move || {
            ABORT.with(|a| *a.borrow_mut() = Some(flag));
            let res = if streaming {
                let events = tx.clone();
                m.chat_stream(&r, &mut |e| {
                    let _ = events.send(Msg::Event(e));
                })
            } else {
                m.chat(&r)
            };
            let _ = tx.send(Msg::Done(res));
        })
        .map_err(|e| LlmError::transient(format!("cannot start model call thread: {e}")))?;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Msg::Event(e)) => {
                if let Some(s) = sink.as_mut() {
                    s(e);
                }
            }
            Ok(Msg::Done(res)) => return res,
            Err(RecvTimeoutError::Timeout) => {
                if token.is_cancelled() {
                    abort.store(true, Ordering::SeqCst);
                    return Err(cancelled(token));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(LlmError::permanent("model call ended without a result"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;
    use std::time::Instant;

    /// A model that takes its time, like a real endpoint.
    struct Slow;
    impl ChatModel for Slow {
        fn name(&self) -> String {
            "slow".into()
        }
        fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
            std::thread::sleep(Duration::from_secs(3));
            Ok(ChatResponse::text("late"))
        }
    }

    #[test]
    fn cancel_returns_promptly_while_the_call_is_in_flight() {
        let model: Arc<dyn ChatModel> = Arc::new(Slow);
        let token = CancelToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            t2.cancel("Ctrl-C");
        });
        let started = Instant::now();
        let e = call(&model, &ChatRequest::new(vec![ChatMessage::user("hi")]), &token, None).unwrap_err();
        assert!(e.is_cancelled(), "{e}");
        assert!(e.message.contains("Ctrl-C"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn uncancelled_calls_return_the_answer_and_stream_events() {
        let model: Arc<dyn ChatModel> = Arc::new(crate::scripted::ScriptedModel::ok("m", vec![ChatResponse::text("ok")]));
        let mut got = Vec::new();
        let mut sink = |e: StreamEvent| got.push(e);
        let r = call(&model, &ChatRequest::new(vec![]), &CancelToken::new(), Some(&mut sink)).unwrap();
        assert_eq!(r.content.as_deref(), Some("ok"));
        assert!(got.contains(&StreamEvent::TextDelta("ok".into())));
    }
}
