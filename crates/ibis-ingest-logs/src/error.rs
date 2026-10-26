//! Typed errors for the log-ingest pipeline.

use thiserror::Error;

/// Error type shared by the analyzer, tailer, watcher, polling, notifier and
/// enrichment modules.
#[derive(Debug, Error)]
pub enum LogsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] ibis_db::DbError),
    #[error("ai error: {0}")]
    Ai(#[from] ibis_ai::AiError),
    #[error("{0}")]
    Other(String),
    #[error("smtp: {0}")]
    Smtp(String),
    #[error("ftp: {0}")]
    Ftp(String),
    #[error("sftp: {0}")]
    Sftp(String),
    #[error("polling: {0}")]
    Polling(String),
    #[error("rule validation failed: {0}")]
    RuleValidation(String),
}

impl LogsError {
    pub fn other(message: impl Into<String>) -> Self {
        LogsError::Other(message.into())
    }

    pub fn smtp(message: impl Into<String>) -> Self {
        LogsError::Smtp(message.into())
    }

    pub fn ftp(message: impl Into<String>) -> Self {
        LogsError::Ftp(message.into())
    }

    pub fn sftp(message: impl Into<String>) -> Self {
        LogsError::Sftp(message.into())
    }

    pub fn polling(message: impl Into<String>) -> Self {
        LogsError::Polling(message.into())
    }
}

/// Result alias for the crate.
pub type Result<T> = std::result::Result<T, LogsError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_build_typed_variants() {
        assert_eq!(LogsError::other("boom").to_string(), "boom");
        assert_eq!(LogsError::smtp("no server").to_string(), "smtp: no server");
        assert_eq!(LogsError::ftp("530").to_string(), "ftp: 530");
        assert_eq!(LogsError::sftp("closed").to_string(), "sftp: closed");
        assert_eq!(
            LogsError::polling("bad config").to_string(),
            "polling: bad config"
        );
    }
}
