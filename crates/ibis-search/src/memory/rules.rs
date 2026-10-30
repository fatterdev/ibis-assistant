//! Rule memories: store + list team rules.
//!
//! Port delta (F-002): interpolated title/content/owner/branch through
//! `db.EscapeSQL`; the Rust port binds every content-bearing value.

use ibis_db::value::Value;

use super::{
    bind, repo_record_id, temporal::memory_currentness_sql, Bindings, Memory, ES_HAS_MEMORY,
    TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

impl SearchService {
    /// Creates a current team-visible rule memory. Returns
    /// the created record id.
    // Argument list mirrors the original MCP tool surface 1:1 (see kdiff note).
    #[allow(clippy::too_many_arguments)]
    pub async fn store_rule(
        &self,
        actor_ctx: &crate::ActorContext,
        project_name: &str,
        title: &str,
        content: &str,
        tags: &[String],
        branch: &str,
        owner: &str,
    ) -> Result<String> {
        let db = self.require_db()?;
        if content.trim().is_empty() {
            return Err(SearchError::other("rule content is required"));
        }
        if project_name.trim().is_empty() {
            return Err(SearchError::other("project_name is required"));
        }
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;
        let vec = ai
            .embed_text(content)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;

        let actor = actor_ctx.resolve_actor(owner);
        let repo_id = repo_record_id(project_name);
        let id = new_rule_memory_id();
        let bindings: Bindings = vec![
            ("repo_name".into(), Value::from_t(project_name)),
            ("title".into(), Value::from_t(title)),
            ("content".into(), Value::from_t(content)),
            ("owner".into(), Value::from_t(actor.as_str())),
            (
                "tags".into(),
                Value::from_t(
                    tags.iter()
                        .map(|t| Value::from_t(t.as_str()))
                        .collect::<Vec<Value>>(),
                ),
            ),
            ("branch".into(), Value::from_t(branch)),
            ("embedding".into(), crate::commit_meaning::vec_value(&vec)),
        ];
        let mut query = format!("BEGIN TRANSACTION;\nUPSERT {repo_id} SET name = $repo_name;\nLET $mem = CREATE {id} SET kind = 'rule', title = $title, content = $content, status = 'current', confidence = 0.7, visibility = 'team', owner = $owner, tags = $tags, branch = $branch, project = {repo_id}, valid_from = time::now(), embedding = $embedding, created_at = time::now(), updated_at = time::now();\n");
        query.push_str(&format!("RELATE {repo_id}->{ES_HAS_MEMORY}->$mem;\n"));
        query.push_str("COMMIT TRANSACTION;");
        db.smart_query(&query, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("store rule: {e}")))?;
        Ok(id)
    }

    /// Returns current team-visible rules. Empty queries use deterministic
    /// recency ordering; non-empty queries use semantic memory search
    ///
    pub async fn get_rules(
        &self,
        project_name: &str,
        query: &str,
        tags: &[String],
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let mut limit = limit;
        if limit == 0 {
            limit = 50;
        }
        if !query.trim().is_empty() {
            return self
                .search_memory(
                    &crate::ActorContext::LOCAL,
                    super::MemorySearchParams {
                        project_name: project_name.to_string(),
                        query: query.to_string(),
                        kinds: vec!["rule".to_string()],
                        status: "current".to_string(),
                        visibility: "team".to_string(),
                        tags: tags.to_vec(),
                        limit,
                        actor: String::new(),
                    },
                )
                .await;
        }
        let db = self.require_db()?;

        let mut where_parts = vec![
            "kind IN ['rule']".to_string(),
            memory_currentness_sql(),
            "visibility = 'team'".to_string(),
        ];
        let mut bindings: Bindings = Vec::new();
        let project = project_name.trim();
        if !project.is_empty() {
            where_parts.push(format!("project = {}", repo_record_id(project)));
        }
        let clean_tags = super::non_empty_strings(tags);
        if !clean_tags.is_empty() {
            where_parts.push("tags CONTAINSANY $tags".to_string());
            bindings.push((
                "tags".into(),
                Value::from_t(
                    clean_tags
                        .iter()
                        .map(|t| Value::from_t(t.as_str()))
                        .collect::<Vec<Value>>(),
                ),
            ));
        }
        let sql = format!(
            "SELECT * FROM {TABLE_MEMORY} WHERE {} ORDER BY updated_at DESC LIMIT {limit};",
            where_parts.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("get rules: {e}")))?;
        Ok(super::memories_from_rows(
            &rows::normalize_rows(&outcome).unwrap_or_default(),
        ))
    }
}

/// `newRuleMemoryID: random `memory:⟨rule_<16 hex bytes>⟩` (crypto/rand;
/// the uuid crate's v4 generator is the Rust OS-CSPRNG source here).
fn new_rule_memory_id() -> String {
    let random = *uuid::Uuid::new_v4().as_bytes();
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    ibis_db::ids::format_record_id(TABLE_MEMORY, &format!("rule_{hex}"))
}
