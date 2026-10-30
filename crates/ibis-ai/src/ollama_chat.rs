//! Local Ollama chat reasoning provider:
//! `POST {url}/api/chat` with `stream:false` and
//! `options{temperature,num_predict}`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig, Part};

const DEFAULT_API_URL: &str = "http://127.0.0.1:11434";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    options: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    message: ChatMessage,
}

/// `ReasoningProvider` via Ollama `/api/chat`.
pub struct OllamaChatProvider {
    model: String,
    api_url: String,
    http: reqwest::Client,
}

impl OllamaChatProvider {
    pub fn new(model: &str, api_url: &str) -> Self {
        let api_url = if api_url.is_empty() {
            DEFAULT_API_URL.to_string()
        } else {
            api_url.to_string()
        };
        OllamaChatProvider {
            model: model.to_string(),
            api_url,
            http: http_client(HTTP_TIMEOUT),
        }
    }
}

#[async_trait]
impl AIProvider for OllamaChatProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn is_functional(&self) -> bool {
        !self.api_url.is_empty() && !self.model.is_empty()
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let mut msgs = Vec::with_capacity(contents.len());
        for c in contents {
            let role = if c.role.is_empty() {
                "user"
            } else {
                c.role.as_str()
            };
            msgs.push(ChatMessage {
                role: role.to_string(),
                content: c.joined_text(),
            });
        }
        let mut options = BTreeMap::new();
        if cfg.temperature > 0.0 {
            options.insert(
                "temperature".to_string(),
                serde_json::json!(cfg.temperature),
            );
        }
        if cfg.max_output_tokens > 0 {
            options.insert(
                "num_predict".to_string(),
                serde_json::json!(cfg.max_output_tokens),
            );
        }
        let body = serde_json::to_string(&ChatRequest {
            model: self.model.clone(),
            messages: msgs,
            stream: false,
            options,
        })
        .map_err(|e| AiError::other(e.to_string()))?;
        let url = format!("{}/api/chat", self.api_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| AiError::other(format!("ollama chat: {e}")))?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await?;
        if status != 200 {
            return Err(AiError::other(format!(
                "ollama chat status {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        let parsed: ChatResponse =
            serde_json::from_slice(&raw).map_err(|e| AiError::other(e.to_string()))?;
        Ok(Candidate {
            content: Content {
                role: "model".to_string(),
                parts: vec![Part {
                    text: parsed.message.content,
                }],
            },
            finish_reason: "STOP".to_string(),
            usage_metadata: None,
        })
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

    fn ok(body: &str) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body.to_string())
            .unwrap()
            .into_response()
    }

    #[tokio::test]
    async fn wire_contract_stream_false_and_options() {
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/api/chat",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().unwrap() = String::from_utf8_lossy(&body).into_owned();
                    ok(
                        r#"{"model":"m","message":{"role":"assistant","content":"answer"},"done":true}"#,
                    )
                }
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaChatProvider::new("qwen2.5-coder:7b", &base_url);
        assert!(p.is_functional().await);
        let cand = p
            .generate_content(
                &[
                    Content {
                        role: String::new(),
                        parts: vec![Part { text: "q".into() }],
                    },
                    Content {
                        role: "model".into(),
                        parts: vec![Part {
                            text: "prev".into(),
                        }],
                    },
                ],
                GenerationConfig {
                    temperature: 0.3,
                    max_output_tokens: 512,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cand.content.parts[0].text, "answer");
        assert_eq!(cand.content.role, "model");
        assert_eq!(cand.finish_reason, "STOP");

        let body: serde_json::Value =
            serde_json::from_str(captured.lock().unwrap().as_str()).unwrap();
        assert_eq!(body["model"], "qwen2.5-coder:7b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["temperature"], 0.3);
        assert_eq!(body["options"]["num_predict"], 512);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "model");
    }

    #[tokio::test]
    async fn zero_options_omitted() {
        let captured = Arc::new(std::sync::Mutex::new(String::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/api/chat",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    *captured.lock().unwrap() = String::from_utf8_lossy(&body).into_owned();
                    ok(r#"{"message":{"role":"assistant","content":""},"done":true}"#)
                }
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaChatProvider::new("m", &base_url);
        p.generate_content(&[], GenerationConfig::default())
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_str(captured.lock().unwrap().as_str()).unwrap();
        assert!(body.get("options").is_none());
    }

    #[tokio::test]
    async fn error_status_and_transport() {
        let app = Router::new().route(
            "/api/chat",
            post(|| async {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body("model missing".to_string())
                    .unwrap()
                    .into_response()
            }),
        );
        let base_url = serve(app).await;
        let p = OllamaChatProvider::new("m", &base_url);
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "ollama chat status 404: model missing");
    }

    #[tokio::test]
    async fn trailing_slash_trimmed_and_defaults() {
        let p = OllamaChatProvider::new("m", "");
        assert_eq!(p.api_url, "http://127.0.0.1:11434");
        assert!(p.is_functional().await);
        let p2 = OllamaChatProvider::new("m", "http://x:1/");
        assert_eq!(p2.api_url.trim_end_matches('/'), "http://x:1");
        let p3 = OllamaChatProvider::new("", "http://x:1");
        assert!(!p3.is_functional().await);
    }
}
