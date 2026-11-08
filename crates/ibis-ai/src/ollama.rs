//! Local Ollama embedding provider: `POST {url}/api/embed`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::embedding_cache::{embedding_cache_key, EmbeddingCacheStore};
use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;

const DEFAULT_API_URL: &str = "http://127.0.0.1:11434";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Ollama unloads idle models after its keep-alive (default 5m) and
/// reloading the embedding model costs ~4s per call — ask for a longer
/// residency so repeated searches hit a warm model. `IBIS_OLLAMA_KEEP_ALIVE`
/// overrides it (note: the API parameter overrides the server's
/// `OLLAMA_KEEP_ALIVE` for the embedding model).
const EMBED_KEEP_ALIVE_DEFAULT: &str = "30m";

fn embed_keep_alive() -> &'static String {
    use std::sync::OnceLock;
    static KEEP_ALIVE: OnceLock<String> = OnceLock::new();
    KEEP_ALIVE.get_or_init(|| {
        std::env::var("IBIS_OLLAMA_KEEP_ALIVE")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| EMBED_KEEP_ALIVE_DEFAULT.to_string())
    })
}

/// Ollama serves requests with limited parallelism (`OLLAMA_NUM_PARALLEL`,
/// traditionally 1): yesterday's pile-up of concurrent embed requests
/// surfaced as `ollama API request failed` transport errors. All
/// `/api/embed` calls go through this FIFO queue + limiter instead — one
/// permit per in-flight HTTP call, cache hits never queue.
const EMBED_CONCURRENCY_DEFAULT: usize = 1;

/// Cache cap default. Exceeded → wholesale clear (query texts are short;
/// the cap bounds memory without an LRU dance).
const EMBED_CACHE_CAP: usize = 1024;

/// Process-wide embed queue, shared by every [`OllamaProvider`] instance
/// (they all target the same local server). `IBIS_OLLAMA_EMBED_CONCURRENCY`
/// overrides the permit count when the local Ollama runs a higher
/// `OLLAMA_NUM_PARALLEL`.
fn embed_queue() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    use std::sync::{Arc, OnceLock};
    use tokio::sync::Semaphore;
    static QUEUE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let concurrency = std::env::var("IBIS_OLLAMA_EMBED_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(EMBED_CONCURRENCY_DEFAULT);
        Arc::new(Semaphore::new(concurrency))
    })
}

#[derive(Debug, Serialize)]
struct EmbedRequest {
    model: String,
    input: Vec<String>,
    keep_alive: String,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

/// `EmbeddingProvider` for local Ollama instances.
pub struct OllamaProvider {
    model: String,
    api_url: String,
    http: reqwest::Client,
    /// Query-embedding cache keyed by input text (the model is fixed per
    /// instance; identical inputs are deterministic).
    cache: Mutex<HashMap<String, Vec<f32>>>,
    /// Cache cap; tests shrink it to force wholesale clears deterministically.
    cache_cap: usize,
    /// B6: optional persistent (L2) embedding cache — consulted before the
    /// HTTP batch and written back after a fresh fetch. Best-effort: a
    /// missing or failing store degrades to the HTTP path.
    disk_cache: Option<Arc<dyn EmbeddingCacheStore>>,
    /// Vector dimension inside the L2 cache key (schema DiskANN DIMENSION);
    /// only meaningful once a store is attached via [`Self::with_disk_cache`].
    disk_cache_dim: usize,
}

impl OllamaProvider {
    pub fn new(model: &str, api_url: &str) -> Self {
        let api_url = if api_url.is_empty() {
            DEFAULT_API_URL.to_string()
        } else {
            api_url.to_string()
        };
        OllamaProvider {
            model: model.to_string(),
            api_url,
            http: http_client(HTTP_TIMEOUT),
            cache: Mutex::new(HashMap::new()),
            cache_cap: EMBED_CACHE_CAP,
            disk_cache: None,
            disk_cache_dim: 0,
        }
    }

    /// B6: attaches a persistent (L2) embedding cache. `dim` must match the
    /// schema's DiskANN `DIMENSION` (768) — it is part of the cache key, so a
    /// mismatched writer can never poison the lookups.
    pub fn with_disk_cache(mut self, store: Arc<dyn EmbeddingCacheStore>, dim: usize) -> Self {
        self.disk_cache = Some(store);
        self.disk_cache_dim = dim.max(1);
        self
    }

