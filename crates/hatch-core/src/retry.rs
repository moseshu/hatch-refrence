//! Bounded retry for setup operations: spawning a sub-agent child or a
//! sandbox helper. A transient failure (see [`crate::Error::is_transient`])
//! is retried; a permanent one fails closed at once. The helper never
//! decides what the caller does after the attempts run out — it only
//! reports the last error and how many attempts ran, so the caller keeps
//! its own fallback (the in-process path for sub-agents, the honest error
//! text for sandboxed commands).
//!
//! Only setup steps are retried here: nothing has executed yet, so a
//! retry cannot repeat a side effect. Model calls have their own retry
//! in the router; this helper must not be used for them.

use std::time::Duration;

/// Maximum attempts for one setup operation, including the first try.
pub const MAX_ATTEMPTS: usize = 3;

/// Fixed pause between attempts in production. Tests pass
/// [`Duration::ZERO`] so no test ever sleeps for real.
pub const RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Run `attempt` until it succeeds or fails permanently, at most
/// [`MAX_ATTEMPTS`] attempts in total for transient failures. Returns the
/// last outcome and the attempt count: 1 means the failure was permanent
/// (or the first try succeeded), [`MAX_ATTEMPTS`] means a transient
/// failure was retried until the budget ran out.
pub fn retry_transient<T, E, F>(
    mut attempt: F,
    is_transient: impl Fn(&E) -> bool,
    backoff: Duration,
) -> (std::result::Result<T, E>, usize)
where
    F: FnMut() -> std::result::Result<T, E>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        match attempt() {
            Ok(v) => return (Ok(v), attempts),
            Err(e) => {
                if is_transient(&e) && attempts < MAX_ATTEMPTS {
                    if !backoff.is_zero() {
                        std::thread::sleep(backoff);
                    }
                    continue;
                }
                return (Err(e), attempts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn timed_out() -> Error {
        Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "probe"))
    }

    fn is_transient(e: &Error) -> bool {
        e.is_transient()
    }

    #[test]
    fn success_first_try_is_one_attempt() {
        let (r, n) = retry_transient(|| Ok::<_, Error>(7), is_transient, Duration::ZERO);
        assert_eq!((r.unwrap(), n), (7, 1));
    }

    #[test]
    fn transient_failure_retried_up_to_three_then_reported() {
        let mut calls = 0;
        let (r, n) = retry_transient(
            || {
                calls += 1;
                Err::<(), _>(timed_out())
            },
            is_transient,
            Duration::ZERO,
        );
        assert!(r.is_err());
        assert_eq!((calls, n), (3, 3));
    }

    #[test]
    fn transient_then_success_stops_early() {
        let mut calls = 0;
        let (r, n) = retry_transient(
            || {
                calls += 1;
                if calls < 2 {
                    Err::<(), _>(timed_out())
                } else {
                    Ok(())
                }
            },
            is_transient,
            Duration::ZERO,
        );
        assert!(r.is_ok());
        assert_eq!((calls, n), (2, 2));
    }

    #[test]
    fn permanent_failure_is_one_attempt() {
        let mut calls = 0;
        let (r, n) = retry_transient(
            || {
                calls += 1;
                Err::<(), _>(Error::Config("bad mode".into()))
            },
            is_transient,
            Duration::ZERO,
        );
        assert!(r.is_err());
        assert_eq!((calls, n), (1, 1));
    }

    #[test]
    fn cancelled_is_never_retried() {
        let mut calls = 0;
        let (r, n) = retry_transient(
            || {
                calls += 1;
                Err::<(), _>(Error::Cancelled("stop".into()))
            },
            is_transient,
            Duration::ZERO,
        );
        assert!(r.is_err());
        assert_eq!((calls, n), (1, 1));
    }
}
