//! Typed errors for the code-ingest crate.

use thiserror::Error;

/// Error surface of `ibis-ingest-code` (orchestration, chunking, rule
/// synthesis, installer).
#[derive(Debug, Error)]
pub enum CodeIngestError {
    #[error("database error: {0}")]
    Db(#[from] ibis_db::DbError),
    #[error("git error: {0}")]
    Git(#[from] ibis_git::GitError),
    #[error("ai error: {0}")]
    Ai(#[from] ibis_ai::AiError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("walk error: {0}")]
    Walk(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

impl CodeIngestError {
    pub fn other(message: impl Into<String>) -> Self {
        CodeIngestError::Other(message.into())
    }
}

/// Failure of [`crate::rule_synth::persist_validated_rule`] — returns a
/// non-nil `*RulePersistResult` alongside the error (the result carries the
/// quarantine path of the rejected YAML), so the Rust error carries the
/// partial result too. `None` mirrors the nil result for the early
/// `empty yaml` / `yaml too large` rejections.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct RulePersistError {
    /// Partial outcome when one exists (quarantine path populated when the
    /// YAML was saved aside; `validated: false`). Boxed so the error value
    /// stays small.
    pub result: Option<Box<crate::rule_synth::RulePersistResult>>,
    #[source]
    pub source: CodeIngestError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_forms() {
        assert_eq!(
            CodeIngestError::other("sgconfig root not found").to_string(),
            "sgconfig root not found"
        );
        assert_eq!(
            CodeIngestError::Io(std::io::Error::other("boom")).to_string(),
            "io error: boom"
        );
    }
}
