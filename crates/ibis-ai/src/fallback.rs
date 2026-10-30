//! Ordered reasoning fallback chain (F0.5): `sampling → lm_bridge →
//! ollama`. Unlike the hybrid [`crate::router::ReasoningRouter`] (local vs
//! cloud with route hints), the chain is a fixed priority list — the first
//! functional step that answers wins, failures fold into one joined error.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// Tries its steps in order; non-functional steps are skipped, failing ones
/// fold into the joined error (`AiError::Joined`). With `context_only` the
/// all-failed outcome degrades to the `AiError::ContextOnly` sentinel.
pub struct FallbackChain {
    steps: Vec<Arc<dyn AIProvider>>,
    context_only: bool,
}

impl FallbackChain {
    pub fn new(steps: Vec<Arc<dyn AIProvider>>, context_only: bool) -> Self {
        FallbackChain {
            steps,
            context_only,
        }
    }

    /// Names of the steps, in order (`status`/diagnostics reporting).
    pub fn step_names(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.name().to_string()).collect()
    }

    fn fold(errors: Vec<AiError>) -> AiError {
        let mut iter = errors.into_iter().rev();
        let Some(last) = iter.next() else {
            return AiError::other("no reasoning step attempted");
        };
        iter.fold(last, |joined, err| AiError::Joined {
            first: Box::new(err),
            second: Box::new(joined),
        })
    }
}

#[async_trait]
impl AIProvider for FallbackChain {
    fn name(&self) -> &str {
        "fallback_chain"
    }

    async fn is_functional(&self) -> bool {
        for step in &self.steps {
            if step.is_functional().await {
                return true;
            }
        }
        false
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let mut errors: Vec<AiError> = Vec::new();
        for step in &self.steps {
            if !step.is_functional().await {
                errors.push(AiError::wrapped(
                    format!("{} unavailable", step.name()),
                    AiError::other("not connected"),
                ));
                continue;
            }
            match step.generate_content(contents, config.clone()).await {
                Ok(candidate) => return Ok(candidate),
                Err(err) => errors.push(AiError::wrapped(step.name(), err)),
            }
        }
        let err = Self::fold(errors);
        if self.context_only {
            return Err(AiError::ContextOnly);
        }
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Part;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Stub {
        name: String,
        functional: bool,
        err: Option<String>,
        text: String,
        generates: Arc<AtomicUsize>,
    }

    impl Stub {
        fn ok(name: &str, text: &str, generates: Arc<AtomicUsize>) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: None,
                text: text.to_string(),
                generates,
            }
        }
        fn failing(name: &str, err: &str, generates: Arc<AtomicUsize>) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: Some(err.to_string()),
                text: String::new(),
                generates,
            }
        }
        fn nonfunctional(name: &str, generates: Arc<AtomicUsize>) -> Self {
            Stub {
                name: name.to_string(),
                functional: false,
                err: None,
                text: String::new(),
                generates,
            }
        }
    }

    #[async_trait]
    impl AIProvider for Stub {
        fn name(&self) -> &str {
            &self.name
        }
        async fn is_functional(&self) -> bool {
            self.functional
        }
        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.generates.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = &self.err {
                return Err(AiError::other(err.clone()));
            }
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part { text: self.text.clone() }],
                },
                finish_reason: String::new(),
                usage_metadata: None,
            })
        }
    }

    fn text_of(result: Result<Candidate, AiError>) -> String {
        result.unwrap().content.parts[0].text.clone()
    }

    #[tokio::test]
    async fn first_functional_step_wins() {
        let sampling = Arc::new(AtomicUsize::new(0));
        let bridge = Arc::new(AtomicUsize::new(0));
        let ollama = Arc::new(AtomicUsize::new(0));
        let chain = FallbackChain::new(
            vec![
                Arc::new(Stub::ok("mcp_sampling", "sampled", sampling.clone())),
                Arc::new(Stub::ok("cursor", "bridge", bridge.clone())),
                Arc::new(Stub::ok("ollama_chat", "local", ollama.clone())),
            ],
            false,
        );
        assert_eq!(chain.name(), "fallback_chain");
        assert_eq!(
            chain.step_names(),
            vec!["mcp_sampling", "cursor", "ollama_chat"]
        );
        assert!(chain.is_functional().await);
        assert_eq!(
            text_of(
                chain
                    .generate_content(&[], GenerationConfig::default())
                    .await
            ),
            "sampled"
        );
        assert_eq!(sampling.load(Ordering::SeqCst), 1);
        assert_eq!(bridge.load(Ordering::SeqCst), 0);
        assert_eq!(ollama.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failure_falls_through_the_whole_chain() {
        let sampling = Arc::new(AtomicUsize::new(0));
        let bridge = Arc::new(AtomicUsize::new(0));
        let ollama = Arc::new(AtomicUsize::new(0));
        let chain = FallbackChain::new(
            vec![
                Arc::new(Stub::failing("mcp_sampling", "declined", sampling.clone())),
                Arc::new(Stub::nonfunctional("cursor", bridge.clone())),
                Arc::new(Stub::ok("ollama_chat", "local", ollama.clone())),
            ],
            false,
        );
        assert_eq!(
            text_of(
                chain
                    .generate_content(&[], GenerationConfig::default())
                    .await
            ),
            "local"
        );
        assert_eq!(sampling.load(Ordering::SeqCst), 1);
        assert_eq!(bridge.load(Ordering::SeqCst), 0, "skipped while down");
        assert_eq!(ollama.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_failed_joins_errors_in_order() {
        let chain = FallbackChain::new(
            vec![
                Arc::new(Stub::failing("mcp_sampling", "declined", Default::default())),
                Arc::new(Stub::nonfunctional("cursor", Default::default())),
                Arc::new(Stub::failing("ollama_chat", "daemon down", Default::default())),
            ],
            false,
        );
        let err = chain
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "mcp_sampling: declined\ncursor unavailable: not connected\nollama_chat: daemon down"
        );
        assert!(!err.is_context_only());
    }

    #[tokio::test]
    async fn context_only_flag_degrades_the_all_failed_outcome() {
        let chain = FallbackChain::new(
            vec![Arc::new(Stub::failing(
                "mcp_sampling",
                "declined",
                Default::default(),
            ))],
            true,
        );
        let err = chain
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert!(err.is_context_only());
        assert_eq!(
            err.to_string(),
            "context only: no reasoning provider available"
        );
    }

    #[tokio::test]
    async fn empty_chain_reports_not_attempted() {
        let chain = FallbackChain::new(vec![], false);
        assert!(!chain.is_functional().await);
        let err = chain
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no reasoning step attempted");
    }

    #[tokio::test]
    async fn nonfunctional_steps_make_is_functional_false() {
        let chain = FallbackChain::new(
            vec![
                Arc::new(Stub::nonfunctional("mcp_sampling", Default::default())),
                Arc::new(Stub::nonfunctional("ollama_chat", Default::default())),
            ],
            true,
        );
        assert!(!chain.is_functional().await);
    }
}