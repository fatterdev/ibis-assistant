//! MCP sampling provider (F0.5): reasons through the client's IDE model by
//! sending `sampling/createMessage` on the current MCP session
//! (`SamplingTransport`, implemented by [`crate::mcp_sampling`] consumers in
//! ibis-server over the rmcp `Peer<RoleServer>`).
//!
//! Reasoning only — embeddings stay on Ollama. The provider is functional
//! while the session peer is connected; when it is not (no session yet,
//! client without the sampling capability, background work), the F0.5
//! fallback chain (`sampling → lm_bridge → ollama`) moves on to the next
//! step.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::traits::AIProvider;
use crate::types::{Candidate, Content, GenerationConfig};

/// `maxTokens` used when the generation request does not pin one.
pub const DEFAULT_SAMPLING_MAX_TOKENS: u32 = 1024;

/// One turn of the sampling conversation (`SamplingMessage` subset: the
/// text-content form the reasoning lanes produce).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplingMessage {
    /// `user` | `assistant`
    pub role: String,
    pub text: String,
}

/// The `sampling/createMessage` request subset the provider sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingRequest {
    pub messages: Vec<SamplingMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// The `sampling/createMessage` result subset the provider consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingResponse {
    /// `user` | `assistant`
    pub role: String,
    /// Model id reported by the client (e.g. the IDE model name).
    pub model: String,
    /// `endTurn` | `stopSequence` | `maxTokens` | `toolUse` | …
    pub stop_reason: Option<String>,
    pub text: String,
}

/// The bridge to the client-side sampling endpoint: `connected` is the
/// cheap, synchronous liveness probe (`is_functional`), `create_message`
/// performs the actual request.
#[async_trait]
pub trait SamplingTransport: Send + Sync {
    /// True while a sampling-capable MCP client session is attached.
    fn connected(&self) -> bool;

    async fn create_message(&self, request: SamplingRequest) -> Result<SamplingResponse, AiError>;
}

/// Reasons through the MCP client (`sampling/createMessage`) — the primary
/// reasoning via of plugin/IDE sessions.
pub struct McpSamplingProvider {
    transport: Arc<dyn SamplingTransport>,
    default_max_tokens: u32,
}

impl McpSamplingProvider {
    pub fn new(transport: Arc<dyn SamplingTransport>) -> Self {
        McpSamplingProvider {
            transport,
            default_max_tokens: DEFAULT_SAMPLING_MAX_TOKENS,
        }
    }

    /// Maps the unified content list onto sampling messages: `model` turns
    /// into `assistant`, everything else into `user`; parts join like the
    /// other providers do. Empty turns are dropped (the MCP schema requires
    /// a non-empty text content).
    fn to_sampling_messages(contents: &[Content]) -> Vec<SamplingMessage> {
        contents
            .iter()
            .filter(|c| !c.joined_text().trim().is_empty())
            .map(|c| SamplingMessage {
                role: if c.role == "model" { "assistant" } else { "user" }.to_string(),
                text: c.joined_text(),
            })
            .collect()
    }
}

#[async_trait]
impl AIProvider for McpSamplingProvider {
    fn name(&self) -> &str {
        "mcp_sampling"
    }

