//! F0.5 MCP sampling session registry: the bridge between the runtime's
//! [`SamplingTransport`] seam and the *current* MCP client session.
//!
//! Tool dispatch attaches the session peer ([`rmcp::service::Peer`]); the
//! `mcp_sampling` reasoning provider then sends `sampling/createMessage`
//! over that session — the IDE answers with its own model (VS Code declares
//! the `sampling` capability). Reasoning outside a tool call (background
//! lanes) finds no attached peer and reports disconnected, letting the F0.5
//! chain (`sampling → lm_bridge → ollama`) move to the next step.
//!
//! Concurrency note: the registry keeps the most recently attached peer —
//! exact for the one-session stdio/plugin transport; concurrent Streamable
//! HTTP sessions share the slot (the last tool caller wins) — acceptable for
//! reasoning, which is best-effort per request.

use std::sync::Mutex;

use async_trait::async_trait;
use rmcp::service::Peer;
use rmcp::RoleServer;

use threski_ai::error::AiError;
use threski_ai::{SamplingRequest, SamplingResponse, SamplingTransport};

#[cfg(test)]
use threski_ai::SamplingMessage;

/// Holds the live session peer; implements the [`SamplingTransport`] seam.
pub struct SamplingSessionRegistry {
    peer: Mutex<Option<Peer<RoleServer>>>,
}

impl SamplingSessionRegistry {
    pub fn new() -> Self {
        SamplingSessionRegistry {
            peer: Mutex::new(None),
        }
    }

    /// Records the peer of the in-flight request (called by tool dispatch).
    pub fn attach(&self, peer: &Peer<RoleServer>) {
        *self.peer.lock().unwrap() = Some(peer.clone());
    }

    /// Drops the recorded peer (session teardown).
    pub fn detach(&self) {
        *self.peer.lock().unwrap() = None;
    }

    fn current(&self) -> Option<Peer<RoleServer>> {
        self.peer.lock().unwrap().clone()
    }

    /// True when the attached client negotiated the `sampling` capability.
    #[allow(deprecated)]
    fn client_declares_sampling(peer: &Peer<RoleServer>) -> bool {
        peer.peer_info()
            .is_some_and(|info| info.capabilities.sampling.is_some())
    }
}

impl Default for SamplingSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SamplingTransport for SamplingSessionRegistry {
    fn connected(&self) -> bool {
        self.current()
            .is_some_and(|p| Self::client_declares_sampling(&p))
    }

    async fn create_message(&self, request: SamplingRequest) -> Result<SamplingResponse, AiError> {
        let peer = self
            .current()
            .ok_or_else(|| AiError::other("mcp sampling: no client session attached"))?;
        if !Self::client_declares_sampling(&peer) {
            return Err(AiError::other(
                "mcp sampling: client did not declare the sampling capability",
            ));
        }
        let params = to_create_message_params(request);
        #[allow(deprecated)]
        let result = peer
            .create_message(params)
            .await
            .map_err(|e| AiError::other(format!("mcp sampling: {e}")))?;
        Ok(from_create_message_result(result))
    }
}

/// Maps the provider request onto `CreateMessageRequestParams`
/// (`sampling/createMessage`; roles must be `user`/`assistant` per schema).
#[allow(deprecated)]
fn to_create_message_params(request: SamplingRequest) -> rmcp::model::CreateMessageRequestParams {
    let messages = request
        .messages
        .into_iter()
        .map(|m| {
            let role = if m.role == "assistant" {
                rmcp::model::Role::Assistant
            } else {
                rmcp::model::Role::User
            };
            rmcp::model::SamplingMessage::new(
                role,
                rmcp::model::SamplingMessageContentBlock::text(m.text),
            )
        })
        .collect();
    let mut params = rmcp::model::CreateMessageRequestParams::new(messages, request.max_tokens);
    if let Some(system) = request.system_prompt {
        params = params.with_system_prompt(system);
    }
    if let Some(temperature) = request.temperature {
        params = params.with_temperature(temperature);
    }
    params
}

/// Maps `CreateMessageResult` back to the provider response: the assistant
/// message's text content joins (non-text content degrades to empty text —
/// the reasoning lanes are text-first).
#[allow(deprecated)]
fn from_create_message_result(result: rmcp::model::CreateMessageResult) -> SamplingResponse {
    let text = result
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            rmcp::model::SamplingMessageContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    SamplingResponse {
        role: match result.message.role {
            rmcp::model::Role::Assistant => "assistant",
            rmcp::model::Role::User => "user",
        }
        .to_string(),
        model: result.model,
        stop_reason: result.stop_reason,
        text,
    }
}

/// `SamplingMessage` → role/text helper used by the mapping test.
#[cfg(test)]
fn sampling_text(messages: &[SamplingMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|m| (m.role.clone(), m.text.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(deprecated)]
    fn text_of_message(message: &rmcp::model::SamplingMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                rmcp::model::SamplingMessageContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    #[allow(deprecated)]
    fn params_mapping_carries_roles_tokens_and_options() {
        let params = to_create_message_params(SamplingRequest {
            messages: vec![
                SamplingMessage {
                    role: "user".into(),
                    text: "why?".into(),
                },
                SamplingMessage {
                    role: "assistant".into(),
                    text: "because".into(),
                },
                SamplingMessage {
                    role: "model".into(),
                    text: "map to user".into(),
                },
            ],
            system_prompt: Some("be terse".into()),
            max_tokens: 512,
            temperature: Some(0.3),
        });
        assert_eq!(params.messages.len(), 3);
        assert_eq!(params.max_tokens, 512);
        assert_eq!(params.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(params.temperature, Some(0.3));
        let roles: Vec<_> = params.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                rmcp::model::Role::User,
                rmcp::model::Role::Assistant,
                rmcp::model::Role::User
            ]
        );
        let texts: Vec<String> = params
            .messages
            .iter()
            .map(text_of_message)
            .collect();
        assert_eq!(texts, vec!["why?", "because", "map to user"]);
    }

    #[test]
    #[allow(deprecated)]
    fn result_mapping_extracts_assistant_text_and_metadata() {
        let result = rmcp::model::CreateMessageResult::new(
            rmcp::model::SamplingMessage::new(
                rmcp::model::Role::Assistant,
                rmcp::model::SamplingMessageContentBlock::text("sampled answer"),
            ),
            "gpt-5-codex".to_string(),
        )
        .with_stop_reason(rmcp::model::CreateMessageResult::STOP_REASON_END_TURN.to_string());
        let response = from_create_message_result(result);
        assert_eq!(response.role, "assistant");
        assert_eq!(response.model, "gpt-5-codex");
        assert_eq!(response.stop_reason.as_deref(), Some("endTurn"));
        assert_eq!(response.text, "sampled answer");
        assert_eq!(
            sampling_text(&[SamplingMessage {
                role: "user".into(),
                text: "x".into()
            }]),
            vec![("user".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn disconnected_registry_reports_unattached() {
        let registry = SamplingSessionRegistry::new();
        assert!(!registry.connected());
        assert_eq!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(registry.create_message(SamplingRequest {
                    messages: vec![],
                    system_prompt: None,
                    max_tokens: 8,
                    temperature: None,
                }))
                .unwrap_err()
                .to_string(),
            "mcp sampling: no client session attached"
        );
    }
}