//! Typed errors for the dynamic ingest ( returned `fmt.Errorf` strings; the
//! `CredentialsRequired` shape is kept as a first-class variant because the
//! server lane branches on it to answer `credentials_required` statuses).

use std::time::Duration;

use thiserror::Error;

use threski_core::workspace::WorkspaceError;
use threski_git::GitError;

/// Generous ingest timeout applied to the [`crate::queue`] `ProcessJob` phase
///
pub const PROCESS_JOB_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Error)]
pub enum DynamicError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("failed to create dynamic repos directory: {0}")]
    DynamicReposDir(#[source] std::io::Error),
    #[error("{message}")]
    CredentialsRequired {
        provider: String,
        target: String,
        message: String,
    },
    #[error("git clone failed: {0}")]
    Clone(#[source] GitError),
    #[error("git fetch failed: {0}")]
    Fetch(#[source] GitError),
    #[error("git checkout {target} failed: {source}")]
    Checkout {
        target: String,
        #[source]
        source: GitError,
    },
    #[error("git checkout fallback to {branch} failed: {source}")]
    CheckoutFallback {
        branch: String,
        #[source]
        source: GitError,
    },
    #[error("live workspace {path} is not a directory: {source}")]
    LiveWorkspace {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git rev-parse HEAD failed in live workspace {path}: {source}")]
    LiveRevParse {
        path: String,
        #[source]
        source: GitError,
    },
    #[error("queue is full")]
    QueueFull,
    #[error("ingestion queue closed")]
    QueueClosed,
    #[error("ingestion timed out after {0:?}")]
    IngestTimeout(Duration),
    #[error("ingestion manager stopped")]
    ManagerStopped,
}

impl DynamicError {
    /// Builds the `CredentialsRequired` error with the provider detected from
    /// the remote URL (`detectProvider(originUrl) + the clone/fetch
    /// message).
    pub fn credentials_required(origin_url: &str, message: &str) -> Self {
        DynamicError::CredentialsRequired {
            provider: threski_git::detect_provider(origin_url),
            target: origin_url.to_string(),
            message: message.to_string(),
        }
    }

    /// Branch helper for the server lane: returns `(provider, target)` when
    /// this error is the credentials-required shape (
    /// `errors.As(&gitrepo.CredentialsRequiredError{})`).
    pub fn credentials_required_fields(&self) -> Option<(&str, &str)> {
        match self {
            DynamicError::CredentialsRequired {
                provider, target, ..
            } => Some((provider, target)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_required_shape() {
        let err = DynamicError::credentials_required(
            "https://github.com/x/y",
            "Git credentials required or authentication failed during clone",
        );
        assert_eq!(
            err.credentials_required_fields(),
            Some(("github", "https://github.com/x/y"))
        );
        assert_eq!(
            err.to_string(),
            "Git credentials required or authentication failed during clone"
        );

        let generic = DynamicError::credentials_required("https://git.example.com/x", "msg");
        assert_eq!(generic.credentials_required_fields().unwrap().0, "generic");
        assert!(DynamicError::QueueFull
            .credentials_required_fields()
            .is_none());
    }

    #[test]
    fn git_wrappers_match_reference_message_prefixes() {
        let inner = GitError::Command {
            status: "exit status 128".to_string(),
            output: "fatal: bad".to_string(),
        };
        assert_eq!(
            DynamicError::Clone(inner).to_string(),
            "git clone failed: git command failed: exit status 128, output: fatal: bad"
        );
    }
}
