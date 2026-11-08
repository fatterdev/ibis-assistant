//! D12 — exact-match LLM response cache seam.
//!
//! v1 ships the **exact-match tier only**: a byte-identical prompt under the
//! same call site, routing identity and invalidation context is served from
//! the store instead of the provider. The semantic tier stays out by design
//! (a wrong semantic hit is a silent quality regression; it needs a
//! calibration dataset and an error metric first).
//!
//! Best-effort by contract, like [`crate::EmbeddingCacheStore`] and
//! [`crate::CostStore`]: a missing or failing store degrades to the live
//! provider — correctness never depends on the cache. Call sites opt in by
//! setting [`crate::GenerationConfig::response_cache`]; everything else
//! (notably `ask_project`/agentic) stays uncached.
//!
//! Key shape: `{call_site}:{model}:{sha256(context, full_prompt)}`. The
//! invalidation context (code-index HEAD oid, `rules/` fingerprint) is folded
//! into the hash so the key stays bounded; the model component comes from
//! [`crate::AIProvider::cache_identity`], which composite providers override
//! with their routing fingerprint.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig, Part};

/// One cached response: the text of the first content part plus the finish
/// reason. The three v1 call sites consume `parts[0].text`; multi-part
/// responses are intentionally out of scope (a miss stays the safe fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedResponse {
    pub text: String,
    pub finish_reason: String,
}

/// Persistent response-cache seam: exact-key lookups, no ANN. `get` returns
/// `None` on miss **or** on any store failure (callers never branch on
/// errors); `put` is best-effort. `prune` drops entries older than the TTL
/// and returns the pruned row count (default: no-op).
#[async_trait]
pub trait ResponseCacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<CachedResponse>;
    async fn put(&self, key: &str, response: CachedResponse);
    async fn prune(&self, ttl_secs: u64) -> usize {
        let _ = ttl_secs;
        0
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Deterministic fingerprint of the full prompt (every content role and part
/// with length prefixes, so two different part splits cannot collide).
fn prompt_fingerprint(contents: &[Content]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ibis.response_cache.v1\0");
    hasher.update((contents.len() as u64).to_le_bytes());
    for content in contents {
        hasher.update((content.role.len() as u64).to_le_bytes());
        hasher.update(content.role.as_bytes());
        hasher.update((content.parts.len() as u64).to_le_bytes());
        for part in &content.parts {
            hasher.update((part.text.len() as u64).to_le_bytes());
            hasher.update(part.text.as_bytes());
        }
    }
    hex(hasher.finalize())
}

/// `{call_site}:{model}:{sha256(context \0 prompt)}` — the call site and the
/// routing identity are readable, the prompt and its invalidation context are
/// hashed. Model names may contain `:` (`qwen2.5-coder:7b`); the key is
/// never parsed back, so the shape is display-only.
pub fn response_cache_key(
    call_site: &str,
    model: &str,
    context: &str,
    contents: &[Content],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ibis.response_cache.v1.context\0");
    hasher.update((context.len() as u64).to_le_bytes());
    hasher.update(context.as_bytes());
    hasher.update(prompt_fingerprint(contents).as_bytes());
    format!("{call_site}:{model}:{}", hex(hasher.finalize()))
}

/// Fingerprint of the ast-grep `rules/` directory: SHA-256 over the sorted
/// `(file name, SHA-256(content))` pairs of every `*.yml` file. A missing or
/// unreadable directory is a stable `"none"`; adding, removing or editing a
/// rule changes the fingerprint, so a cached rule-synthesis response can
/// never outlive the rule set it was produced against.
pub fn rules_fingerprint(rules_dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(rules_dir) else {
        return "none".to_string();
    };
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        files.push((
            entry.file_name().to_string_lossy().into_owned(),
            hex(Sha256::digest(&bytes)),
        ));
    }
    files.sort();
    let mut hasher = Sha256::new();
    for (name, content_hash) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(content_hash.as_bytes());
    }
    hex(hasher.finalize())
}

/// Provider decorator: consult the store before the inner provider when the
/// call carries a [`crate::ResponseCacheSpec`], write the response back on a
/// miss. Without a directive it is a transparent pass-through (so wrapping
/// the whole reasoning stack never changes uncached call sites).
pub struct CachedGenerate {
    inner: Arc<dyn AIProvider>,
    store: Arc<dyn ResponseCacheStore>,
}

impl CachedGenerate {
    pub fn new(inner: Arc<dyn AIProvider>, store: Arc<dyn ResponseCacheStore>) -> Self {
        CachedGenerate { inner, store }
    }

