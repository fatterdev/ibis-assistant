//! Typed errors for the database layer.

use std::time::Duration;

/// Query/codec errors surfaced at the `Executor` / codec boundary.
///
/// Every decode failure carries the offending SQL statement excerpt so the
/// caller can pinpoint the mapping mismatch.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database client not initialized")]
    NotInitialized,
    #[error("query failed [{duration_ms}ms]: {message}")]
    Query { message: String, duration_ms: u64 },
    #[error("query timed out after {0:?}")]
    Timeout(Duration),
    #[error("failed to decode query result: {message}")]
    Decode { message: String },
    #[error("expected a single result, got {count} statements")]
    NotSingleResult { count: usize },
    #[error(transparent)]
    Surreal(#[from] surrealdb::Error),
}

impl DbError {
    /// Wraps a driver failure with the statement excerpt and duration.
    pub fn query(err: impl std::fmt::Display, sql: &str, duration: Duration) -> Self {
        let excerpt = sql_excerpt(sql);
        DbError::Query {
            message: format!("{err} [sql: {excerpt}]"),
            duration_ms: duration.as_millis() as u64,
        }
    }

    /// Builds a decode failure carrying the SQL statement excerpt.
    pub fn decode(err: impl std::fmt::Display, sql: &str) -> Self {
        let excerpt = sql_excerpt(sql);
        DbError::Decode {
            message: format!("{err} [sql: {excerpt}]"),
        }
    }
}

/// First ~120 chars of a SQL statement, whitespace-collapsed, for error context.
fn sql_excerpt(sql: &str) -> String {
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut = 120.min(collapsed.len());
    while !collapsed.is_char_boundary(cut) {
        cut -= 1;
    }
    collapsed[..cut].to_string()
}

/// Binary install/upgrade and embedded-process errors.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("archive error: {0}")]
    Archive(String),
    #[error("no compatible SurrealDB asset found for {os}/{arch} in release {tag}")]
    NoAsset {
        os: String,
        arch: String,
        tag: String,
    },
    #[error("could not parse surreal version output: {0}")]
    VersionOutput(String),
    #[error("embedded database did not open port {port} within {timeout:?}")]
    NotReady { port: u16, timeout: Duration },
    #[error("failed to start database process: {0}")]
    Spawn(String),
    #[error("failed to replace existing binary (locked?): {0}")]
    ReplaceBinary(String),
    #[error("{0}")]
    Other(String),
}
