//! Hybrid local/cloud reasoning router.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// D10: local model tier selected by a call-site (`local:S` … `local:XL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalTier {
    S,
    M,
    L,
    XL,
}

impl LocalTier {
    pub fn as_str(self) -> &'static str {
        match self {
            LocalTier::S => "S",
            LocalTier::M => "M",
            LocalTier::L => "L",
            LocalTier::XL => "XL",
        }
    }

    /// Case-insensitive tier parse (`s`, `M`, `xl`, …).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "S" => Some(LocalTier::S),
            "M" => Some(LocalTier::M),
            "L" => Some(LocalTier::L),
            "XL" => Some(LocalTier::XL),
            _ => None,
        }
    }
}

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
    Local(Option<LocalTier>),
    Cloud,
}

impl Step {
    fn label(self) -> String {
        match self {
            Step::Cloud => "cloud".to_string(),
            Step::Local(None) => "local".to_string(),
            Step::Local(Some(tier)) => format!("local:{}", tier.as_str()),
        }
    }
}

/// Splits a normalized route hint into its base policy and optional D10 tier.
fn split_hint(hint: &str) -> (&str, Option<LocalTier>) {
    match hint.split_once(':') {
        Some((base, suffix)) => match LocalTier::parse(suffix) {
            Some(tier) => (base, Some(tier)),
            None => (hint, None),
        },
        None => (hint, None),
    }
}

