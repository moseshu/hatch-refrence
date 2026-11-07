//! Workspace-wide error type.

/// Every fallible operation in hatch returns this error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("policy denied: {0}")]
    Denied(String),
    #[error("secret material rejected: {0}")]
    SecretRejected(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("connector error: {0}")]
    Connector(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("sandbox: {0}")]
    Sandbox(String),
    /// Stopped on request (Ctrl-C, parent cancelled). Not a failure.
    #[error("cancelled: {0}")]
    Cancelled(String),
    /// A quota refused the write (artifact limits, ...). The message
    /// names the limit, the current usage and the cap.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Short machine-readable kind, used in audit logs and receipts.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Invalid(_) => "invalid",
            Error::NotFound(_) => "not_found",
            Error::Denied(_) => "denied",
            Error::SecretRejected(_) => "secret_rejected",
            Error::Storage(_) => "storage",
            Error::Model(_) => "model",
            Error::Sandbox(_) => "sandbox",
            Error::Cancelled(_) => "cancelled",
            Error::QuotaExceeded(_) => "quota_exceeded",
            Error::Connector(_) => "connector",
            Error::Config(_) => "config",
            Error::Io(_) => "io",
            Error::Json(_) => "json",
        }
    }

    /// Whether this failure is transient: a network wobble, a timeout, or
    /// a remote end that is not ready yet — worth a bounded retry (see
    /// [`crate::retry::retry_transient`]).
    ///
    /// Only [`Error::Io`] can be transient (see [`io_is_transient`]).
    /// Everything else is permanent: retrying a config error, a permission
    /// refusal, a missing file, a rejected secret, a quota, a bad model
    /// reply, or a sandbox setup failure cannot succeed, so it fails
    /// closed at once. Model 429/5xx classification stays with the
    /// router (`hatch-llm` owns model retry); this method never
    /// second-guesses it, so [`Error::Model`] is permanent here.
    /// [`Error::Cancelled`] is not a failure at all and is never transient.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Io(e) => io_is_transient(e),
            _ => false,
        }
    }

    /// Stopped on request, not a failure: never retried, never reported
    /// as one.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled(_))
    }
}

/// Whether one I/O error is a transient network/timeout cause.
///
/// The raw errno wins when present: `ErrorKind::NotFound` covers both a
/// missing binary (ENOENT — permanent config) and a failed DNS lookup,
/// so the kind alone cannot tell them apart. Unknown errnos default to
/// permanent (fail closed). Errno values are the Linux numbers; hatch
/// only spawns processes on Unix.
pub fn io_is_transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if let Some(code) = e.raw_os_error() {
        // EINTR | EAGAIN | EPIPE | ENETUNREACH | ECONNABORTED |
        // ECONNRESET | ETIMEDOUT | ECONNREFUSED | EHOSTDOWN | EHOSTUNREACH
        return matches!(code, 4 | 11 | 32 | 101 | 103 | 104 | 110 | 111 | 112 | 113);
    }
    matches!(
        e.kind(),
        ErrorKind::TimedOut
            | ErrorKind::Interrupted
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::WouldBlock
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_stable() {
        assert_eq!(Error::Denied("x".into()).kind(), "denied");
        assert_eq!(Error::NotFound("x".into()).to_string(), "not found: x");
    }

    #[test]
    fn non_io_variants_are_permanent() {
        for e in [
            Error::Invalid("x".into()),
            Error::NotFound("x".into()),
            Error::Denied("x".into()),
            Error::SecretRejected("x".into()),
            Error::Storage("x".into()),
            Error::Model("503".into()),
            Error::Connector("x".into()),
            Error::Config("x".into()),
            Error::Sandbox("x".into()),
            Error::QuotaExceeded("x".into()),
            Error::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
        ] {
            assert!(!e.is_transient(), "{e:?} must be permanent");
            assert!(!e.is_cancelled());
        }
    }

    #[test]
    fn cancelled_is_not_a_failure() {
        let e = Error::Cancelled("parent stopped".into());
        assert!(!e.is_transient());
        assert!(e.is_cancelled());
    }

    fn io(kind: std::io::ErrorKind) -> Error {
        Error::Io(std::io::Error::new(kind, "probe"))
    }

    #[test]
    fn io_kind_matrix() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::Interrupted,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::WouldBlock,
        ] {
            assert!(io(kind).is_transient(), "{kind:?} must be transient");
        }
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidInput,
            ErrorKind::AddrInUse,
            ErrorKind::AddrNotAvailable,
            ErrorKind::UnexpectedEof,
            ErrorKind::WriteZero,
            ErrorKind::Other,
        ] {
            assert!(!io(kind).is_transient(), "{kind:?} must be permanent");
        }
    }

    #[test]
    fn raw_errno_wins_over_kind() {
        // ENOENT (2): a missing binary is permanent config, even though
        // the kind is NotFound — the case a kind-only check gets wrong.
        assert!(!Error::Io(std::io::Error::from_raw_os_error(2)).is_transient());
        // EACCES (13) / EINVAL (22): permanent.
        assert!(!Error::Io(std::io::Error::from_raw_os_error(13)).is_transient());
        assert!(!Error::Io(std::io::Error::from_raw_os_error(22)).is_transient());
        // ECONNREFUSED (111) / ETIMEDOUT (110) / ECONNRESET (104) /
        // EPIPE (32) / EAGAIN (11) / EINTR (4): transient.
        for code in [4, 11, 32, 101, 103, 104, 110, 111, 112, 113] {
            assert!(
                Error::Io(std::io::Error::from_raw_os_error(code)).is_transient(),
                "errno {code} must be transient"
            );
        }
        // Unknown errno defaults to permanent (fail closed).
        assert!(!Error::Io(std::io::Error::from_raw_os_error(99)).is_transient());
        assert!(!Error::Io(std::io::Error::from_raw_os_error(28)).is_transient());
    }
}
