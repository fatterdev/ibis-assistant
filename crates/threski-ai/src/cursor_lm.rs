//! IDE LM bridge provider: calls the ide-host bridge
//! (`vscode.lm`) over loopback HTTP. Reasoning only — embeddings stay on
//! Ollama.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// Used when `THRESKI_LM_BRIDGE_URL` is unset.
pub const DEFAULT_IDE_BRIDGE_URL: &str = "http://127.0.0.1:39201";

const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

#[derive(Debug, Serialize)]
struct BridgeRequest {
    contents: Vec<Content>,
    config: GenerationConfig,
}

#[derive(Debug, Deserialize)]
struct BridgeResponse {
    #[serde(default)]
    candidate: Candidate,
    #[serde(default)]
    error: String,
}

/// Calls the IDE host LM bridge over a loopback HTTP JSON RPC.
pub struct CursorLMProvider {
    base_url: String,
    http: reqwest::Client,
}

impl CursorLMProvider {
    /// Creates a bridge client. Empty `base_url` uses `THRESKI_LM_BRIDGE_URL`
    /// or the default loopback URL.
    pub fn new(base_url: &str) -> Self {
        let mut base_url = base_url.trim().to_string();
        if base_url.is_empty() {
            base_url = std::env::var("THRESKI_LM_BRIDGE_URL")
                .unwrap_or_default()
                .trim()
                .to_string();
        }
        if base_url.is_empty() {
            base_url = DEFAULT_IDE_BRIDGE_URL.to_string();
        }
        CursorLMProvider {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: http_client(HTTP_TIMEOUT),
        }
    }
}

#[async_trait]
impl AIProvider for CursorLMProvider {
    fn name(&self) -> &str {
        "cursor"
    }

    /// Health probe: `GET /health` must answer 200 within 400ms
    ///
    async fn is_functional(&self) -> bool {
        if self.base_url.is_empty() {
            return false;
        }
        let url = format!("{}/health", self.base_url);
        let probe = http_client(HEALTH_TIMEOUT);
        match probe.get(url).send().await {
            Ok(resp) => resp.status().as_u16() == 200,
            Err(_) => false,
        }
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        if self.base_url.is_empty() {
            return Err(AiError::other("cursor LM bridge not configured"));
        }
        let payload = serde_json::to_string(&BridgeRequest {
            contents: contents.to_vec(),
            config: cfg,
        })
        .map_err(|e| AiError::other(e.to_string()))?;
        let resp = self
            .http
            .post(format!("{}/v1/generate", self.base_url))
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| AiError::other(format!("cursor LM bridge: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await?;
        if status != 200 {
            return Err(AiError::other(format!(
                "cursor LM bridge HTTP {}: {}",
                status,
                String::from_utf8_lossy(&body).trim()
            )));
        }
        let out: BridgeResponse =
            serde_json::from_slice(&body).map_err(|e| AiError::other(e.to_string()))?;
        if !out.error.is_empty() {
            return Err(AiError::other(out.error));
        }
        Ok(out.candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use crate::types::Part;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::Arc;

    #[tokio::test]
    async fn generate_over_bridge() {
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new()
            .route("/health", get(|| async { StatusCode::OK }))
            .route(
                "/v1/generate",
                post(move |req: Request| {
                    let captured = handler_state.clone();
                    async move {
                        let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                            .await
                            .unwrap()
                            .to_vec();
                        *captured.lock().unwrap() = String::from_utf8_lossy(&body).into_owned();
                        ok(
                            r#"{"candidate":{"content":{"role":"model","parts":[{"text":"hello-from-bridge"}]},"finishReason":"stop"}}"#,
                        )
                    }
                }),
            );
        let base_url = serve(app).await;
        let p = CursorLMProvider::new(&base_url);
        assert!(p.is_functional().await);
        let cand = p
            .generate_content(
                &[Content {
                    role: String::new(),
                    parts: vec![Part { text: "hi".into() }],
                }],
                GenerationConfig::default(),
            )
            .await
            .unwrap();
        assert!(cand.content.parts[0].text.contains("hello-from-bridge"));

        let payload: serde_json::Value =
            serde_json::from_str(captured.lock().unwrap().as_str()).unwrap();
        assert_eq!(payload["contents"][0]["parts"][0]["text"], "hi");
        assert!(payload.get("config").is_some());
    }

    #[tokio::test]
    async fn bridge_error_field_surfaces() {
        let app = Router::new()
            .route("/health", get(|| async { StatusCode::OK }))
            .route(
                "/v1/generate",
                post(|| async { ok(r#"{"error":"no model available"}"#) }),
            );
        let base_url = serve(app).await;
        let p = CursorLMProvider::new(&base_url);
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no model available");
    }

    #[tokio::test]
    async fn non_200_reports_status_and_body() {
        let app = Router::new()
            .route("/health", get(|| async { StatusCode::OK }))
            .route(
                "/v1/generate",
                post(|| async {
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body("host busy".to_string())
                        .unwrap()
                        .into_response()
                }),
            );
        let base_url = serve(app).await;
        let p = CursorLMProvider::new(&base_url);
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "cursor LM bridge HTTP 503: host busy");
    }

    #[tokio::test]
    async fn down_bridge_is_not_functional() {
        let p = CursorLMProvider::new("http://127.0.0.1:1");
        assert!(!p.is_functional().await);
    }

    #[tokio::test]
    async fn health_timeout_is_not_functional() {
        let app = Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                StatusCode::OK
            }),
        );
        let base_url = serve(app).await;
        let p = CursorLMProvider::new(&base_url);
        assert!(!p.is_functional().await);
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
    async fn default_and_env_resolution() {
        let p = CursorLMProvider::new("");
        assert_eq!(p.base_url, DEFAULT_IDE_BRIDGE_URL);
        let p = CursorLMProvider::new("http://custom:1/");
        assert_eq!(p.base_url, "http://custom:1");
        std::env::set_var("THRESKI_LM_BRIDGE_URL", "http://env-bridge:9");
        let p = CursorLMProvider::new("");
        assert_eq!(p.base_url, "http://env-bridge:9");
        std::env::remove_var("THRESKI_LM_BRIDGE_URL");
    }
}
