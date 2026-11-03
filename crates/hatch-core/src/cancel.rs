//! Cooperative cancellation.
//!
//! A [`CancelToken`] is cheap to clone (shared state behind an `Arc`). Tokens
//! form a tree: [`CancelToken::child`] creates a token that is cancelled when
//! its parent is, while cancelling the child leaves the parent and siblings
//! untouched. Long-running work (the turn loop, model calls, sandboxed
//! commands, web fetches, sub-agents) polls [`CancelToken::is_cancelled`] at
//! safe points and stops with the third terminal state — *cancelled* — which
//! is neither success nor failure. Effects that already happened are not
//! rolled back.

use crate::{Error, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    reason: Mutex<Option<String>>,
    parent: Option<CancelToken>,
}

#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// A fresh root token.
    pub fn new() -> Self {
        Self::default()
    }

    /// A token cancelled together with `self`, but cancellable on its own.
    pub fn child(&self) -> Self {
        CancelToken { inner: Arc::new(Inner { parent: Some(self.clone()), ..Default::default() }) }
    }

    /// Request cancellation. The first reason wins.
    pub fn cancel(&self, reason: impl Into<String>) {
        let mut r = self.inner.reason.lock().unwrap_or_else(|p| p.into_inner());
        if r.is_none() {
            *r = Some(reason.into());
        }
        self.inner.flag.store(true, Ordering::SeqCst);
    }

    /// Cancelled directly or through any ancestor.
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst) || self.inner.parent.as_ref().is_some_and(|p| p.is_cancelled())
    }

    /// The reason given by whichever token in the chain was cancelled first
    /// (own reason preferred).
    pub fn reason(&self) -> Option<String> {
        if self.inner.flag.load(Ordering::SeqCst) {
            if let Some(r) = self.inner.reason.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                return Some(r);
            }
        }
        self.inner.parent.as_ref().and_then(|p| p.reason())
    }

    /// `Err(Error::Cancelled)` once cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled(self.reason().unwrap_or_else(|| "cancelled".into())))
        } else {
            Ok(())
        }
    }

    /// Sleep up to `d`, waking early (and returning `true`) on cancellation.
    pub fn sleep(&self, d: Duration) -> bool {
        let step = Duration::from_millis(20);
        let mut left = d;
        while !left.is_zero() {
            if self.is_cancelled() {
                return true;
            }
            let s = left.min(step);
            std::thread::sleep(s);
            left -= s;
        }
        self.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_cancels_children_but_not_the_reverse() {
        let root = CancelToken::new();
        let a = root.child();
        let b = root.child();
        let a1 = a.child();
        a.cancel("stop a");
        assert!(a.is_cancelled() && a1.is_cancelled());
        assert!(!root.is_cancelled() && !b.is_cancelled());
        assert_eq!(a1.reason().as_deref(), Some("stop a"));
        root.cancel("Ctrl-C");
        root.cancel("ignored second reason");
        assert!(b.is_cancelled());
        assert_eq!(b.reason().as_deref(), Some("Ctrl-C"));
        assert_eq!(root.reason().as_deref(), Some("Ctrl-C"));
        assert!(matches!(b.check(), Err(Error::Cancelled(r)) if r == "Ctrl-C"));
        assert!(CancelToken::new().check().is_ok());
    }

    #[test]
    fn sleep_wakes_on_cancel_from_another_thread() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            t2.cancel("x");
        });
        let started = std::time::Instant::now();
        assert!(t.sleep(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
    }
}
