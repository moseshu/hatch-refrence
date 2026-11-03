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
            Error::Connector(_) => "connector",
            Error::Config(_) => "config",
            Error::Io(_) => "io",
            Error::Json(_) => "json",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_stable() {
        assert_eq!(Error::Denied("x".into()).kind(), "denied");
        assert_eq!(Error::NotFound("x".into()).to_string(), "not found: x");
    }
}