    /// L2 lookup (B6): `None` on miss, dimension mismatch, or any store
    /// failure — the HTTP path stays the only correctness path.
    async fn l2_get(&self, text: &str) -> Option<Vec<f32>> {
        let store = self.disk_cache.as_ref()?;
        let key = embedding_cache_key(&self.model, self.disk_cache_dim, text);
        match store.get(&key).await {
            Some(vec) if vec.len() == self.disk_cache_dim => Some(vec),
            Some(_) => {
                tracing::warn!(
                    "embedding disk cache: dimension mismatch on {key} (expected {}), treating as miss",
                    self.disk_cache_dim
                );
                None
            }
            None => None,
        }
    }

    /// L2 write-back (B6): fire-and-forget; the adapter logs and swallows its
    /// own failures.
    async fn l2_put(&self, text: &str, embedding: Vec<f32>) {
        let Some(store) = self.disk_cache.as_ref() else {
            return;
        };
        let key = embedding_cache_key(&self.model, self.disk_cache_dim, text);
        store.put(&key, embedding).await;
    }

    /// Test-only constructor with a shrunken cache cap so the wholesale
    /// clear path churns under the concurrent stress test.
    #[cfg(test)]
    fn with_cache_cap(model: &str, api_url: &str, cap: usize) -> Self {
        let mut provider = OllamaProvider::new(model, api_url);
        provider.cache_cap = cap.max(1);
        provider
    }