    async fn is_functional(&self) -> bool {
        self.transport.connected()
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        let max_tokens = if config.max_output_tokens == 0 {
            self.default_max_tokens
        } else {
            config.max_output_tokens.min(u32::MAX as usize) as u32
        };
        let request = SamplingRequest {
            messages: Self::to_sampling_messages(contents),
            system_prompt: None,
            max_tokens,
            temperature: (config.temperature != 0.0).then_some(config.temperature as f32),
        };
        if request.messages.is_empty() {
            return Err(AiError::other("mcp sampling: empty conversation"));
        }
        let response = self.transport.create_message(request).await?;
        Ok(Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![crate::types::Part { text: response.text }],
            },
            finish_reason: response.stop_reason.unwrap_or_else(|| "stop".to_string()),
            usage_metadata: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Part;
    use std::sync::Mutex;

    struct MockTransport {
        connected: bool,
        fail_with: Option<String>,
        seen: Mutex<Vec<SamplingRequest>>,
    }

    impl MockTransport {
        fn connected() -> Self {
            MockTransport {
                connected: true,
                fail_with: None,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn disconnected() -> Self {
            MockTransport {
                connected: false,
                fail_with: None,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SamplingTransport for MockTransport {
        fn connected(&self) -> bool {
            self.connected
        }

        async fn create_message(&self, request: SamplingRequest) -> Result<SamplingResponse, AiError> {
            self.seen.lock().unwrap().push(request);
            if let Some(err) = &self.fail_with {
                return Err(AiError::other(err.clone()));
            }
            Ok(SamplingResponse {
                role: "assistant".to_string(),
                model: "gpt-5-codex".to_string(),
                stop_reason: Some("endTurn".to_string()),
                text: "sampled reply".to_string(),
            })
        }
    }

    fn content(role: &str, text: &str) -> Content {
        Content {
            role: role.to_string(),
            parts: vec![Part { text: text.to_string() }],
        }
    }

    #[tokio::test]
    async fn maps_contents_to_sampling_messages_and_back() {
        let mock = Arc::new(MockTransport::connected());
        let p = McpSamplingProvider::new(mock.clone());
        assert_eq!(p.name(), "mcp_sampling");
        assert!(p.is_functional().await);

        let cand = p
            .generate_content(
                &[
                    content("user", "why did this change?"),
                    content("model", "because tests demanded it"),
                    content("user", "prove it"),
                ],
                GenerationConfig {
                    temperature: 0.2,
                    max_output_tokens: 2000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cand.content.role, "model");
        assert_eq!(cand.content.parts[0].text, "sampled reply");
        assert_eq!(cand.finish_reason, "endTurn");

        let seen = mock.seen.lock().unwrap();
        assert_eq!(
            seen[0].messages,
            vec![
                SamplingMessage {
                    role: "user".into(),
                    text: "why did this change?".into()
                },
                SamplingMessage {
                    role: "assistant".into(),
                    text: "because tests demanded it".into()
                },
                SamplingMessage {
                    role: "user".into(),
                    text: "prove it".into()
                },
            ]
        );
        assert_eq!(seen[0].max_tokens, 2000);
        assert_eq!(seen[0].temperature, Some(0.2));
    }

    #[tokio::test]
    async fn empty_generation_uses_default_max_tokens_and_no_temperature() {
        let mock = Arc::new(MockTransport::connected());
        let p = McpSamplingProvider::new(mock.clone());
        p.generate_content(
            &[content("user", "hi")],
            GenerationConfig::default(),
        )
        .await
        .unwrap();
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen[0].max_tokens, DEFAULT_SAMPLING_MAX_TOKENS);
        assert_eq!(seen[0].temperature, None);
        assert!(seen[0].system_prompt.is_none());
    }

    #[tokio::test]
    async fn empty_parts_and_blank_turns_are_dropped_and_empty_request_errors() {
        let mock = Arc::new(MockTransport::connected());
        let p = McpSamplingProvider::new(mock.clone());
        let err = p
            .generate_content(
                &[
                    Content {
                        role: "user".into(),
                        parts: vec![Part { text: "   ".into() }],
                    },
                    Content { role: String::new(), parts: vec![] },
                ],
                GenerationConfig::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "mcp sampling: empty conversation");
        assert_eq!(mock.seen.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn transport_errors_surface_verbatim() {
        let mut t = MockTransport::connected();
        t.fail_with = Some("client declined sampling".to_string());
        let p = McpSamplingProvider::new(Arc::new(t));
        let err = p
            .generate_content(&[content("user", "hi")], GenerationConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "client declined sampling");
    }

    #[tokio::test]
    async fn disconnected_transport_is_not_functional() {
        let p = McpSamplingProvider::new(Arc::new(MockTransport::disconnected()));
        assert!(!p.is_functional().await);
    }

    #[test]
    fn request_and_response_wire_shapes() {
        let req = SamplingRequest {
            messages: vec![SamplingMessage {
                role: "user".into(),
                text: "hi".into(),
            }],
            system_prompt: Some("be terse".into()),
            max_tokens: 64,
            temperature: Some(0.5),
        };
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["messages"][0]["role"], serde_json::json!("user"));
        assert_eq!(wire["messages"][0]["text"], serde_json::json!("hi"));
        assert_eq!(wire["systemPrompt"], serde_json::json!("be terse"));
        assert_eq!(wire["maxTokens"], serde_json::json!(64));
        assert_eq!(wire["temperature"], serde_json::json!(0.5));

        let res: SamplingResponse = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "model": "claude-sonnet-4",
            "stopReason": "endTurn",
            "text": "answer"
        }))
        .unwrap();
        assert_eq!(res.text, "answer");
        assert_eq!(res.stop_reason.as_deref(), Some("endTurn"));
    }
}