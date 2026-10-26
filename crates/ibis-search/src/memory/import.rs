//! Memory import: writes a batch of memories
//! (typically from [`super::export`]). Duplicates are matched by
//! `provenance.source_hash` when present, otherwise by exact content;
//! `on_conflict` is "skip" (default) or "supersede".
//!
//! Transfer paths supported: plugin-isolated → personal/server, personal →
//! server, server → personal/plugin (seed a local runtime from the team
//! store).

use super::add::AddMemoryParams;
use super::export::memory_content_hash;
use super::{first_non_empty, Memory};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Reports the outcome of one memory_import call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub superseded: usize,
}

impl SearchService {
    /// Writes a batch of memories. Every lookup binds
    /// its values (F-002).
    pub async fn memory_import(
        &self,
        actor_ctx: &crate::ActorContext,
        project_name: &str,
        items: &[Memory],
        on_conflict: &str,
        actor: &str,
    ) -> Result<MemoryImportResult> {
        let mut result = MemoryImportResult::default();
        self.require_db()?;
        let mut on_conflict = on_conflict.trim().to_lowercase();
        if on_conflict.is_empty() {
            on_conflict = "skip".to_string();
        }
        if on_conflict != "skip" && on_conflict != "supersede" {
            return Err(SearchError::other(format!(
                "on_conflict must be 'skip' or 'supersede', got {on_conflict:?}"
            )));
        }
        let resolved_actor = actor_ctx.resolve_actor(actor);

        for item in items {
            let content = item.content.trim().to_string();
            if content.is_empty() {
                result.skipped += 1;
                continue;
            }
            let mut source_hash = item.source_hash.trim().to_string();
            if source_hash.is_empty() {
                source_hash = memory_content_hash(&content);
            }
            let existing_id = self
                .find_memory_conflict(project_name, &content, &source_hash)
                .await?;
            if !existing_id.is_empty() {
                if on_conflict == "supersede" {
                    self.add_memory(
                        actor_ctx,
                        &mut AddMemoryParams {
                            project_name: project_name.to_string(),
                            kind: item.kind.clone(),
                            title: item.title.clone(),
                            content,
                            tags: item.tags.clone(),
                            visibility: {
                                let v = first_non_empty(&[&item.visibility, "team"]);
                                if v.is_empty() {
                                    "team".to_string()
                                } else {
                                    v
                                }
                            },
                            owner: resolved_actor.clone(),
                            branch: item.branch.clone(),
                            supersedes: existing_id,
                            provenance_tool: "memory_import".to_string(),
                            provenance_imported_from: first_non_empty(&[&item.id, "export"]),
                            provenance_source_hash: source_hash,
                            ..Default::default()
                        },
                    )
                    .await?;
                    result.superseded += 1;
                    continue;
                }
                result.skipped += 1;
                continue;
            }

            self.add_memory(
                actor_ctx,
                &mut AddMemoryParams {
                    project_name: project_name.to_string(),
                    kind: item.kind.clone(),
                    title: item.title.clone(),
                    content,
                    tags: item.tags.clone(),
                    visibility: {
                        let v = first_non_empty(&[&item.visibility, "team"]);
                        if v.is_empty() {
                            "team".to_string()
                        } else {
                            v
                        }
                    },
                    owner: resolved_actor.clone(),
                    branch: item.branch.clone(),
                    provenance_tool: "memory_import".to_string(),
                    provenance_imported_from: first_non_empty(&[&item.id, "export"]),
                    provenance_source_hash: source_hash,
                    ..Default::default()
                },
            )
            .await?;
            result.imported += 1;
        }
        Ok(result)
    }

    /// Finds an existing conflicting memory id.
    async fn find_memory_conflict(
        &self,
        project_name: &str,
        content: &str,
        source_hash: &str,
    ) -> Result<String> {
        if !source_hash.is_empty() {
            let id = self
                .find_memory_by_source_hash(project_name, source_hash)
                .await?;
            if !id.is_empty() {
                return Ok(id);
            }
        }
        self.find_memory_by_content(project_name, content).await
    }

    async fn find_memory_by_source_hash(
        &self,
        project_name: &str,
        source_hash: &str,
    ) -> Result<String> {
        let db = self.require_db()?;
        let repo_id = super::repo_record_id(project_name);
        let mut where_parts = vec!["provenance.source_hash = $hash".to_string()];
        if !repo_id.is_empty() {
            where_parts.push(format!("project = {repo_id}"));
        }
        let sql = format!(
            "SELECT id FROM {} WHERE {} LIMIT 1;",
            super::TABLE_MEMORY,
            where_parts.join(" AND ")
        );
        let mut vars = ibis_db::executor::Vars::new();
        vars.insert("hash".into(), ibis_db::value::Value::from_t(source_hash));
        first_memory_id(db, &sql, vars).await
    }

    async fn find_memory_by_content(&self, project_name: &str, content: &str) -> Result<String> {
        let db = self.require_db()?;
        let repo_id = super::repo_record_id(project_name);
        let mut where_parts = vec!["content = $content".to_string()];
        if !repo_id.is_empty() {
            where_parts.push(format!("project = {repo_id}"));
        }
        let sql = format!(
            "SELECT id FROM {} WHERE {} LIMIT 1;",
            super::TABLE_MEMORY,
            where_parts.join(" AND ")
        );
        let mut vars = ibis_db::executor::Vars::new();
        vars.insert("content".into(), ibis_db::value::Value::from_t(content));
        first_memory_id(db, &sql, vars).await
    }
}

/// `firstMemoryID.
async fn first_memory_id(
    db: &std::sync::Arc<dyn ibis_db::Executor>,
    sql: &str,
    vars: ibis_db::executor::Vars,
) -> Result<String> {
    let outcome = db
        .smart_query(sql, vars)
        .await
        .map_err(|e| SearchError::other(e.to_string()))?;
    let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
    let first = rows::object_rows(&rows_values).into_iter().next();
    Ok(first
        .map(|row| rows::record_id(rows::field(row, "id")))
        .unwrap_or_default())
}
