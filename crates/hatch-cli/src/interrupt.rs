//! Ctrl-C handling for `hatch chat` / `hatch ask`.
//!
//! * First Ctrl-C during a turn: cancel that turn — the model call, running
//!   commands and every sub-agent — and return to the prompt.
//! * A second Ctrl-C within two seconds (or Ctrl-C while idle, twice): exit.
//!
//! Effects that already happened are not rolled back; the turn's receipt says
//! what was done.

use hatch_core::CancelToken;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

static CURRENT: Mutex<Option<CancelToken>> = Mutex::new(None);
static LAST_PRESS: Mutex<Option<Instant>> = Mutex::new(None);
static INSTALL: Once = Once::new();

const DOUBLE_PRESS: Duration = Duration::from_secs(2);

fn on_signal() {
    let now = Instant::now();
    let mut last = LAST_PRESS.lock().unwrap_or_else(|p| p.into_inner());
    if last.is_some_and(|t| now.duration_since(t) < DOUBLE_PRESS) {
        eprintln!("\n退出。");
        std::process::exit(130);
    }
    *last = Some(now);
    let current = CURRENT.lock().unwrap_or_else(|p| p.into_inner()).clone();
    match current {
        Some(t) if !t.is_cancelled() => {
            t.cancel("Ctrl-C");
            eprintln!(
                "\n^C 正在停止当前任务（包括子代理和正在运行的命令）… 2 秒内再按一次 Ctrl-C 退出"
            );
        }
        _ => eprintln!("\n（2 秒内再按一次 Ctrl-C 退出，或输入 /exit）"),
    }
}

/// Install the handler once. Failure (e.g. no signal support) is reported, not fatal.
pub fn install() {
    INSTALL.call_once(|| {
        if let Err(e) = ctrlc::set_handler(on_signal) {
            eprintln!("（无法安装 Ctrl-C 处理：{e}；中断将直接结束进程）");
        }
    });
}

/// Start a cancellable turn; Ctrl-C now cancels the returned token.
pub fn begin_turn() -> CancelToken {
    let t = CancelToken::new();
    *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(t.clone());
    t
}

/// The turn is over; Ctrl-C goes back to "press twice to exit".
pub fn end_turn() {
    *CURRENT.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_cancels_the_turn() {
        let t = begin_turn();
        on_signal();
        assert!(t.is_cancelled());
        assert_eq!(t.reason().as_deref(), Some("Ctrl-C"));
        end_turn();
        // Reset so a later press in another test is not a "double press".
        *LAST_PRESS.lock().unwrap() = None;
    }
}