    /// Queue + one batched HTTP call; the fetched embeddings are returned by
    /// value AND cached. The wholesale clear (cap) only affects the cache:
    /// callers rely on the returned pairs, never on re-reading the cache, so
    /// a concurrent clear cannot wipe a result that was already computed.
    async fn fetch_embeddings(&self, texts: &[String]) -> Result<Vec<(String, Vec<f32>)>, AiError> {
        // Coda + limiter: one in-flight call per permit (FIFO); the wait
        // is before the request, so the HTTP timeout covers the call
        // itself, not the queue.
        let queue_wait = std::time::Instant::now();
        let _permit = embed_queue()
            .acquire()
            .await
            .map_err(|_| AiError::other("ollama embed queue is closed"))?;
        let waited = queue_wait.elapsed();
        if waited > std::time::Duration::from_secs(1) {
            tracing::info!("ollama embed queue: slot acquired after {waited:?}");
        }
        let body = serde_json::to_string(&EmbedRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
            keep_alive: embed_keep_alive().clone(),
        })
        .map_err(|e| AiError::other(format!("failed to marshal Ollama embed request: {e}")))?;
        // did not trim a trailing slash here (chat did) — a
        // misconfigured `{url}/` produced `{url}//api/embed`; trim for both.
        let url = format!("{}/api/embed", self.api_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| AiError::other(format!("ollama API request failed: {e}")))?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await?;
        if status != 200 {
            return Err(AiError::other(format!(
                "ollama API returned status {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        let resp_data: EmbedResponse = serde_json::from_slice(&raw)
            .map_err(|e| AiError::other(format!("failed to decode Ollama response: {e}")))?;
        if resp_data.embeddings.is_empty() {
            return Err(AiError::other("ollama returned empty embeddings"));
        }
        if resp_data.embeddings.len() != texts.len() {
            return Err(AiError::other(format!(
                "ollama returned {} embeddings for {} inputs",
                resp_data.embeddings.len(),
                texts.len()
            )));
        }
        let fresh: Vec<(String, Vec<f32>)> =
            texts.iter().cloned().zip(resp_data.embeddings).collect();
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() + fresh.len() > self.cache_cap {
                cache.clear();
            }
            for (text, embedding) in &fresh {
                cache.insert(text.clone(), embedding.clone());
            }
        }
        Ok(fresh)
    }
}

#[async_trait]
impl AIProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn is_functional(&self) -> bool {
        !self.api_url.is_empty()
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        let embeddings = self.batch_embed_text(&[text.to_string()]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| AiError::other("no embedding returned from Ollama"))
    }

    async fn batch_embed_text(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Cache lookups never hold the guard across the HTTP await (the
        // future must stay Send).
        let misses: Vec<String> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut misses: Vec<String> = Vec::new();
            for text in texts {
                if !cache.contains_key(text) && !misses.contains(text) {
                    misses.push(text.clone());
                }
            }
            misses
        };
        // Fresh-by-value results for the misses. Another writer's wholesale
        // clear (cap) can land between the miss pass and the hit read below,
        // so results never depend on the cache still holding an entry.
        let mut resolved: HashMap<String, Vec<f32>> = HashMap::new();
        if !misses.is_empty() {
            // B6: the L2 disk cache is consulted before the HTTP batch — one
            // exact-key lookup per miss, and an L2 hit never queues on the
            // embed FIFO (the semaphore gates HTTP calls only).
            let mut fetched: Vec<String> = Vec::new();
            for text in &misses {
                match self.l2_get(text).await {
                    Some(embedding) => {
                        resolved.insert(text.clone(), embedding);
                    }
                    None => fetched.push(text.clone()),
                }
            }
            if !fetched.is_empty() {
                for (text, embedding) in self.fetch_embeddings(&fetched).await? {
                    resolved.insert(text, embedding);
                }
                // Write-back after the response, best-effort (B6). L2 hits
                // are not re-written (their created_at stays the write time).
                for text in &fetched {
                    if let Some(embedding) = resolved.get(text) {
                        self.l2_put(text, embedding.clone()).await;
                    }
                }
            }
        }
        // Hits resolve from the cache in the same lock that decides the
        // stragglers; anything the concurrent clear already wiped joins the
        // straggler re-fetch instead of panicking on a missing entry.
        let stragglers: Vec<String> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut stragglers: Vec<String> = Vec::new();
            for text in texts {
                if resolved.contains_key(text) {
                    continue;
                }
                match cache.get(text) {
                    Some(embedding) => {
                        resolved.insert(text.clone(), embedding.clone());
                    }
                    None => stragglers.push(text.clone()),
                }
            }
            stragglers
        };
        if !stragglers.is_empty() {
            // B6: stragglers (wiped by a concurrent wholesale clear between
            // the passes) get the same L2 pass before hitting HTTP.
            let mut refetched: Vec<String> = Vec::new();
            for text in &stragglers {
                match self.l2_get(text).await {
                    Some(embedding) => {
                        resolved.insert(text.clone(), embedding);
                    }
                    None => refetched.push(text.clone()),
                }
            }
            if !refetched.is_empty() {
                for (text, embedding) in self.fetch_embeddings(&refetched).await? {
                    resolved.insert(text, embedding);
                }
                for text in &refetched {
                    if let Some(embedding) = resolved.get(text) {
                        self.l2_put(text, embedding.clone()).await;
                    }
                }
            }
        }
        Ok(texts
            .iter()
            .map(|text| {
                resolved
                    .get(text)
                    .cloned()
                    .expect("every requested text was resolved above")
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;

    #[tokio::test]
    async fn wire_contract_model_and_array_input() {
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/api/embed",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().unwrap() = String::from_utf8_lossy(&body).into_owned();
                    ok(r#"{"embeddings":[[0.1,0.2],[0.3,0.4]]}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaProvider::new("nomic-embed-text", &base_url);
        assert!(p.is_functional().await);
        let embs = p
            .batch_embed_text(&["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(embs, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
        assert_eq!(
            captured.lock().unwrap().as_str(),
            r#"{"model":"nomic-embed-text","input":["a","b"],"keep_alive":"30m"}"#
        );
    }

    /// Repeated identical queries reuse the cached embedding: the second
    /// `embed_text` and any batch re-request never touch the HTTP route.
    #[tokio::test]
    async fn repeated_queries_hit_the_cache() {
        use serde_json::Value;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_state = calls.clone();
        let app = Router::new().route(
            "/api/embed",
            post(move |req: Request| {
                let calls = handler_state.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    let parsed: Value = serde_json::from_slice(&body).unwrap();
                    let n = parsed["input"].as_array().map(Vec::len).unwrap_or(0);
                    let embs: Vec<Vec<f64>> = vec![vec![0.1, 0.2]; n];
                    ok(&serde_json::json!({ "embeddings": embs }).to_string())
                }
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaProvider::new("m", &base_url);

        let first = p.embed_text("same query").await.unwrap();
        let second = p.embed_text("same query").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call must be cached"
        );

        let batch = p
            .batch_embed_text(&["same query".to_string(), "fresh".to_string()])
            .await
            .unwrap();
        assert_eq!(batch, vec![first, vec![0.1, 0.2]]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "batch must only fetch the cache miss"
        );
    }

    #[tokio::test]
    async fn empty_batch_short_circuits() {
        let p = OllamaProvider::new("m", "http://127.0.0.1:9");
        assert!(p.batch_embed_text(&[]).await.unwrap().is_empty());
    }

    /// Regression: with the wholesale cache clear in play, the old tail
    /// (`cache.get(text).expect(...)`) could panic when a concurrent batch
    /// cleared the cache between the miss pass and the final read. The
    /// stress run shrinks the cap so clears churn while batches overlap;
    /// every caller must still resolve every text.
    #[tokio::test]
    async fn cache_clears_never_lose_a_requested_embedding() {
        use std::sync::Arc;
        let app = Router::new().route(
            "/api/embed",
            post(move |req: Request| async move {
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let n = parsed["input"].as_array().map(Vec::len).unwrap_or(0);
                let embs: Vec<Vec<f64>> = vec![vec![0.1, 0.2]; n];
                ok(&serde_json::json!({ "embeddings": embs }).to_string())
            }),
        );
        let base_url = serve(app).await;
        let provider = Arc::new(OllamaProvider::with_cache_cap("m", &base_url, 4));
        let mut tasks = Vec::new();
        for task in 0..6 {
            let provider = provider.clone();
            tasks.push(tokio::spawn(async move {
                let texts: Vec<String> = (0..6)
                    .map(|i| format!("t{task}-{i}"))
                    .chain((0..3).map(|i| format!("shared-{i}")))
                    .collect();
                let embs = provider.batch_embed_text(&texts).await.expect("batch");
                assert_eq!(embs.len(), texts.len());
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn non_200_and_empty_embeddings_error() {
        let app = Router::new().route(
            "/api/embed",
            post(|| async {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body("nope".to_string())
                    .unwrap()
                    .into_response()
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaProvider::new("m", &base_url);
        let err = p.embed_text("x").await.unwrap_err();
        assert!(err.to_string().contains("ollama API returned status 404"));
    }

    /// Concurrent embed calls are funneled through the FIFO queue: the fake
    /// server must never see more than one in-flight request (default permit
    /// count is 1), yet every caller gets its embedding.
    #[tokio::test]
    async fn concurrent_embeds_are_queued_serially() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let handler_in = in_flight.clone();
        let handler_max = max_seen.clone();
        let app = Router::new().route(
            "/api/embed",
            post(move |req: Request| {
                let in_flight = handler_in.clone();
                let max_seen = handler_max.clone();
                async move {
                    let _ = axum::body::to_bytes(req.into_body(), 1 << 20).await;
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    ok(r#"{"embeddings":[[0.1,0.2]]}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let provider = Arc::new(OllamaProvider::new("m", &base_url));
        let mut tasks = Vec::new();
        for i in 0..4 {
            let provider = provider.clone();
            tasks.push(tokio::spawn(async move {
                provider.embed_text(&format!("q{i}")).await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "embeds must be serialized through the queue"
        );
    }

    fn ok(body: &str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.to_string())
            .unwrap()
            .into_response()
    }

    #[tokio::test]
    async fn empty_embedding_list_rejected() {
        let app = Router::new().route("/api/embed", post(|| async { ok(r#"{"embeddings":[]}"#) }));
        let base_url = serve(app).await;
        let p = OllamaProvider::new("m", &base_url);
        let err = p.embed_text("x").await.unwrap_err();
        assert_eq!(err.to_string(), "ollama returned empty embeddings");
    }

    #[tokio::test]
    async fn default_url() {
        let p = OllamaProvider::new("m", "");
        assert_eq!(p.api_url, "http://127.0.0.1:11434");
    }

    /// B6 fake L2 store: counts gets/puts, optionally failing gets (the
    /// best-effort contract turns failures into misses).
    struct FakeL2 {
        rows: std::sync::Mutex<HashMap<String, Vec<f32>>>,
        gets: AtomicUsize,
        puts: AtomicUsize,
        fail_gets: AtomicBool,
    }

    impl FakeL2 {
        fn new() -> Self {
            FakeL2 {
                rows: std::sync::Mutex::new(HashMap::new()),
                gets: AtomicUsize::new(0),
                puts: AtomicUsize::new(0),
                fail_gets: AtomicBool::new(false),
            }
        }

        fn seed(&self, model: &str, dim: usize, text: &str, embedding: Vec<f32>) {
            let key = embedding_cache_key(model, dim, text);
            self.rows.lock().unwrap().insert(key, embedding);
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingCacheStore for FakeL2 {
        async fn get(&self, key: &str) -> Option<Vec<f32>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            if self.fail_gets.load(Ordering::SeqCst) {
                return None;
            }
            self.rows.lock().unwrap().get(key).cloned()
        }

        async fn put(&self, key: &str, embedding: Vec<f32>) {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.rows.lock().unwrap().insert(key.to_string(), embedding);
        }
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// An L2 hit answers the query without any HTTP call: the provider
    /// targets a dead URL, so the request can only succeed from the store.
    #[tokio::test]
    async fn l2_hit_skips_http() {
        let store = Arc::new(FakeL2::new());
        store.seed("m", 2, "seeded", vec![0.25, -0.75]);
        let p = OllamaProvider::new("m", "http://127.0.0.1:9")
            .with_disk_cache(store.clone() as Arc<dyn EmbeddingCacheStore>, 2);
        let emb = p.embed_text("seeded").await.expect("l2 hit");
        assert_eq!(emb, vec![0.25, -0.75]);
        assert_eq!(store.gets.load(Ordering::SeqCst), 1);
        assert_eq!(store.puts.load(Ordering::SeqCst), 0, "hits are not re-written");
    }

    /// After a fresh fetch the vector is written back to L2; a second
    /// provider instance (cold L1, same store) is then served from L2 with
    /// no further HTTP call.
    #[tokio::test]
    async fn l2_write_back_then_second_instance_hits_store() {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = Router::new().route(
            "/api/embed",
            post(move |_req: Request| {
                let hits = handler_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    ok(r#"{"embeddings":[[0.5,0.6]]}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let store = Arc::new(FakeL2::new());
        let first = OllamaProvider::new("m", &base_url)
            .with_disk_cache(store.clone() as Arc<dyn EmbeddingCacheStore>, 2);
        let emb = first.embed_text("fresh").await.expect("first fetch");
        assert_eq!(emb, vec![0.5, 0.6]);
        assert_eq!(store.puts.load(Ordering::SeqCst), 1, "write-back after fetch");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Cold L1, same L2: no HTTP.
        let second = OllamaProvider::new("m", &base_url)
            .with_disk_cache(store.clone() as Arc<dyn EmbeddingCacheStore>, 2);
        let emb = second.embed_text("fresh").await.expect("l2 hit");
        assert_eq!(emb, vec![0.5, 0.6]);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second call never hits HTTP");
    }

    /// A failing store degrades to the HTTP path (best-effort contract) and
    /// fresh results are still written back.
    #[tokio::test]
    async fn l2_failure_is_transparent() {
        let store = Arc::new(FakeL2::new());
        store.fail_gets.store(true, Ordering::SeqCst);
        let app = Router::new().route(
            "/api/embed",
            post(|| async { ok(r#"{"embeddings":[[0.1,0.2]]}"#) }),
        );
        let base_url = serve(app).await;
        let p = OllamaProvider::new("m", &base_url)
            .with_disk_cache(store.clone() as Arc<dyn EmbeddingCacheStore>, 2);
        let emb = p.embed_text("x").await.expect("http path");
        assert_eq!(emb, vec![0.1, 0.2]);
        assert_eq!(store.puts.load(Ordering::SeqCst), 1);
    }

    /// A row with a mismatched dimension is a miss, never a poison.
    #[tokio::test]
    async fn l2_dimension_mismatch_is_miss() {
        let store = Arc::new(FakeL2::new());
        store.seed("m", 2, "bad-dim", vec![0.1, 0.2, 0.3]); // 3 floats ≠ dim 2
        let app = Router::new().route(
            "/api/embed",
            post(|| async { ok(r#"{"embeddings":[[0.7,0.8]]}"#) }),
        );
        let base_url = serve(app).await;
        let p = OllamaProvider::new("m", &base_url)
            .with_disk_cache(store.clone() as Arc<dyn EmbeddingCacheStore>, 2);
        let emb = p.embed_text("bad-dim").await.expect("http fallback");
        assert_eq!(emb, vec![0.7, 0.8]);
    }
}
