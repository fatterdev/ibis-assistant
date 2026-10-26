use async_trait::async_trait;

use crate::error::AiError;
use crate::types::{Candidate, Content, GenerationConfig};

/// The unified AI provider boundary — fake-injection seam for the search,
/// ingest, and server lanes ( split `EmbeddingProvider`/`ReasoningProvider`
/// into one trait; providers implement the half they support and the other
/// half's default methods return [`AiError::Unsupported`], mirroring how
/// code only ever called the half its interface exposed).
#[async_trait]
pub trait AIProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn is_functional(&self) -> bool;

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let _ = text;
        Err(AiError::unsupported(self.name(), "embed_text"))
    }

    async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        let _ = texts;
        Err(AiError::unsupported(self.name(), "batch_embed_text"))
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let _ = (contents, config);
        Err(AiError::unsupported(self.name(), "generate_content"))
    }

    /// Shuts down background resources (gemini worker pool, spawned daemons).
    /// Idempotent; default is a no-op.
    async fn stop(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    #[async_trait]
    impl AIProvider for Nothing {
        fn name(&self) -> &str {
            "nothing"
        }
        async fn is_functional(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn defaults_report_unsupported() {
        let p = Nothing;
        assert_eq!(
            p.embed_text("x").await.unwrap_err().to_string(),
            "embed_text is not supported by provider 'nothing'"
        );
        assert_eq!(
            p.generate_content(&[], GenerationConfig::default())
                .await
                .unwrap_err()
                .to_string(),
            "generate_content is not supported by provider 'nothing'"
        );
        p.stop().await;
    }
}
