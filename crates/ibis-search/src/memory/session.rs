//! Session context assembly: the single bounded
//! payload a fresh agent session loads.

use super::{repo_record_id, Memory, Workstream};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// A compact pointer to useful non-memory graph context.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphHint {
    pub kind: String,
    pub subject: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub detail: String,
    #[serde(
        rename = "record_id",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub record_id: String,
}

/// The bounded payload a fresh agent session loads.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionContext {
    pub working_state: Option<Memory>,
    pub rules: Vec<Memory>,
    pub recent_failures: Vec<Memory>,
    pub top_memories: Vec<Memory>,
    pub decisions: Vec<Memory>,
    pub workstreams: Vec<Workstream>,
    pub graph_hints: Vec<GraphHint>,
}

impl SearchService {
    /// `LoadSessionContext: combines personal working state, team rules,
    /// recent memory/log failures, semantic memories, and active graph
    /// workstreams.
    pub async fn load_session_context(
        &self,
        actor_ctx: &crate::ActorContext,
        project_name: &str,
        branch: &str,
        actor: &str,
    ) -> Result<SessionContext> {
        let mut session = SessionContext::default();

        session.working_state = self
            .get_working_state(actor_ctx, project_name, branch, actor)
            .await
            .map_err(|e| SearchError::wrap("load working state", e))?;

        session.rules = self
            .get_rules(project_name, "", &[], 20)
            .await
            .map_err(|e| SearchError::wrap("load rules", e))?;

        session.recent_failures = self
            .recent_failures(&super::queries::RecentFailuresParams {
                project_name: project_name.to_string(),
                limit: 5,
                ..Default::default()
            })
            .await
            .map_err(|e| SearchError::wrap("load recent failures", e))?;

        session.top_memories = self
            .search_memory(
                actor_ctx,
                super::MemorySearchParams {
                    project_name: project_name.to_string(),
                    query: "current project decisions, constraints, designs, fixes, and important facts"
                        .to_string(),
                    kinds: ["decision", "constraint", "design", "fix", "gotcha", "fact"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    status: "current".to_string(),
                    limit: 5,
                    actor: actor.to_string(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SearchError::wrap("load top memories", e))?;

        session.decisions = self
            .search_memory(
                actor_ctx,
                super::MemorySearchParams {
                    project_name: project_name.to_string(),
                    query: "current project decisions, constraints, and designs".to_string(),
                    kinds: ["decision", "constraint", "design"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    status: "current".to_string(),
                    limit: 5,
                    actor: actor.to_string(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SearchError::wrap("load decisions", e))?;

        session.workstreams = self.active_workstreams(project_name, 5).await?;
        session.graph_hints = session_graph_hints(&session.recent_failures, &session.workstreams);
        Ok(session)
    }

    /// `activeWorkstreams (project literal is a sanitized identifier).
    async fn active_workstreams(
        &self,
        project_name: &str,
        limit: usize,
    ) -> Result<Vec<Workstream>> {
        let db = self.require_db()?;
        let mut where_parts = vec!["status = 'active'".to_string()];
        let project = project_name.trim();
        if !project.is_empty() {
            where_parts.push(format!("project = {}", repo_record_id(project)));
        }
        let sql = format!(
            "SELECT id, name, status FROM workstream WHERE {} ORDER BY created_at DESC LIMIT {limit};",
            where_parts.join(" AND ")
        );
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("load active workstreams: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Ok(Vec::new());
        };
        let mut workstreams = Vec::new();
        for row in rows::object_rows(&rows_values) {
            workstreams.push(Workstream {
                id: rows::record_id(rows::field(row, "id")),
                name: rows::scalar_string(rows::field(row, "name")),
                status: rows::scalar_string(rows::field(row, "status")),
            });
        }
        Ok(workstreams)
    }
}

/// `sessionGraphHints.
pub(crate) fn session_graph_hints(
    failures: &[Memory],
    workstreams: &[Workstream],
) -> Vec<GraphHint> {
    let mut hints = Vec::with_capacity(failures.len() + workstreams.len());
    for failure in failures {
        if !failure.id.starts_with("error_type:") {
            continue;
        }
        hints.push(GraphHint {
            kind: "recent_error".to_string(),
            subject: failure.title.clone(),
            detail: failure.content.clone(),
            record_id: failure.id.clone(),
        });
    }
    for workstream in workstreams {
        hints.push(GraphHint {
            kind: "active_workstream".to_string(),
            subject: workstream.name.clone(),
            detail: String::new(),
            record_id: workstream.id.clone(),
        });
    }
    hints
}
