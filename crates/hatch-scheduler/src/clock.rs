//! Time source, injectable so schedules can be tested across days in milliseconds.

use chrono::Duration;
use hatch_core::time::Timestamp;
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        hatch_core::time::now()
    }
}

/// A manually advanced clock.
#[derive(Debug)]
pub struct FakeClock {
    t: Mutex<Timestamp>,
}

impl FakeClock {
    pub fn new(start: Timestamp) -> Self {
        FakeClock { t: Mutex::new(start) }
    }

    pub fn set(&self, t: Timestamp) {
        *self.t.lock().unwrap_or_else(|p| p.into_inner()) = t;
    }

    pub fn advance(&self, d: Duration) -> Timestamp {
        let mut t = self.t.lock().unwrap_or_else(|p| p.into_inner());
        *t += d;
        *t
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self.t.lock().unwrap_or_else(|p| p.into_inner())
    }
}
