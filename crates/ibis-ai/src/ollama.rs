//! Local Ollama embedding provider: `POST {url}/api/embed`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;

const DEFAULT_API_URL: &str = "http://127.0.0.1:11434";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Serialize)]
struct EmbedRequest {
    model: String,
    input: Vec<String>,
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
        let body = serde_json::to_string(&EmbedRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
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
        Ok(resp_data.embeddings)
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
            r#"{"model":"nomic-embed-text","input":["a","b"]}"#
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
