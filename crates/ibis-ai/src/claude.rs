//! Anthropic Claude reasoning provider: Messages API with
//! `x-api-key` + `anthropic-version: 2023-06-01`, `system` pulled out of the
//! contents.

use async_trait::async_trait;

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig, Part};

const DEFAULT_MODEL: &str = "claude-sonnet-4";
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: usize = 4096;
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, serde::Serialize)]
struct ClaudeReq {
    model: String,
    max_tokens: usize,
    messages: Vec<ClaudeMsg>,
    #[serde(skip_serializing_if = "String::is_empty")]
    system: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ClaudeMsg {
    role: String,
    content: String,
}

#[derive(Debug, serde::Deserialize)]
struct ClaudeResp {
    #[serde(default)]
    content: Vec<ClaudeBlock>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaudeBlock {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    text: String,
}

/// Anthropic Messages API reasoning provider.
pub struct ClaudeProvider {
    api_key: String,
    model: String,
    endpoint: String,
    http: reqwest::Client,
}

impl ClaudeProvider {
    pub fn new(api_key: &str, model: &str) -> Self {
        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };
        ClaudeProvider {
            api_key: api_key.to_string(),
            model,
            endpoint: ENDPOINT.to_string(),
            http: http_client(HTTP_TIMEOUT),
        }
    }

    /// Endpoint override (test seam / proxy deployments; hardcoded the
    /// URL at ).
    pub fn with_endpoint(api_key: &str, model: &str, endpoint: &str) -> Self {
        let mut provider = ClaudeProvider::new(api_key, model);
        provider.endpoint = endpoint.trim_end_matches('/').to_string();
        provider
    }
}

#[async_trait]
impl AIProvider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
    }

    async fn is_functional(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        if !self.is_functional().await {
            return Err(AiError::NotConfigured {
                provider: "claude".to_string(),
            });
        }
        let max_tokens = if cfg.max_output_tokens == 0 {
            DEFAULT_MAX_TOKENS
        } else {
            cfg.max_output_tokens
        };
        let mut system = String::new();
        let mut msgs = Vec::with_capacity(contents.len());
        for c in contents {
            let text = c.joined_text();
            match c.role.as_str() {
                "system" => {
                    system = text;
                    continue;
                }
                "model" | "assistant" => msgs.push(ClaudeMsg {
                    role: "assistant".to_string(),
                    content: text,
                }),
                _ => msgs.push(ClaudeMsg {
                    role: "user".to_string(),
                    content: text,
                }),
            }
        }
        let body = serde_json::to_string(&ClaudeReq {
            model: self.model.clone(),
            max_tokens,
            messages: msgs,
            system,
        })
        .map_err(|e| AiError::other(e.to_string()))?;

        let resp = self
            .http
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(AiError::other(format!(
                "claude status {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        let parsed: ClaudeResp =
            serde_json::from_slice(&raw).map_err(|e| AiError::other(e.to_string()))?;
        let text: String = parsed
            .content
            .iter()
            .filter(|b| b.r#type == "text")
            .map(|b| b.text.as_str())
            .collect();
        Ok(Candidate {
            content: Content {
                role: "model".to_string(),
                parts: vec![Part { text }],
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
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;

    struct Captured {
        headers: HeaderMap,
        body: serde_json::Value,
    }

    async fn capture_server() -> (String, Arc<std::sync::Mutex<Vec<Captured>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let headers = req.headers().clone();
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    captured.lock().unwrap().push(Captured {
                        headers,
                        body: serde_json::from_slice(&body).unwrap(),
                    });
                    ok(
                        r#"{"content":[{"type":"text","text":"hello "},{"type":"tool_use","text":"skip"},{"type":"text","text":"world"}]}"#,
                    )
                }
            }),
        );
        (serve(app).await, captured)
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
    async fn wire_contract_system_extraction_and_roles() {
        let (base_url, captured) = capture_server().await;
        let p = ClaudeProvider::with_endpoint("sk-ant", "", &format!("{base_url}/v1/messages"));
        assert!(p.is_functional().await);
        let cand = p
            .generate_content(
                &[
                    Content {
                        role: "system".into(),
                        parts: vec![Part {
                            text: "be nice".into(),
                        }],
                    },
                    Content {
                        role: "model".into(),
                        parts: vec![Part { text: "hi".into() }],
                    },
                    Content {
                        role: String::new(),
                        parts: vec![Part { text: "q".into() }],
                    },
                ],
                GenerationConfig::default(),
            )
            .await
            .unwrap();

        assert_eq!(cand.content.parts[0].text, "hello world");
        assert_eq!(cand.content.role, "model");
        assert_eq!(cand.finish_reason, "STOP");

        let c = captured.lock().unwrap();
        assert_eq!(c[0].headers.get("x-api-key").unwrap(), "sk-ant");
        assert_eq!(c[0].headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(c[0].body["model"], "claude-sonnet-4");
        assert_eq!(c[0].body["max_tokens"], 4096);
        assert_eq!(c[0].body["system"], "be nice");
        assert_eq!(c[0].body["messages"][0]["role"], "assistant");
        assert_eq!(c[0].body["messages"][1]["role"], "user");
        assert!(c[0].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system"));
    }

    #[tokio::test]
    async fn max_tokens_override_and_default_model() {
        let (base_url, captured) = capture_server().await;
        let p =
            ClaudeProvider::with_endpoint("k", "claude-opus-9", &format!("{base_url}/v1/messages"));
        p.generate_content(
            &[],
            GenerationConfig {
                max_output_tokens: 128,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let c = captured.lock().unwrap();
        assert_eq!(c[0].body["max_tokens"], 128);
        assert_eq!(c[0].body["model"], "claude-opus-9");
    }

    #[tokio::test]
    async fn error_status_carries_body() {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async {
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body("bad key".to_string())
                    .unwrap()
                    .into_response()
            }),
        );
        let base_url = serve(app).await;
        let p = ClaudeProvider::with_endpoint("k", "", &format!("{base_url}/v1/messages"));
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "claude status 401: bad key");
    }

    #[tokio::test]
    async fn unconfigured_provider_rejected() {
        let p = ClaudeProvider::new("  ", "");
        assert!(!p.is_functional().await);
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "claude not configured");
    }
}
