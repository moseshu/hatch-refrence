//! SIGTERM / SIGINT → a flag the daemon polls. The handler only stores an
//! atomic (async-signal-safe); the daemon turns it into cancellation.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Route SIGTERM and SIGINT to the stop flag.
pub fn install() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only performs an atomic store.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

pub fn stop_requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// For tests: simulate a signal.
pub fn request_stop() {
    REQUESTED.store(true, Ordering::SeqCst);
}
