//! P1 D7 — the task watchdog: liveness probing over the ledger.
//!
//! [`TaskStore::reconcile`] is the convergence logic (D6); this module is
//! the live caller. The daemon runs [`probe_once`] on an interval
//! (default [`WatchdogConfig::default`]); a turn start also reconciles
//! (see `Agent::run_turn`). The two call sites differ on purpose:
//!
//! * startup / turn-start reconcile uses
//!   [`DEFAULT_STALE_AFTER_SECS`] (1800s) — a generous safety net for
//!   rows written before D7 or by producers that do not heartbeat;
//! * the live watchdog uses [`WatchdogConfig::stale_after_secs`] (300s)
//!   — 5x the heartbeat interval, so a dead owner is found within ~5
//!   minutes while a live task (heartbeat at most ~60s old) can never
//!   trip it, even with several missed beats.
//!
//! A future producer that creates `running` rows without heartbeating
//! will be converged to `waiting` after 5 minutes by design: a running
//! task that does not prove aliveness is presumed dead. Producers must
//! heartbeat (see [`TaskStore::heartbeat`]).
//!
//! [`probe_once`] never fails: a DB error returns an empty report. The
//! watchdog must never fail a daemon or a turn — convergence is
//! best-effort observability, not the work itself.

use crate::store::{ReconcileReport, TaskStore};
use hatch_store::Db;

/// The watchdog's cadence. Both values are configurable
/// (`[tasks] watchdog_probe_secs` / `watchdog_stale_after_secs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// Seconds between probes.
    pub probe_interval_secs: u64,
    /// A `running` task whose heartbeat is older than this is presumed dead.
    pub stale_after_secs: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        WatchdogConfig { probe_interval_secs: 60, stale_after_secs: 300 }
    }
}

/// One liveness probe: converge `running` tasks whose heartbeat is
/// stale (revived to `waiting`, or `failed` when retries are out).
/// Never fails — a DB error yields an empty report, so the caller
/// (daemon thread, turn start) never has to handle it.
pub fn probe_once(db: &Db, stale_after_secs: u64) -> ReconcileReport {
    TaskStore::new(db)
        .reconcile(&hatch_core::time::now(), stale_after_secs)
        .unwrap_or_default()
}
