//! Hybrid local/cloud reasoning router.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub use_cloud_when_no_gpu: bool,
    pub cpu_only: bool,
    /// Deadline for the local attempt; `Duration::ZERO` at construction falls
    /// back to the original default of 120s.
    pub local_timeout: Duration,
    /// When both providers fail, return `AiError::ContextOnly` instead.
    pub context_only: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            use_cloud_when_no_gpu: false,
            cpu_only: false,
            local_timeout: Duration::ZERO,
            context_only: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Local,
    Cloud,
}

/// Routes generation between a local provider and a cloud pool:
/// - hint `quality` → cloud first;
/// - hint `auto` → cloud only when `use_cloud_when_no_gpu` and the host is CPU-only;
/// - `local_only` → local, no fallback;
/// - `bulk`/`bulk_local` → local then cloud;
/// - otherwise local first, cloud as fallback.
pub struct ReasoningRouter {
    local: Option<Box<dyn AIProvider>>,
    cloud: Option<Box<dyn AIProvider>>,
    cfg: RouterConfig,
}

impl ReasoningRouter {
    pub fn new(
        local: Option<Box<dyn AIProvider>>,
        cloud: Option<Box<dyn AIProvider>>,
        cfg: RouterConfig,
    ) -> Self {
        let mut cfg = cfg;
        if cfg.local_timeout.is_zero() {
            cfg.local_timeout = Duration::from_secs(120);
        }
        ReasoningRouter { local, cloud, cfg }
    }

    async fn try_step(
        &self,
        step: Step,
        contents: &[Content],
        cfg: &GenerationConfig,
    ) -> Result<Candidate, AiError> {
        match step {
            Step::Cloud => match &self.cloud {
                Some(c) if c.is_functional().await => {
                    c.generate_content(contents, cfg.clone()).await
                }
                _ => Err(AiError::CloudUnavailable),
            },
            Step::Local => match &self.local {
                Some(l) if l.is_functional().await => {
                    if self.cfg.local_timeout > Duration::ZERO {
                        match tokio::time::timeout(
                            self.cfg.local_timeout,
                            l.generate_content(contents, cfg.clone()),
                        )
                        .await
                        {
                            Ok(res) => res,
                            Err(_) => Err(AiError::DeadlineExceeded),
                        }
                    } else {
                        l.generate_content(contents, cfg.clone()).await
                    }
                }
                _ => Err(AiError::LocalUnavailable),
            },
        }
    }

    fn context_only_or(&self, err: AiError) -> AiError {
        if self.cfg.context_only {
            AiError::ContextOnly
        } else {
            err
        }
    }
}

#[async_trait]
impl AIProvider for ReasoningRouter {
    fn name(&self) -> &str {
        "hybrid"
    }

