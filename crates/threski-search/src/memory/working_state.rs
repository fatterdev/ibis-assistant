//! Personal working state: a deterministic,
//! latest-wins personal memory per (project, branch, owner).

use sha2::{Digest, Sha256};

use super::{
    memory_from_row, repo_record_id, temporal::memory_currentness_sql, Memory, TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// `workingStateID: deterministic `memory:⟨ws_<sha256[:16]>⟩`.
pub(crate) fn working_state_id(project_name: &str, branch: &str, owner: &str) -> String {
    let sum = Sha256::digest(format!("{project_name}\x00{branch}\x00{owner}").as_bytes());
    let hex: String = sum.iter().take(16).map(|b| format!("{b:02x}")).collect();
    threski_db::ids::format_record_id(TABLE_MEMORY, &format!("ws_{hex}"))
}

impl SearchService {
    /// `SetWorkingState: upserts the latest personal state for a
    /// project, branch and owner. Its deterministic record ID makes repeated
    /// calls latest-wins. Content/owner/branch bind (F-002); the id,
    /// project literal and code-controlled enum values stay inline.
    pub async fn set_working_state(
        &self,
        actor_ctx: &crate::ActorContext,
        project_name: &str,
        branch: &str,
        content: &str,
        owner: &str,
    ) -> Result<String> {
        let db = self.require_db()?;
        if content.trim().is_empty() {
            return Err(SearchError::other("working-state content is required"));
        }
        if project_name.trim().is_empty() {
            return Err(SearchError::other("project_name is required"));
        }
        let actor = actor_ctx.resolve_trusted_actor(owner);
        if actor.is_empty() {
            return Err(SearchError::other(
                "working state requires a trusted actor identity",
            ));
        }
        let id = working_state_id(project_name, branch, &actor);
        let repo_id = repo_record_id(project_name);
        let sql = format!(
            "UPSERT {id} SET kind = 'working_state', status = 'current', visibility = 'personal', confidence = 0.9, content = $content, owner = $owner, branch = $branch, project = {repo_id}, updated_at = time::now(), created_at = (created_at OR time::now());"
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("content".into(), threski_db::value::Value::from_t(content));
        vars.insert(
            "owner".into(),
            threski_db::value::Value::from_t(actor.as_str()),
        );
        vars.insert("branch".into(), threski_db::value::Value::from_t(branch));
        db.smart_query(&sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("set working state: {e}")))?;
        Ok(id)
    }

    /// `GetWorkingState: returns the caller-owned state for a project
    /// and branch; missing state is `None`.
    pub async fn get_working_state(
        &self,
        actor_ctx: &crate::ActorContext,
        project_name: &str,
        branch: &str,
        actor: &str,
    ) -> Result<Option<Memory>> {
        let db = self.require_db()?;
        let owner = actor_ctx.resolve_trusted_actor(actor);
        if owner.is_empty() {
            return Ok(None);
        }
        let id = working_state_id(project_name, branch, &owner);
        let sql = format!(
            "SELECT * FROM {id} WHERE kind = 'working_state' AND visibility = 'personal' AND owner = $owner AND {};",
            memory_currentness_sql()
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert(
            "owner".into(),
            threski_db::value::Value::from_t(owner.as_str()),
        );
        let outcome = db
            .smart_query(&sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("get working state: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Ok(None);
        };
        let objects = rows::object_rows(&rows_values);
        let Some(row) = objects.first() else {
            return Ok(None);
        };
        if !rows_values.first().map(rows::is_object).unwrap_or(false) {
            return Ok(None);
        }
        let memory = memory_from_row(row);
        if memory.kind != "working_state"
            || memory.owner != owner
            || memory.visibility != "personal"
        {
            return Ok(None);
        }
        Ok(Some(memory))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_latest_wins_id() {
        let a = working_state_id("threski", "feat/memory-absorption", "alice");
        let b = working_state_id("threski", "feat/memory-absorption", "alice");
        assert_eq!(a, b);
        let dash = working_state_id("threski", "feat-memory-absorption", "alice");
        assert_ne!(a, dash);
        assert!(a.starts_with("memory:⟨ws_"));
    }
}