    fn candidate_from_hit(hit: CachedResponse) -> Candidate {
        Candidate {
            content: Content {
                role: "model".to_string(),
                parts: vec![Part { text: hit.text }],
            },
            finish_reason: hit.finish_reason,
            // A cache hit costs nothing: leaving usage unset keeps the cost
            // tracker from billing a call that never happened.
            usage_metadata: None,
        }
    }
}

#[async_trait]
impl AIProvider for CachedGenerate {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> Option<&str> {
        self.inner.model()
    }

    fn cache_identity(&self, route_hint: &str) -> String {
        self.inner.cache_identity(route_hint)
    }

    async fn is_functional(&self) -> bool {
        self.inner.is_functional().await
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        self.inner.embed_text(text).await
    }

    async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        self.inner.batch_embed_text(texts).await
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let Some(spec) = config.response_cache.clone() else {
            return self.inner.generate_content(contents, config).await;
        };
        let model = self.inner.cache_identity(&config.route_hint);
        let call_site = spec.call_site.as_str();
        let key = response_cache_key(call_site, &model, &spec.context, contents);
        if let Some(hit) = self.store.get(&key).await {
            tracing::debug!(
                target: "ibis_ai::response_cache",
                call_site,
                model = %model,
                "response cache hit"
            );
            return Ok(Self::candidate_from_hit(hit));
        }
        let candidate = self.inner.generate_content(contents, config).await?;
        if let Some(text) = candidate
            .content
            .parts
            .first()
            .map(|p| p.text.clone())
            .filter(|text| !text.is_empty())
        {
            tracing::debug!(
                target: "ibis_ai::response_cache",
                call_site,
                model = %model,
                "response cache miss; write-back"
            );
            self.store
                .put(
                    &key,
                    CachedResponse {
                        text,
                        finish_reason: candidate.finish_reason.clone(),
                    },
                )
                .await;
        }
        Ok(candidate)
    }

    async fn stop(&self) {
        self.inner.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::types::{ResponseCacheCallSite, UsageMetadata};

    #[derive(Default)]
    struct MemoryStore {
        rows: Mutex<HashMap<String, CachedResponse>>,
    }

    #[async_trait]
    impl ResponseCacheStore for MemoryStore {
        async fn get(&self, key: &str) -> Option<CachedResponse> {
            self.rows.lock().unwrap().get(key).cloned()
        }
        async fn put(&self, key: &str, response: CachedResponse) {
            self.rows.lock().unwrap().insert(key.to_string(), response);
        }
    }

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        reply: String,
        fail_next: AtomicUsize,
    }

    impl CountingProvider {
        fn new(calls: &Arc<AtomicUsize>, reply: &str) -> Self {
            CountingProvider {
                calls: calls.clone(),
                reply: reply.to_string(),
                fail_next: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl AIProvider for CountingProvider {
        fn name(&self) -> &str {
            "counting"
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_next.load(Ordering::SeqCst) > 0 {
                self.fail_next.fetch_sub(1, Ordering::SeqCst);
                return Err(AiError::other("boom"));
            }
            Ok(Candidate {
                content: Content::text(self.reply.clone()),
                finish_reason: "STOP".to_string(),
                usage_metadata: Some(UsageMetadata::default()),
            })
        }
    }

    fn cached_call(cfg: GenerationConfig) -> (GenerationConfig, Vec<Content>) {
        (
            cfg,
            vec![Content::text("Analyze this log line: ERROR boom")],
        )
    }

    fn directive() -> GenerationConfig {
        GenerationConfig {
            response_cache: Some(crate::ResponseCacheSpec::new(
                ResponseCacheCallSite::LogAnalyze,
                "head:abc",
            )),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn second_identical_call_is_served_from_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider::new(&calls, "cached-reply"));
        let store = Arc::new(MemoryStore::default());
        let cached = CachedGenerate::new(provider, store);

        let (cfg, contents) = cached_call(directive());
        let first = cached
            .generate_content(&contents, cfg.clone())
            .await
            .expect("first call");
        assert_eq!(first.content.parts[0].text, "cached-reply");
        assert!(first.usage_metadata.is_some(), "live call keeps usage");

        let second = cached
            .generate_content(&contents, cfg)
            .await
            .expect("second call");
        assert_eq!(second.content.parts[0].text, "cached-reply");
        assert_eq!(second.finish_reason, "STOP");
        assert!(
            second.usage_metadata.is_none(),
            "a cache hit never bills tokens"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the provider must be called once"
        );
    }

    #[tokio::test]
    async fn no_directive_is_a_transparent_pass_through() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider::new(&calls, "reply"));
        let cached = CachedGenerate::new(provider, Arc::new(MemoryStore::default()));

        let (cfg, contents) = cached_call(GenerationConfig::default());
        cached
            .generate_content(&contents, cfg.clone())
            .await
            .unwrap();
        cached.generate_content(&contents, cfg).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "ask/agentic calls stay uncached"
        );
    }

    #[tokio::test]
    async fn context_prompt_and_call_site_partition_the_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider::new(&calls, "reply"));
        let cached = CachedGenerate::new(provider, Arc::new(MemoryStore::default()));

        let (base, contents) = cached_call(directive());
        cached
            .generate_content(&contents, base.clone())
            .await
            .unwrap();
        cached.generate_content(&contents, base).await.unwrap();

        // A different invalidation context is a miss.
        let other_context = GenerationConfig {
            response_cache: Some(crate::ResponseCacheSpec::new(
                ResponseCacheCallSite::LogAnalyze,
                "head:def",
            )),
            ..Default::default()
        };
        cached
            .generate_content(&contents, other_context.clone())
            .await
            .unwrap();
        // A different call site is a miss.
        let other_site = GenerationConfig {
            response_cache: Some(crate::ResponseCacheSpec::new(
                ResponseCacheCallSite::RuleSynth,
                "head:abc",
            )),
            ..Default::default()
        };
        cached
            .generate_content(&contents, other_site.clone())
            .await
            .unwrap();
        // A different prompt is a miss.
        let other_prompt = vec![Content::text("a different prompt")];
        cached
            .generate_content(&other_prompt, other_site)
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn provider_errors_are_never_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider::new(&calls, "reply"));
        provider.fail_next.store(1, Ordering::SeqCst);
        let cached = CachedGenerate::new(provider, Arc::new(MemoryStore::default()));

        let (cfg, contents) = cached_call(directive());
        assert!(cached
            .generate_content(&contents, cfg.clone())
            .await
            .is_err());
        let ok = cached
            .generate_content(&contents, cfg.clone())
            .await
            .expect("second attempt succeeds");
        assert_eq!(ok.content.parts[0].text, "reply");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the failure must not be cached"
        );
    }

    #[test]
    fn key_is_stable_and_sensitive() {
        let contents = [Content::text("prompt")];
        let a = response_cache_key("log_analyze", "m", "head:1", &contents);
        let b = response_cache_key("log_analyze", "m", "head:1", &contents);
        assert_eq!(a, b);
        assert_ne!(
            a,
            response_cache_key("log_analyze", "m2", "head:1", &contents)
        );
        assert_ne!(
            a,
            response_cache_key("log_analyze", "m", "head:2", &contents)
        );
        assert_ne!(a, response_cache_key("other", "m", "head:1", &contents));
        assert_ne!(
            a,
            response_cache_key("log_analyze", "m", "head:1", &[Content::text("other")])
        );
        // Model names with `:` are display-safe (the key is never parsed).
        let model = "qwen2.5-coder:7b";
        assert!(response_cache_key("x", model, "", &contents).starts_with("x:qwen2.5-coder:7b:"));
    }

    #[test]
    fn prompt_fingerprint_covers_role_and_part_boundaries() {
        let one_part = [Content {
            role: "user".to_string(),
            parts: vec![Part {
                text: "ab".to_string(),
            }],
        }];
        let two_parts = [Content {
            role: "user".to_string(),
            parts: vec![
                Part {
                    text: "a".to_string(),
                },
                Part {
                    text: "b".to_string(),
                },
            ],
        }];
        assert_ne!(
            prompt_fingerprint(&one_part),
            prompt_fingerprint(&two_parts)
        );
    }

    #[test]
    fn rules_fingerprint_tracks_content_and_is_order_stable() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        let missing = rules_fingerprint(&dir.path().join("missing"));
        assert_eq!(missing, "none");

        std::fs::write(rules.join("a.yml"), "id: a\n").unwrap();
        std::fs::write(rules.join("b.yml"), "id: b\n").unwrap();
        let first = rules_fingerprint(&rules);
        assert_eq!(first, rules_fingerprint(&rules), "stable across reads");

        // Rewriting the same files in reverse creation order keeps the hash.
        std::fs::write(rules.join("b.yml"), "id: b\n").unwrap();
        std::fs::write(rules.join("a.yml"), "id: a\n").unwrap();
        assert_eq!(first, rules_fingerprint(&rules));

        // Any content change invalidates.
        std::fs::write(rules.join("a.yml"), "id: a\nrule: changed\n").unwrap();
        assert_ne!(first, rules_fingerprint(&rules));

        // Non-yml files do not participate.
        let before_txt = rules_fingerprint(&rules);
        std::fs::write(rules.join("notes.txt"), "ignore me").unwrap();
        assert_eq!(before_txt, rules_fingerprint(&rules));
    }
}
