//! Typed errors for the search crate. Messages mirror the `fmt.Errorf
//! strings the tests and MCP surface pin (e.g. "database not connected",
//! "confidence must be between 0.0 and 1.0").

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("database not connected")]
    DatabaseNotConnected,
    #[error("embedding failed: {0}")]
    Embedding(String),
    #[error("{context}: {source}")]
    Context {
        context: String,
        source: Box<SearchError>,
    },
    #[error(transparent)]
    Git(#[from] ibis_git::GitError),
    #[error(transparent)]
    Db(#[from] ibis_db::DbError),
    #[error(transparent)]
    Ai(#[from] ibis_ai::AiError),
    #[error(transparent)]
    Claim(#[from] ibis_core::claims::ClaimError),
    #[error("{0}")]
    Other(String),
}

impl SearchError {
    pub fn other(message: impl Into<String>) -> Self {
        SearchError::Other(message.into())
    }

    pub fn context(context: impl Into<String>, source: SearchError) -> Self {
        SearchError::Context {
            context: context.into(),
            source: Box::new(source),
        }
    }

    /// Scenario: pattern: `fmt.Errorf("...: %w", err)`.
    pub fn wrap(context: impl Into<String>, source: impl Into<SearchError>) -> Self {
        Self::context(context, source.into())
    }
}

pub type Result<T> = std::result::Result<T, SearchError>;
