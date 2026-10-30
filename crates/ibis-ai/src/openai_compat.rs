//! OpenAI-compatible chat reasoning provider.

use async_trait::async_trait;

use crate::error::AiError;
use crate::runner::http_client;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig, Part};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, serde::Serialize)]
struct ChatReq {
    model: String,
    messages: Vec<ChatMsg>,
    #[serde(skip_serializing_if = "is_zero_f64")]
    temperature: f64,
    #[serde(skip_serializing_if = "is_zero_usize")]
    max_tokens: usize,
}

fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct ChatMsg {
    role: String,
    content: String,
}

#[derive(Debug, serde::Deserialize)]
struct ChatResp {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, serde::Deserialize)]
struct ChatChoice {
    #[serde(default)]
    message: ChatMsg,
}

/// Implements `ReasoningProvider` for OpenAI-compatible chat APIs:
/// `POST {base}/chat/completions` with `Authorization: Bearer`.
pub struct OpenAICompatProvider {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl OpenAICompatProvider {
    /// Creates a provider; `base_url` should include `/v1` or the full prefix.
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let base_url = if base_url.is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            base_url.to_string()
        };
        OpenAICompatProvider {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            http: http_client(HTTP_TIMEOUT),
        }
    }
}

#[async_trait]
impl AIProvider for OpenAICompatProvider {
    fn name(&self) -> &str {
        "openai_compat"
    }

    async fn is_functional(&self) -> bool {
        !self.api_key.trim().is_empty() && !self.model.trim().is_empty()
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        cfg: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        if !self.is_functional().await {
            return Err(AiError::NotConfigured {
                provider: "openai_compat".to_string(),
            });
        }
        let mut msgs = Vec::with_capacity(contents.len());
        for c in contents {
            let role = match c.role.as_str() {
                "" => "user",
                "model" => "assistant",
                other => other,
            };
            msgs.push(ChatMsg {
                role: role.to_string(),
                content: c.joined_text(),
            });
        }
        let body = serde_json::to_string(&ChatReq {
            model: self.model.clone(),
            messages: msgs,
            temperature: cfg.temperature,
            max_tokens: cfg.max_output_tokens,
        })
        .map_err(|e| AiError::other(e.to_string()))?;

        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(AiError::other(format!(
                "openai_compat status {status}: {}",
                String::from_utf8_lossy(&raw)
            )));
        }
        let parsed: ChatResp =
            serde_json::from_slice(&raw).map_err(|e| AiError::other(e.to_string()))?;
        let text = parsed
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();
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
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;

    struct Captured {
        path: String,
        auth: String,
        content_type: String,
        body: serde_json::Value,
    }

    async fn capture_server() -> (String, Arc<std::sync::Mutex<Vec<Captured>>>) {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_state = captured.clone();
        let app = Router::new().route(
            "/chat/completions",
            post(move |req: Request| {
                let captured = handler_state.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default();
                    let content_type = req
                        .headers()
                        .get("content-type")
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default();
                    let path = req.uri().path().to_string();
                    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                        .await
                        .unwrap()
                        .to_vec();
                    captured.lock().unwrap().push(Captured {
                        path,
                        auth,
                        content_type,
                        body: serde_json::from_slice(&body).unwrap(),
                    });
                    ok(r#"{"choices":[{"message":{"role":"assistant","content":"hi there"}}]}"#)
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
    async fn wire_contract_headers_and_role_mapping() {
        let (base_url, captured) = capture_server().await;
        let p = OpenAICompatProvider::new(&base_url, "sk-test", "gpt-x");
        assert!(p.is_functional().await);
        let cand = p
            .generate_content(
                &[
                    Content {
                        role: "model".into(),
                        parts: vec![Part { text: "ask".into() }],
                    },
                    Content {
                        role: String::new(),
                        parts: vec![Part { text: "q".into() }, Part { text: "r".into() }],
                    },
                    Content {
                        role: "system".into(),
                        parts: vec![Part { text: "sys".into() }],
                    },
                ],
                GenerationConfig {
                    temperature: 0.5,
                    max_output_tokens: 256,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(cand.content.role, "model");
        assert_eq!(cand.content.parts[0].text, "hi there");
        assert_eq!(cand.finish_reason, "STOP");

        let c = captured.lock().unwrap();
        assert_eq!(c[0].path, "/chat/completions");
        assert_eq!(c[0].auth, "Bearer sk-test");
        assert_eq!(c[0].content_type, "application/json");
        assert_eq!(c[0].body["model"], "gpt-x");
        assert_eq!(c[0].body["temperature"], 0.5);
        assert_eq!(c[0].body["max_tokens"], 256);
        assert_eq!(c[0].body["messages"][0]["role"], "assistant");
        assert_eq!(c[0].body["messages"][1]["role"], "user");
        assert_eq!(c[0].body["messages"][1]["content"], "qr");
        assert_eq!(c[0].body["messages"][2]["role"], "system");
    }

    #[tokio::test]
    async fn zero_fields_are_omitted() {
        let (base_url, captured) = capture_server().await;
        let p = OpenAICompatProvider::new(&base_url, "sk", "m");
        p.generate_content(&[Content::text("x")], GenerationConfig::default())
            .await
            .unwrap();
        let c = captured.lock().unwrap();
        assert!(c[0].body.get("temperature").is_none());
        assert!(c[0].body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn error_status_carries_body() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body("boom".to_string())
                    .unwrap()
                    .into_response()
            }),
        );
        let base_url = serve(app).await;
        let p = OpenAICompatProvider::new(&base_url, "sk", "m");
        let err = p
            .generate_content(&[Content::text("x")], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "openai_compat status 500: boom");
    }

    #[tokio::test]
    async fn unconfigured_provider_rejected() {
        let p = OpenAICompatProvider::new("", " ", "m");
        assert!(!p.is_functional().await);
        let err = p
            .generate_content(&[], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "openai_compat not configured");
    }

    #[tokio::test]
    async fn default_base_url() {
        let p = OpenAICompatProvider::new("", "sk", "m");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        let p = OpenAICompatProvider::new("http://x/v1/", "sk", "m");
        assert_eq!(p.base_url, "http://x/v1");
    }
}
