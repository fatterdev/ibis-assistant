use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git command failed: {status}, output: {output}")]
    Command { status: String, output: String },
    #[error("failed to spawn git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<GitError>,
    },
    #[error(
        "shallow_history: {repo_path} is a shallow clone; ancestor sets are unverifiable — deepen the clone (git fetch --unshallow) before temporal queries"
    )]
    ShallowHistory { repo_path: String },
    #[error("{message}")]
    CredentialsRequired {
        provider: String,
        target: String,
        message: String,
    },
    #[error("{0}")]
    Other(String),
}

impl GitError {
    pub fn context(context: impl Into<String>, source: GitError) -> Self {
        GitError::Context {
            context: context.into(),
            source: Box::new(source),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        GitError::Other(message.into())
    }

    pub fn credentials_required(
        provider: impl Into<String>,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        GitError::CredentialsRequired {
            provider: provider.into(),
            target: target.into(),
            message: message.into(),
        }
    }

    pub fn command_output(&self) -> Option<&str> {
        match self {
            GitError::Command { output, .. } => Some(output),
            GitError::Context { source, .. } => source.command_output(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_display_matches_reference_format() {
        let err = GitError::Command {
            status: "exit status 128".to_string(),
            output: "fatal: bad".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "git command failed: exit status 128, output: fatal: bad"
        );
    }

    #[test]
    fn context_preserves_source_output() {
        let inner = GitError::Command {
            status: "exit status 1".to_string(),
            output: "boom".to_string(),
        };
        let err = GitError::context("resolve ref main", inner);
        assert_eq!(err.command_output(), Some("boom"));
        assert!(err.to_string().starts_with("resolve ref main: "));
    }

    #[test]
    fn shallow_history_message() {
        let err = GitError::ShallowHistory {
            repo_path: "/repo".to_string(),
        };
        assert!(err.to_string().starts_with("shallow_history: /repo"));
        assert!(err.to_string().contains("git fetch --unshallow"));
    }

    #[test]
    fn credentials_required_display_is_message() {
        let err = GitError::credentials_required("github", "github.com", "credentials needed");
        assert_eq!(err.to_string(), "credentials needed");
    }
}
