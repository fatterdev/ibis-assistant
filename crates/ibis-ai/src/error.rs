use thiserror::Error;

/// Error type for the whole `ibis-ai` crate. `AiError::ContextOnly` carries the
/// `ErrContextOnly sentinel semantics: callers pack
/// retrieval context instead of generating.
#[derive(Debug, Error, Clone)]
pub enum AiError {
    #[error("context only: no reasoning provider available")]
    ContextOnly,
    #[error("no embedding provider configured")]
    NoEmbeddingProvider,
    #[error("no reasoning provider configured")]
    NoReasoningProvider,
    #[error("AI client not initialized or no keys available")]
    NotInitialized,
    #[error("{provider} not configured")]
    NotConfigured { provider: String },
    #[error("{operation} is not supported by provider '{provider}'")]
    Unsupported { provider: String, operation: String },
    #[error("cloud unavailable")]
    CloudUnavailable,
    #[error("local unavailable")]
    LocalUnavailable,
    #[error("no cloud providers configured")]
    NoCloudProviders,
    #[error("cloud pool exhausted")]
    CloudPoolExhausted,
    #[error("context deadline exceeded")]
    DeadlineExceeded,
    #[error("context canceled")]
    Cancelled,
    #[error("{first}\n{second}")]
    Joined {
        first: Box<AiError>,
        second: Box<AiError>,
    },
    #[error("http transport: {0}")]
    Transport(String),
    #[error("{context}: {source}")]
    Wrapped {
        context: String,
        source: Box<AiError>,
    },
    #[error("{0}")]
    Other(String),
}

impl AiError {
    pub fn other(message: impl Into<String>) -> Self {
        AiError::Other(message.into())
    }

    pub fn unsupported(provider: &str, operation: &str) -> Self {
        AiError::Unsupported {
            provider: provider.to_string(),
            operation: operation.to_string(),
        }
    }

    pub fn wrapped(context: impl Into<String>, source: AiError) -> Self {
        AiError::Wrapped {
            context: context.into(),
            source: Box::new(source),
        }
    }

    pub fn is_context_only(&self) -> bool {
        matches!(self, AiError::ContextOnly)
    }

    /// Stable, bounded identifier for telemetry (`ai.error.kind`). Never the
    /// message: provider errors can echo the prompt.
    pub fn kind(&self) -> &'static str {
        match self {
            AiError::ContextOnly => "context_only",
            AiError::NoEmbeddingProvider => "no_embedding_provider",
            AiError::NoReasoningProvider => "no_reasoning_provider",
            AiError::NotInitialized => "not_initialized",
            AiError::NotConfigured { .. } => "not_configured",
            AiError::Unsupported { .. } => "unsupported",
            AiError::CloudUnavailable => "cloud_unavailable",
            AiError::LocalUnavailable => "local_unavailable",
            AiError::NoCloudProviders => "no_cloud_providers",
            AiError::CloudPoolExhausted => "cloud_pool_exhausted",
            AiError::DeadlineExceeded => "deadline_exceeded",
            AiError::Cancelled => "cancelled",
            AiError::Joined { .. } => "joined",
            AiError::Transport(_) => "transport",
            AiError::Wrapped { .. } => "wrapped",
            AiError::Other(_) => "other",
        }
    }
}

impl From<reqwest::Error> for AiError {
    fn from(source: reqwest::Error) -> Self {
        AiError::Transport(source.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_only_display_matches_sentinel() {
        let err = AiError::ContextOnly;
        assert_eq!(
            err.to_string(),
            "context only: no reasoning provider available"
        );
        assert!(err.is_context_only());
    }

    #[test]
    fn joined_renders_like_errors_join() {
        let err = AiError::Joined {
            first: Box::new(AiError::other("a")),
            second: Box::new(AiError::other("b")),
        };
        assert_eq!(err.to_string(), "a\nb");
    }
}
