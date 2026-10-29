//! Cloud failover pool: primary provider first, then the
//! configured fallback order.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// Tries cloud providers in order with failover. Non-functional providers are
/// dropped at construction; an empty `primary` picks the first functional
/// provider from `order`.
pub struct CloudPool {
    primary: String,
    providers: HashMap<String, Arc<dyn AIProvider>>,
    order: Vec<String>,
}

impl CloudPool {
    pub async fn new(
        primary: &str,
        order: &[String],
        providers: HashMap<String, Arc<dyn AIProvider>>,
    ) -> Self {
        let mut clean: HashMap<String, Arc<dyn AIProvider>> = HashMap::new();
        for (name, provider) in providers {
            if provider.is_functional().await {
                clean.insert(name, provider);
            }
        }
        let mut primary = primary.to_string();
        if primary.is_empty() {
            for name in order {
                if clean.contains_key(name) {
                    primary = name.clone();
                    break;
                }
            }
        }
        CloudPool {
            primary,
            providers: clean,
            order: order.to_vec(),
        }
    }

    /// Reports whether any named cloud is functional.
    pub fn has_any(&self) -> bool {
        self.is_functional_impl()
    }

    fn is_functional_impl(&self) -> bool {
        !self.providers.is_empty()
    }

    fn try_order(&self) -> Vec<String> {
        let mut try_order = Vec::with_capacity(self.order.len() + 1);
        if !self.primary.is_empty() {
            try_order.push(self.primary.clone());
        }
        for name in &self.order {
            if name == &self.primary {
                continue;
            }
            try_order.push(name.clone());
        }
        try_order
    }
}

#[async_trait]
impl AIProvider for CloudPool {
    fn name(&self) -> &str {
        "cloud_pool"
    }

    async fn is_functional(&self) -> bool {
        self.is_functional_impl()
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        if !self.is_functional_impl() {
            return Err(AiError::NoCloudProviders);
        }
        let mut last_err = None;
        for name in self.try_order() {
            let Some(provider) = self.providers.get(&name) else {
                continue;
            };
            match provider.generate_content(contents, cfg.clone()).await {
                Ok(cand) => return Ok(cand),
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or(AiError::CloudPoolExhausted))
    }
}

/// `openai` → `openai_compat`, `anthropic` → `claude`.
pub fn normalize_cloud_name(s: &str) -> String {
    let s = s.trim().to_lowercase();
    match s.as_str() {
        "openai" => "openai_compat".to_string(),
        "anthropic" => "claude".to_string(),
        _ => s,
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
        err: Option<AiError>,
        text: String,
        calls: Arc<AtomicUsize>,
    }

    impl Stub {
        fn ok(name: &str, text: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: None,
                text: text.to_string(),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn failing(name: &str, err: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: true,
                err: Some(AiError::other(err)),
                text: String::new(),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn nonfunctional(name: &str) -> Self {
            Stub {
                name: name.to_string(),
                functional: false,
                err: None,
                text: String::new(),
                calls: Arc::new(AtomicUsize::new(0)),
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
            _cfg: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
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

    fn map(entries: Vec<Stub>) -> HashMap<String, Arc<dyn AIProvider>> {
        entries
            .into_iter()
            .map(|s| (s.name.clone(), Arc::new(s) as Arc<dyn AIProvider>))
            .collect()
    }

    async fn pool_text(p: &CloudPool) -> Result<String, AiError> {
        let cand = p.generate_content(&[], GenerationConfig::default()).await?;
        Ok(cand.content.parts[0].text.clone())
    }

    #[tokio::test]
    async fn failover_in_order() {
        let bad = Stub::failing("gemini", "quota");
        let good = Stub::ok("claude", "ok");
        let order = vec!["gemini".to_string(), "claude".to_string()];
        let pool = CloudPool::new("gemini", &order, map(vec![bad, good])).await;
        assert_eq!(pool_text(&pool).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn empty_primary_picks_first_functional_in_order() {
        let order = vec!["gemini".to_string(), "claude".to_string()];
        let pool = CloudPool::new("", &order, map(vec![Stub::ok("claude", "claude-text")])).await;
        assert_eq!(pool.primary, "claude");
        assert_eq!(pool_text(&pool).await.unwrap(), "claude-text");
    }

    #[tokio::test]
    async fn nonfunctional_providers_filtered_out() {
        let order = vec!["gemini".to_string(), "claude".to_string()];
        let pool = CloudPool::new(
            "gemini",
            &order,
            map(vec![
                Stub::nonfunctional("gemini"),
                Stub::ok("claude", "ok"),
            ]),
        )
        .await;
        assert!(pool.has_any());
        assert_eq!(pool_text(&pool).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn empty_pool_is_not_functional() {
        let order = vec!["gemini".to_string()];
        let pool = CloudPool::new("gemini", &order, HashMap::new()).await;
        assert!(!pool.has_any());
        let err = pool
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no cloud providers configured");
    }

    #[tokio::test]
    async fn all_failures_surface_last_error() {
        let order = vec!["gemini".to_string(), "claude".to_string()];
        let pool = CloudPool::new(
            "gemini",
            &order,
            map(vec![
                Stub::failing("gemini", "quota"),
                Stub::failing("claude", "overloaded"),
            ]),
        )
        .await;
        let err = pool
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "overloaded");
    }

    #[test]
    fn normalizes_cloud_names() {
        assert_eq!(normalize_cloud_name("OpenAI"), "openai_compat");
        assert_eq!(normalize_cloud_name(" anthropic "), "claude");
        assert_eq!(normalize_cloud_name("gemini"), "gemini");
        assert_eq!(normalize_cloud_name("openai_compat"), "openai_compat");
    }
}