    async fn is_functional(&self) -> bool {
        match &self.local {
            Some(l) if l.is_functional().await => true,
            _ => match &self.cloud {
                Some(c) => c.is_functional().await,
                None => false,
            },
        }
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let mut hint = cfg.route_hint.trim().to_lowercase();
        if hint.is_empty() {
            hint = "auto".to_string();
        }
        let prefer_cloud = hint == "quality"
            || (hint == "auto" && self.cfg.use_cloud_when_no_gpu && self.cfg.cpu_only);

        let cloud_ready = matches!(&self.cloud, Some(c) if c.is_functional().await);
        let (first, second): (Step, Option<Step>) = if prefer_cloud && cloud_ready {
            (Step::Cloud, Some(Step::Local))
        } else if hint == "local_only" {
            (Step::Local, None)
        } else if hint == "bulk" || hint == "bulk_local" {
            (Step::Local, Some(Step::Cloud))
        } else if prefer_cloud {
            (Step::Cloud, Some(Step::Local))
        } else {
            (Step::Local, Some(Step::Cloud))
        };

        let first_err = match self.try_step(first, contents, &cfg).await {
            Ok(cand) => return Ok(cand),
            Err(err) => err,
        };

        let Some(second) = second else {
            return Err(self.context_only_or(first_err));
        };

        match self.try_step(second, contents, &cfg).await {
            Ok(cand) => Ok(cand),
            Err(second_err) => Err(self.context_only_or(AiError::Joined {
                first: Box::new(first_err),
                second: Box::new(second_err),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Part;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Stub {
        name: String,
        functional: bool,
        err: Option<AiError>,
        text: String,
        delay: Option<Duration>,
        calls: Arc<AtomicUsize>,
    }

    impl Stub {
        fn ok(name: &str, text: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: None,
                text: text.to_string(),
                delay: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn failing(name: &str, err: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: Some(AiError::other(err)),
                text: String::new(),
                delay: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn nonfunctional(name: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: false,
                err: None,
                text: String::new(),
                delay: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = Some(d);
            self
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
            _cfg: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(Candidate {
                    content: Content {
                        role: "model".into(),
                        parts: vec![Part {
                            text: self.text.clone(),
                        }],
                    },
                    finish_reason: String::new(),
                    usage_metadata: None,
                }),
            }
        }
    }

    fn boxed(s: Stub) -> Box<dyn AIProvider> {
        Box::new(s)
    }

    async fn route_text(r: &ReasoningRouter, hint: &str) -> Result<String, AiError> {
        let cand = r
            .generate_content(
                &[],
                GenerationConfig {
                    route_hint: hint.into(),
                    ..Default::default()
                },
            )
            .await?;
        Ok(cand.content.parts[0].text.clone())
    }

    #[tokio::test]
    async fn prefers_cloud_on_quality_hint_over_cpu() {
        let local = Stub::ok("ollama", "local");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                use_cloud_when_no_gpu: true,
                cpu_only: true,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "quality").await.unwrap(), "cloud");
    }

    #[tokio::test]
    async fn auto_hint_routes_to_cloud_only_on_cpu_when_enabled() {
        let local = Stub::ok("ollama", "local");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                use_cloud_when_no_gpu: true,
                cpu_only: true,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "auto").await.unwrap(), "cloud");

        // GPU host: stays local.
        let local = Stub::ok("ollama", "local");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                use_cloud_when_no_gpu: true,
                cpu_only: false,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "auto").await.unwrap(), "local");

        // use_cloud_when_no_gpu disabled: stays local.
        let local = Stub::ok("ollama", "local");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                use_cloud_when_no_gpu: false,
                cpu_only: true,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "auto").await.unwrap(), "local");
    }

    #[tokio::test]
    async fn context_only_sentinel_when_both_missing() {
        let r = ReasoningRouter::new(
            None,
            None,
            RouterConfig {
                context_only: true,
                ..Default::default()
            },
        );
        let err = r
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert!(err.is_context_only());
        assert_eq!(
            err.to_string(),
            "context only: no reasoning provider available"
        );

        // Without the fallback flag the raw error surfaces.
        let r = ReasoningRouter::new(None, None, RouterConfig::default());
        let err = r
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert!(!err.is_context_only());
        assert_eq!(err.to_string(), "local unavailable\ncloud unavailable");
    }

    #[tokio::test]
    async fn local_only_never_leaks_to_cloud() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let mut local = Stub::failing("ollama", "offline");
        local.calls = local_calls.clone();
        let mut cloud = Stub::ok("gemini", "leaked-to-cloud");
        cloud.calls = cloud_calls.clone();
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig::default(),
        );
        let err = route_text(&r, "local_only").await.unwrap_err();
        assert!(!err.to_string().contains("leaked-to-cloud"));
        assert_eq!(err.to_string(), "offline");
        assert_eq!(local_calls.load(Ordering::SeqCst), 1);
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bulk_hint_tries_local_then_cloud() {
        let local = Stub::failing("ollama", "offline");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig::default(),
        );
        assert_eq!(route_text(&r, "bulk").await.unwrap(), "cloud");
        assert_eq!(route_text(&r, "bulk_local").await.unwrap(), "cloud");
    }

    #[tokio::test]
    async fn default_hint_is_local_first() {
        let local = Stub::ok("ollama", "local");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig::default(),
        );
        assert_eq!(route_text(&r, "").await.unwrap(), "local");
    }

    #[tokio::test]
    async fn local_deadline_falls_back_to_cloud() {
        let local = Stub::ok("ollama", "local").with_delay(Duration::from_millis(500));
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                local_timeout: Duration::from_millis(50),
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "").await.unwrap(), "cloud");
    }

    #[tokio::test]
    async fn both_failure_and_context_only() {
        let local = Stub::failing("ollama", "offline");
        let cloud = Stub::failing("gemini", "quota");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig {
                context_only: true,
                ..Default::default()
            },
        );
        let err = r
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert!(err.is_context_only());
    }

    #[tokio::test]
    async fn nonfunctional_local_still_falls_back_to_cloud() {
        let local = Stub::nonfunctional("ollama");
        let cloud = Stub::ok("gemini", "cloud");
        let r = ReasoningRouter::new(
            Some(boxed(local)),
            Some(boxed(cloud)),
            RouterConfig::default(),
        );
        assert_eq!(route_text(&r, "").await.unwrap(), "cloud");
    }
}
