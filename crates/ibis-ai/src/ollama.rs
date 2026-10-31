//! Local Ollama embedding provider: `POST {url}/api/embed`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

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

/// Embedding cache cap: cleared wholesale when exceeded (query texts are
/// short; the cap bounds memory without an LRU dance).
const EMBED_CACHE_CAP: usize = 1024;

/// Ollama serves requests with limited parallelism (`OLLAMA_NUM_PARALLEL`,
/// traditionally 1): yesterday's pile-up of concurrent embed requests
/// surfaced as `ollama API request failed` transport errors. All
/// `/api/embed` calls go through this FIFO queue + limiter instead — one
/// permit per in-flight HTTP call, cache hits never queue.
const EMBED_CONCURRENCY_DEFAULT: usize = 1;

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
        }
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
        if !misses.is_empty() {
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
                input: misses.clone(),
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
            if resp_data.embeddings.len() != misses.len() {
                return Err(AiError::other(format!(
                    "ollama returned {} embeddings for {} inputs",
                    resp_data.embeddings.len(),
                    misses.len()
                )));
            }
            {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                if cache.len() + misses.len() > EMBED_CACHE_CAP {
                    cache.clear();
                }
                for (text, embedding) in misses.into_iter().zip(resp_data.embeddings) {
                    cache.insert(text, embedding);
                }
            }
        }
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        Ok(texts
            .iter()
            .map(|text| {
                cache
                    .get(text)
                    .cloned()
                    .expect("every input was cached above")
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
}
