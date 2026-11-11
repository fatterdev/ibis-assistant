//! Unified AI client orchestrator.

use std::sync::Arc;

use tracing::Instrument as _;

use crate::error::AiError;
use crate::runner::OllamaProcess;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// Holds an embedding provider, a reasoning provider and (optionally) the
/// spawned Ollama daemon handle. Embedding and reasoning may be the same
/// object (e.g. the gemini pool) — `stop` deduplicates via pointer equality,
/// mirroring the `interface{} != interface{}` check.
pub struct Client {
    embedding: Option<Arc<dyn AIProvider>>,
    reasoning: Option<Arc<dyn AIProvider>>,
    runner: Option<Arc<OllamaProcess>>,
}

impl Client {
    pub fn new(
        embedding: Option<Arc<dyn AIProvider>>,
        reasoning: Option<Arc<dyn AIProvider>>,
        runner: Option<Arc<OllamaProcess>>,
    ) -> Self {
        Client {
            embedding,
            reasoning,
            runner,
        }
    }

    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        match &self.embedding {
            Some(e) => {
                let span = crate::telemetry::active_embed_span(e.name(), 1);
                let t0 = std::time::Instant::now();
                let out = e.embed_text(text).instrument(span.clone()).await;
                match &out {
                    Ok(vector) => crate::telemetry::record_embed_success(&span, vector.len()),
                    Err(err) => crate::telemetry::record_error(&span, err),
                }
                tracing::info!(
                    "embed_text: provider={} dt={:?} ok={}",
                    e.name(),
                    t0.elapsed(),
                    out.is_ok()
                );
                out
            }
            None => {
                let err = AiError::NoEmbeddingProvider;
                let span = crate::telemetry::active_embed_span("none", 1);
                crate::telemetry::record_error(&span, &err);
                Err(err)
            }
        }
    }

    pub async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        match &self.embedding {
            Some(e) => {
                let span = crate::telemetry::active_embed_span(e.name(), texts.len());
                let out = e.batch_embed_text(texts).instrument(span.clone()).await;
                crate::telemetry::record_embed_result(&span, &out);
                out
            }
            None => {
                let err = AiError::NoEmbeddingProvider;
                let span = crate::telemetry::active_embed_span("none", texts.len());
                crate::telemetry::record_error(&span, &err);
                Err(err)
            }
        }
    }

    pub async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        match &self.reasoning {
            Some(r) => {
                let span = crate::telemetry::active_generate_span(r.name(), &config);
                let out = r
                    .generate_content(contents, config)
                    .instrument(span.clone())
                    .await;
                crate::telemetry::record_generate_result(&span, &out);
                out
            }
            None => {
                let err = AiError::NoReasoningProvider;
                let span = crate::telemetry::active_generate_span("none", &config);
                crate::telemetry::record_error(&span, &err);
                Err(err)
            }
        }
    }

    /// True when both providers are configured with a non-empty name
    ///
    pub fn is_functional(&self) -> bool {
        self.embedding
            .as_ref()
            .is_some_and(|e| !e.name().is_empty())
            && self
                .reasoning
                .as_ref()
                .is_some_and(|r| !r.name().is_empty())
    }

    /// True when only the embedding provider is configured — enough for code
    /// ingestion.
    pub fn is_embedding_functional(&self) -> bool {
        self.embedding
            .as_ref()
            .is_some_and(|e| !e.name().is_empty())
    }

    pub async fn stop(&self) {
        if let Some(runner) = &self.runner {
            if let Err(e) = runner.stop().await {
                tracing::warn!("Stopping local Ollama daemon failed: {e}");
            }
        }
        if let Some(e) = &self.embedding {
            e.stop().await;
        }
        let same = match (&self.embedding, &self.reasoning) {
            (Some(e), Some(r)) => Arc::ptr_eq(e, r),
            _ => false,
        };
        if !same {
            if let Some(r) = &self.reasoning {
                r.stop().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Part, UsageMetadata};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProvider {
        name: String,
        stops: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AIProvider for CountingProvider {
        fn name(&self) -> &str {
            &self.name
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, AiError> {
            Ok(vec![0.1])
        }
        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part { text: "hi".into() }],
                },
                finish_reason: "STOP".into(),
                usage_metadata: Some(UsageMetadata::default()),
            })
        }
        async fn stop(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn provider(name: &str, stops: &Arc<AtomicUsize>) -> Arc<dyn AIProvider> {
        Arc::new(CountingProvider {
            name: name.to_string(),
            stops: stops.clone(),
        })
    }

    #[tokio::test]
    async fn nil_providers_yield_sentinel_errors() {
        let client = Client::new(None, None, None);
        assert!(matches!(
            client.embed_text("x").await,
            Err(AiError::NoEmbeddingProvider)
        ));
        assert!(matches!(
            client
                .generate_content(&[], GenerationConfig::default())
                .await,
            Err(AiError::NoReasoningProvider)
        ));
        assert!(!client.is_functional());
        assert!(!client.is_embedding_functional());
    }

    #[tokio::test]
    async fn delegates_and_dedupes_stop_for_shared_provider() {
        let stops = Arc::new(AtomicUsize::new(0));
        let shared = provider("gemini", &stops);
        let client = Client::new(Some(shared.clone()), Some(shared), None);
        assert!(client.is_functional());
        assert!(client.is_embedding_functional());
        assert_eq!(client.embed_text("x").await.unwrap(), vec![0.1]);
        let cand = client
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap();
        assert_eq!(cand.content.parts[0].text, "hi");
        client.stop().await;
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_providers_stopped_individually() {
        let stops = Arc::new(AtomicUsize::new(0));
        let emb = provider("ollama", &stops);
        let rea = provider("gemini", &stops);
        let client = Client::new(Some(emb), Some(rea), None);
        client.stop().await;
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }
}
