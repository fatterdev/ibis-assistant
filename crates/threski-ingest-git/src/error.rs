//! Typed errors for the git history ingest ( returned `fmt.Errorf` strings).

use threski_db::error::DbError;
use threski_git::GitError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("invalid repo path: {0}")]
    RepoPath(#[source] std::io::Error),
    #[error("failed to upsert repo: {0}")]
    RepoUpsert(#[source] DbError),
    #[error("failed to check existing commits: {0}")]
    ExistingCheck(#[source] DbError),
    #[error("git log hashes failed: {0}")]
    History(#[source] GitError),
    #[error("git ingest log failed: {0}")]
    GitLog(#[source] GitError),
    #[error("batch execution failed: {0}")]
    Batch(#[source] DbError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_reference_strings() {
        let e = IngestError::Batch(DbError::decode("boom", "SELECT 1"));
        assert!(e.to_string().starts_with("batch execution failed: "));
        assert!(matches!(
            IngestError::RepoPath(std::io::Error::other("nope")).to_string(),
            s if s == "invalid repo path: nope"
        ));
    }
}
