//! `hatch-scheduler` — run work on a schedule, unattended but accountable.
//!
//! * [`schedule`] — `daily` / `weekly` / `every` / `once` / `cron`, DST-correct.
//! * [`store`] — job definitions, run records and dedup keys in three tables.
//! * [`engine`] — which jobs fire on a tick: timeouts, retries with back-off,
//!   auto-disable, missed-firing accounting and at-most-once catch-up.
//! * [`daemon`] — the long-running loop with a single-instance lock, stop
//!   file and graceful shutdown on SIGTERM / SIGINT.

pub mod clock;
pub mod cron;
pub mod daemon;
pub mod engine;
pub mod job;
pub mod localtime;
pub mod lock;
pub mod schedule;
pub mod signals;
pub mod store;

pub use clock::{Clock, FakeClock, SystemClock};
pub use daemon::{Daemon, DaemonConfig, DaemonExit, DaemonPaths, DaemonStatus};
pub use engine::{Engine, FnHandler, JobContext, JobHandler, JobOutcome, Notice, RunResult, TickReport};
pub use job::{CatchUp, Delivery, Job, JobRun, NewJob, RunStatus, Trigger};
pub use schedule::Schedule;
pub use store::JobStore;
