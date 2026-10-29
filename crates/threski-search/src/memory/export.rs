//! Memory export: structured items + optional
//! markdown rendering; also backs the `.threski/memory` projection.

use sha2::{Digest, Sha256};

use super::{bind, first_non_empty, memory_from_row, repo_record_id, Memory, TABLE_MEMORY};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Export/projection filter arguments.
#[derive(Debug, Clone, Default)]
pub struct MemoryExportParams {
    pub project_name: String,
    pub kind: String,
    pub status: String,
    /// Default team (never dumps other owners' personal rows).
    pub visibility: String,
    /// "json" (default) | "markdown".
    pub format: String,
    pub actor: String,
}

impl SearchService {
    /// Returns matching memories as structured items and, when
    /// `format == "markdown"`, a rendered markdown document (one section per
    /// memory).
    ///
    /// Visibility defaults to team so exports and projections never include
    /// other owners' personal memories. Callers may pass
    /// `visibility=personal|all` together with a trusted actor to include
    /// their own personal rows.
    pub async fn memory_export(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &MemoryExportParams,
    ) -> Result<(Vec<Memory>, String)> {
        let db = self.require_db()?;
        let mut status = params.status.trim().to_string();
        if status.is_empty() {
            status = "current".to_string();
        }
        let mut visibility = params.visibility.trim().to_string();
        if visibility.is_empty() {
            visibility = "team".to_string();
        }
        let (vis_filter, mut bindings) = super::memory_visibility_filter(
            &visibility,
            &actor_ctx.resolve_trusted_actor(&params.actor),
        )?;
        let mut where_parts = vec!["status = $status".to_string(), vis_filter];
        bindings.push((
            "status".into(),
            threski_db::value::Value::from_t(status.as_str()),
        ));
        let kind = params.kind.trim();
        if !kind.is_empty() {
            where_parts.push("kind = $kind".to_string());
            bindings.push(("kind".into(), threski_db::value::Value::from_t(kind)));
        }
        let repo_id = repo_record_id(&params.project_name);
        if !repo_id.is_empty() {
            where_parts.push(format!("project = {repo_id}"));
        }
        let sql = format!(
            "SELECT id, kind, title, content, status, confidence, visibility, owner, tags, branch, created_at, updated_at, provenance.source_hash AS source_hash FROM {TABLE_MEMORY} WHERE {} ORDER BY updated_at DESC;",
            where_parts.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("memory export failed: {e}")))?;
        let mut items =
            memories_from_export_rows(&rows::normalize_rows(&outcome).unwrap_or_default());
        for item in items.iter_mut() {
            if item.source_hash.trim().is_empty() && !item.content.trim().is_empty() {
                item.source_hash = memory_content_hash(&item.content);
            }
        }

        if !params
            .format
            .trim()
            .to_lowercase()
            .eq_ignore_ascii_case("markdown")
        {
            return Ok((items, String::new()));
        }
        let markdown = render_memory_markdown(&items);
        Ok((items, markdown))
    }
}

/// `memoriesFromExportRows: tolerant decode + `source_hash` pickup.
pub(crate) fn memories_from_export_rows(rows_values: &[threski_db::value::Value]) -> Vec<Memory> {
    rows::object_rows(rows_values)
        .iter()
        .map(|row| {
            let mut m = memory_from_row(row);
            let hash = rows::scalar_string(rows::field(row, "source_hash"));
            if !hash.trim().is_empty() {
                m.source_hash = hash;
            }
            m
        })
        .collect()
}

/// `renderMemoryMarkdown.
pub(crate) fn render_memory_markdown(items: &[Memory]) -> String {
    let mut out = String::new();
    for m in items {
        out.push_str(&format!("## {}\n\n", first_non_empty(&[&m.title, &m.id])));
        out.push_str(&format!(
            "- kind: {}\n- status: {}\n- id: {}\n\n",
            m.kind, m.status, m.id
        ));
        out.push_str(&m.content);
        out.push_str("\n\n");
    }
    out
}

/// `memoryContentHash: `sha256:<hex>` of the content body.
pub(crate) fn memory_content_hash(content: &str) -> String {
    let sum = Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(sum.len() * 2);
    for byte in sum {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256:{hex}")
}