/// Routes generation between local providers and a cloud pool:
/// - hint `quality` → cloud first;
/// - hint `auto` → cloud only when `use_cloud_when_no_gpu` and the host is CPU-only;
/// - `local_only` → local, no fallback (`local_only:<tier>` keeps the tier);
/// - `bulk`/`bulk_local` → local then cloud;
/// - `local:S|M|L|XL` (D10) → the tier's local provider, then cloud (the
///   CPU-only `use_cloud_when_no_gpu` preference applies per tier);
/// - otherwise local first, cloud as fallback.
pub struct ReasoningRouter {
    local: Option<Box<dyn AIProvider>>,
    /// D10 per-task local models keyed by tier (`S`/`M`/`L`/`XL`); a tier
    /// hint falls back to `local` when the map has no entry.
    locals: HashMap<String, Box<dyn AIProvider>>,
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
        ReasoningRouter {
            local,
            locals: HashMap::new(),
            cloud,
            cfg,
        }
    }

    /// D10: installs the per-tier local providers. Keys are parsed
    /// case-insensitively; unknown tier names are ignored (forward compat).
    pub fn with_local_tiers(
        mut self,
        locals: impl IntoIterator<Item = (String, Box<dyn AIProvider>)>,
    ) -> Self {
        for (tier, provider) in locals {
            if let Some(tier) = LocalTier::parse(&tier) {
                self.locals.insert(tier.as_str().to_string(), provider);
            }
        }
        self
    }

    fn local_provider(&self, tier: Option<LocalTier>) -> Option<&dyn AIProvider> {
        tier.and_then(|tier| self.locals.get(tier.as_str()).map(Box::as_ref))
            .or(self.local.as_deref())
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
            Step::Local(tier) => match self.local_provider(tier) {
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

    /// D12: fingerprint the local tier selected by the hint plus the cloud
    /// pool, so switching any model (or the cloud order) invalidates cached
    /// responses even though the concrete provider is chosen per call.
    fn cache_identity(&self, route_hint: &str) -> String {
        let hint = route_hint.trim().to_lowercase();
        let (_, tier) = split_hint(&hint);
        let local = self
            .local_provider(tier)
            .map(|p| p.cache_identity(&hint))
            .unwrap_or_else(|| "none".to_string());
        let cloud = self
            .cloud
            .as_deref()
            .map(|p| p.cache_identity(&hint))
            .unwrap_or_else(|| "none".to_string());
        format!("hybrid[local={local};cloud={cloud}]")
    }

    async fn is_functional(&self) -> bool {
        if let Some(l) = &self.local {
            if l.is_functional().await {
                return true;
            }
        }
        for tier in self.locals.values() {
            if tier.is_functional().await {
                return true;
            }
        }
        match &self.cloud {
            Some(c) => c.is_functional().await,
            None => false,
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
        // D10: `local:<tier>` / `local_only:<tier>` — the base keeps the
        // existing cloud policy, the suffix picks the local provider. An
        // unknown suffix leaves the hint untouched (backward compatible).
        let (base, tier) = split_hint(&hint);
        let local = Step::Local(tier);
        let tiered_local = tier.is_some() && base != "local_only";
        let prefer_cloud = base == "quality"
            || ((base == "auto" || tiered_local)
                && self.cfg.use_cloud_when_no_gpu
                && self.cfg.cpu_only);

        let cloud_ready = matches!(&self.cloud, Some(c) if c.is_functional().await);
        let (first, second): (Step, Option<Step>) = if prefer_cloud && cloud_ready {
            (Step::Cloud, Some(local))
        } else if base == "local_only" {
            (local, None)
        } else if base == "bulk" || base == "bulk_local" || base == "local" || tier.is_some() {
            (local, Some(Step::Cloud))
        } else if prefer_cloud {
            (Step::Cloud, Some(local))
        } else {
            (local, Some(Step::Cloud))
        };
        tracing::debug!(
            target: "ibis_ai::router",
            hint = %hint,
            tier = tier.map(LocalTier::as_str).unwrap_or("-"),
            first = %first.label(),
            second = second.map(Step::label).unwrap_or_else(|| "-".to_string()),
            local_model = self.local_provider(tier).and_then(|p| p.model()).unwrap_or("-"),
            "reasoning route"        );

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
        model: Option<String>,
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
                model: None,
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
                model: None,
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
                model: None,
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
        fn with_model(mut self, model: &str) -> Self {
            self.model = Some(model.to_string());
            self
        }
    }

    #[async_trait]
    impl AIProvider for Stub {
        fn name(&self) -> &str {
            &self.name
        }
        fn model(&self) -> Option<&str> {
            self.model.as_deref()
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

    fn tier_router(
        default_text: &str,
        tiers: &[(&str, &str)],
        cloud_text: &str,
        cfg: RouterConfig,
    ) -> ReasoningRouter {
        ReasoningRouter::new(
            Some(boxed(Stub::ok("default", default_text))),
            Some(boxed(Stub::ok("gemini", cloud_text))),
            cfg,
        )
        .with_local_tiers(
            tiers
                .iter()
                .map(|(tier, text)| ((*tier).to_string(), boxed(Stub::ok("ollama", text))))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn tier_hint_routes_to_the_tier_provider() {
        let r = tier_router(
            "default-model",
            &[("S", "small"), ("M", "medium"), ("L", "large")],
            "cloud",
            RouterConfig::default(),
        );
        assert_eq!(route_text(&r, "local:S").await.unwrap(), "small");
        assert_eq!(route_text(&r, "local:m").await.unwrap(), "medium");
        assert_eq!(route_text(&r, "local:L").await.unwrap(), "large");
        // Tier absent from the map falls back to the default local provider.
        assert_eq!(route_text(&r, "local:XL").await.unwrap(), "default-model");
    }

    /// D12: the cache identity fingerprints the tier model selected by the
    /// hint and the cloud pool, so a model switch invalidates cached
    /// responses.
    #[test]
    fn cache_identity_fingerprints_tier_model_and_cloud() {
        let router = |tier_model: &str, cloud_model: &str| {
            ReasoningRouter::new(
                Some(boxed(Stub::ok("default", "x").with_model("base-model"))),
                Some(boxed(Stub::ok("gemini", "x").with_model(cloud_model))),
                RouterConfig::default(),
            )
            .with_local_tiers(vec![(
                "M".to_string(),
                boxed(Stub::ok("ollama", "x").with_model(tier_model)),
            )])
        };
        let a = router("m1", "c1");
        let b = router("m2", "c1");
        let c = router("m1", "c2");
        let id = a.cache_identity("local:M");
        assert!(id.contains("m1"), "tier model is part of the identity");
        assert_eq!(id, a.cache_identity("local:M"), "stable");
        assert_ne!(
            id,
            b.cache_identity("local:M"),
            "a tier model switch must invalidate"
        );
        assert_ne!(
            id,
            c.cache_identity("local:M"),
            "a cloud model switch must invalidate"
        );
        assert_ne!(
            id,
            a.cache_identity("local:S"),
            "a different hint resolves a different tier identity"
        );
    }

    #[tokio::test]
    async fn tier_hint_falls_back_to_cloud_when_tier_fails() {
        let r = ReasoningRouter::new(
            Some(boxed(Stub::ok("default", "default-model"))),
            Some(boxed(Stub::ok("gemini", "cloud"))),
            RouterConfig::default(),
        )
        .with_local_tiers(vec![(
            "M".to_string(),
            boxed(Stub::failing("ollama", "model not pulled")),
        )]);
        assert_eq!(route_text(&r, "local:M").await.unwrap(), "cloud");
    }

    #[tokio::test]
    async fn local_only_tier_never_leaks_to_cloud() {
        let cloud_calls = Arc::new(AtomicUsize::new(0));
        let mut cloud = Stub::ok("gemini", "leaked");
        cloud.calls = cloud_calls.clone();
        let r = ReasoningRouter::new(
            Some(boxed(Stub::ok("default", "default-model"))),
            Some(boxed(cloud)),
            RouterConfig::default(),
        )
        .with_local_tiers(vec![(
            "M".to_string(),
            boxed(Stub::failing("ollama", "model not pulled")),
        )]);
        let err = route_text(&r, "local_only:M").await.unwrap_err();
        assert_eq!(err.to_string(), "model not pulled");
        assert_eq!(cloud_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tier_hint_prefers_cloud_on_cpu_when_enabled() {
        let r = tier_router(
            "default-model",
            &[("S", "small")],
            "cloud",
            RouterConfig {
                use_cloud_when_no_gpu: true,
                cpu_only: true,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "local:S").await.unwrap(), "cloud");

        // `local_only:S` keeps the no-cloud contract even on CPU-only.
        let r = tier_router(
            "default-model",
            &[("S", "small")],
            "cloud",
            RouterConfig {
                use_cloud_when_no_gpu: true,
                cpu_only: true,
                ..Default::default()
            },
        );
        assert_eq!(route_text(&r, "local_only:S").await.unwrap(), "small");
    }

    #[tokio::test]
    async fn unknown_tier_suffix_keeps_the_base_hint() {
        let r = tier_router(
            "default-model",
            &[("S", "small")],
            "cloud",
            RouterConfig::default(),
        );
        assert_eq!(route_text(&r, "local:Q").await.unwrap(), "default-model");
    }
}
