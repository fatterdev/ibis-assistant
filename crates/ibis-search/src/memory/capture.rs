//! Raw observation capture: intentionally small
//! input surface; classification and consolidation happen separately.

use super::add::AddMemoryParams;
use crate::error::Result;
use crate::search::SearchService;

/// `CaptureObservationParams.
#[derive(Debug, Clone, Default)]
pub struct CaptureObservationParams {
    pub project_name: String,
    pub text: String,
    pub source: String,
    pub session_id: String,
    pub owner: String,
}

impl SearchService {
    /// `CaptureObservation: records a raw, reusable observation with
    /// fixed lifecycle, visibility, confidence, and provenance fields.
    pub async fn capture_observation(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &CaptureObservationParams,
    ) -> Result<String> {
        let mut source = params.source.trim().to_lowercase();
        if source.is_empty() {
            source = "agent".to_string();
        }
        match source.as_str() {
            "agent" | "tool" | "user" => {}
            other => {
                return Err(crate::error::SearchError::other(format!(
                    "observation source must be agent, tool, or user, got {other:?}"
                )));
            }
        }

        let confidence = 0.5;
        self.add_memory(
            actor_ctx,
            &mut AddMemoryParams {
                project_name: params.project_name.clone(),
                kind: "observation".to_string(),
                content: params.text.clone(),
                status: "current".to_string(),
                visibility: "team".to_string(),
                confidence: Some(confidence),
                owner: params.owner.clone(),
                provenance_agent: source,
                provenance_session: params.session_id.clone(),
                provenance_tool: "capture_observation".to_string(),
                ..Default::default()
            },
        )
        .await
    }
}
