//! Design-note ingest: store a design document as a
//! `kind=design` memory with `derived_from` provenance links.

use super::add::AddMemoryParams;
use crate::error::{Result, SearchError};
use crate::search::SearchService;

/// Describes a design document or note to store as a kind=design memory
/// with optional derived_from provenance.
#[derive(Debug, Clone, Default)]
pub struct IngestDesignNoteParams {
    pub project_name: String,
    pub title: String,
    pub content: String,
    /// Optional local markdown/design file. When content is empty, the file
    /// is read from disk.
    pub path: String,
    pub source_refs: Vec<String>,
    pub tags: Vec<String>,
    pub visibility: String,
    pub actor: String,
}

impl SearchService {
    /// Stores a design document as kind=design and links `derived_from`
    /// edges to any safe source_refs (files, commits, issues, memories)
    ///
    pub async fn ingest_design_note(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &IngestDesignNoteParams,
    ) -> Result<String> {
        let mut content = params.content.trim().to_string();
        let mut title = params.title.trim().to_string();
        let path = params.path.trim().to_string();

        if content.is_empty() {
            if path.is_empty() {
                return Err(SearchError::other("design note requires content or path"));
            }
            content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| SearchError::other(format!("read design note: {e}")))?;
            if title.is_empty() {
                title = std::path::Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("design-note")
                    .to_string();
            }
        }
        if content.trim().is_empty() {
            return Err(SearchError::other("design note content is empty"));
        }
        if title.is_empty() {
            title = "Design note".to_string();
        }

        let mut tags = super::non_empty_strings(&params.tags);
        if !path.is_empty() {
            let rel = path.replace('\\', "/");
            if !super::contains_string(&tags, &format!("path:{rel}")) {
                tags.push(format!("path:{rel}"));
            }
            if !super::contains_string(&tags, "design-note") {
                tags.push("design-note".to_string());
            }
        } else if !super::contains_string(&tags, "design-note") {
            tags.push("design-note".to_string());
        }

        let mut refs: Vec<String> = Vec::with_capacity(params.source_refs.len());
        for r in &params.source_refs {
            let r = r.trim();
            if threski_db::ids::is_safe_record_id(r) {
                refs.push(r.to_string());
            }
        }

        let confidence = 0.8;
        self.add_memory(
            actor_ctx,
            &mut AddMemoryParams {
                project_name: params.project_name.clone(),
                kind: "design".to_string(),
                title,
                content,
                tags,
                confidence: Some(confidence),
                visibility: params.visibility.clone(),
                actor: params.actor.clone(),
                derived_from: refs,
                provenance_tool: "ingest_design_note".to_string(),
                ..Default::default()
            },
        )
        .await
    }
}
