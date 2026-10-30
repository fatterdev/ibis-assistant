//! Typed-memory core. One file
//! ≈ one Rust module:
//!
//! | file | Rust module | Port group |
//! |---|---|---|
//! | | this module | 2 |
//! | | [`add`] | 2 |
//! | | [`queries`] | 2 |
//! | | [`gitbound`] | 2 |
//! | | [`lifecycle`] | 2 |
//! | | [`session`] | 2 |
//! | | [`working_state`] | 2 |
//! | | [`capture`] | 2 |
//! | | [`projection`] | 2 |
//! | | [`claim_store`] | 3 |
//! | | [`claim_notes`] | 3 |
//! | | [`claim_tree`] | 3 |
//! | | [`asof`] | 4 |
//! | | [`temporal`] | 1* (needed by the ask core) |
//! | | [`askref`] | 4 |
//! | | [`ancestry`] | 4 |
//! | | [`kdiff`] | 4 |
//! | | [`hotcache`] | 5 |
//! | | [`insights`] | 6 |
//! | | [`rules`] | 6 |
//! | | [`design`] | 6 |
//! | | [`export`] | 6 |
//! | | [`import`] | 6 |
//! | | [`share`] | 6 |

pub mod add;
pub mod ancestry;
pub mod askref;
pub mod asof;
pub mod capture;
pub mod claim_notes;
pub mod claim_store;
pub mod claim_tree;
pub mod design;
pub mod export;
pub mod gitbound;
pub mod hotcache;
pub mod import;
pub mod insights;
pub mod kdiff;
pub mod lifecycle;
pub mod projection;
pub mod queries;
pub mod rules;
pub mod session;
pub mod share;
pub mod temporal;
pub mod working_state;

use ibis_db::value::Object;

use crate::error::SearchError;
use crate::rows;

/// `memory` table name.
pub const TABLE_MEMORY: &str = "memory";
/// `store` marker: git-owned claim (repo is truth).
pub const STORE_GIT: &str = "git";
/// `store` marker: Surreal-only row.
pub const STORE_SURREAL: &str = "surreal";

/// Summary length for semantic memory hits.
pub(crate) const MEMORY_SEARCH_SUMMARY_RUNES: usize = 240;

/// The API-facing representation of a typed memory.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub title: String,
    pub content: String,
    pub status: String,
    pub confidence: f64,
    pub visibility: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub owner: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub branch: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub valid_from: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub valid_until: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub created_at: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub updated_at: String,
    #[serde(default)]
    pub score: f64,
    /// Claim SoT marker: `git` (derived from `.ibis/claims/`) or `surreal`.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub store: String,
    /// `provenance.source_hash` when present (export/import dedup).
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub source_hash: String,
}

/// `memoryFromRow: tolerant decode of any projected memory row.
pub(crate) fn memory_from_row(row: &Object) -> Memory {
    Memory {
        id: rows::record_id(rows::field(row, "id")),
        kind: rows::scalar_string(rows::field(row, "kind")),
        title: rows::scalar_string(rows::field(row, "title")),
        content: rows::scalar_string(rows::field(row, "content")),
        status: rows::scalar_string(rows::field(row, "status")),
        confidence: rows::numeric_float(rows::field(row, "confidence")),
        visibility: rows::scalar_string(rows::field(row, "visibility")),
        owner: rows::scalar_string(rows::field(row, "owner")),
        tags: rows::string_slice(rows::field(row, "tags")),
        branch: rows::scalar_string(rows::field(row, "branch")),
        valid_from: rows::scalar_string(rows::field(row, "valid_from")),
        valid_until: rows::scalar_string(rows::field(row, "valid_until")),
        created_at: rows::scalar_string(rows::field(row, "created_at")),
        updated_at: rows::scalar_string(rows::field(row, "updated_at")),
        score: rows::numeric_float(rows::field(row, "score")),
        store: rows::scalar_string(rows::field(row, "store")),
        source_hash: String::new(),
    }
}

/// `memoriesFromRows.
pub(crate) fn memories_from_rows(rows_values: &[ibis_db::value::Value]) -> Vec<Memory> {
    rows::object_rows(rows_values)
        .iter()
        .map(|row| memory_from_row(row))
        .collect()
}

/// `nonEmptyStrings: trimmed, non-empty filter.
pub(crate) fn non_empty_strings(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `shortenMemoryContent: rune-bounded summary.
pub(crate) fn shorten_memory_content(content: &str) -> String {
    let runes: Vec<char> = content.chars().collect();
    if runes.len() <= MEMORY_SEARCH_SUMMARY_RUNES {
        return content.to_string();
    }
    runes[..MEMORY_SEARCH_SUMMARY_RUNES].iter().collect()
}

/// `firstNonEmpty.
pub(crate) fn first_non_empty(values: &[&str]) -> String {
    for value in values {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// `repoRecordID: `repo:⟨sanitized⟩` for a project name.
pub(crate) fn repo_record_id(project_name: &str) -> String {
    if project_name.trim().is_empty() {
        return String::new();
    }
    ibis_db::ids::format_record_id("repo", &ibis_db::ids::sanitize_id(project_name))
}

/// Validates a memory record id.
pub(crate) fn require_memory_record_id(id: &str) -> crate::Result<()> {
    if !ibis_db::ids::is_safe_record_id(id) {
        return Err(crate::SearchError::other(format!(
            "invalid memory id: {id}"
        )));
    }
    let table = id.split(':').next().unwrap_or("");
    if table != TABLE_MEMORY {
        return Err(crate::SearchError::other(format!(
            "invalid memory id: {id} (expected {TABLE_MEMORY} table)"
        )));
    }
    Ok(())
}

/// `validateMemoryID.
pub(crate) fn validate_memory_id(id: &str) -> crate::Result<()> {
    require_memory_record_id(id)
}

/// `validateMemoryConfidence.
pub(crate) fn validate_memory_confidence(confidence: f64) -> crate::Result<()> {
    if !(0.0..=1.0).contains(&confidence) {
        return Err(crate::SearchError::other(format!(
            "confidence must be between 0.0 and 1.0, got {confidence}"
        )));
    }
    Ok(())
}

/// `memoryRecordKey: raw record key (`mem_...`) from a memory record id.
pub(crate) fn memory_record_key(id: &str) -> crate::Result<String> {
    require_memory_record_id(id)?;
    let key = id.strip_prefix(&format!("{TABLE_MEMORY}:")).unwrap_or(id);
    let key = key.trim_start_matches('⟨').trim_end_matches('⟩');
    Ok(key.to_string())
}

/// `firstLine: first line, rune-capped.
pub(crate) fn first_line(content: &str, max_runes: usize) -> String {
    let trimmed = content.trim();
    let line = trimmed.split('\n').next().unwrap_or("").trim();
    let runes: Vec<char> = line.chars().collect();
    if runes.len() > max_runes {
        runes[..max_runes].iter().collect()
    } else {
        line.to_string()
    }
}

/// `containsString.
pub(crate) fn contains_string(values: &[String], want: &str) -> bool {
    values.iter().any(|v| v == want)
}

/// Bound-variable pairs (F-002): SQL fragments may carry named placeholders
/// whose values ride the `Vars` map.
pub(crate) type Bindings = Vec<(String, ibis_db::value::Value)>;

pub(crate) fn bind(pairs: Bindings) -> ibis_db::executor::Vars {
    pairs.into_iter().collect()
}

/// `memoryVisibilityFilter: the visibility predicate for memory reads.
/// Actor names are user-controlled → bound as `$actor` (F-002).
pub(crate) fn memory_visibility_filter(
    visibility: &str,
    actor: &str,
) -> crate::Result<(String, Bindings)> {
    let actor = actor.trim();
    match visibility.trim().to_lowercase().as_str() {
        "team" => Ok(("visibility = 'team'".to_string(), Vec::new())),
        "personal" => Ok((
            "visibility = 'personal' AND owner = $actor".to_string(),
            vec![("actor".into(), ibis_db::value::Value::from_t(actor))],
        )),
        "" | "all" => {
            if actor.is_empty() {
                Ok(("visibility = 'team'".to_string(), Vec::new()))
            } else {
                Ok((
                    "(visibility = 'team' OR (visibility = 'personal' AND owner = $actor))"
                        .to_string(),
                    vec![("actor".into(), ibis_db::value::Value::from_t(actor))],
                ))
            }
        }
        other => Err(crate::SearchError::other(format!(
            "visibility must be team, personal, or all, got {other:?}"
        ))),
    }
}

/// `quotedList → bound kind filter (F-002: values bind as `$kinds`).
pub(crate) fn kinds_filter(values: &[String]) -> Option<(String, Bindings)> {
    let clean = non_empty_strings(values);
    if clean.is_empty() {
        return None;
    }
    Some((
        "kind IN $kinds".to_string(),
        vec![(
            "kinds".into(),
            ibis_db::value::Value::from_t(
                clean
                    .iter()
                    .map(|k| ibis_db::value::Value::from_t(k.as_str()))
                    .collect::<Vec<ibis_db::value::Value>>(),
            ),
        )],
    ))
}

/// `tags CONTAINSANY [...] → bound `$tags`.
pub(crate) fn tags_filter(values: &[String]) -> Option<(String, Bindings)> {
    let clean = non_empty_strings(values);
    if clean.is_empty() {
        return None;
    }
    Some((
        "tags CONTAINSANY $tags".to_string(),
        vec![(
            "tags".into(),
            ibis_db::value::Value::from_t(
                clean
                    .iter()
                    .map(|t| ibis_db::value::Value::from_t(t.as_str()))
                    .collect::<Vec<ibis_db::value::Value>>(),
            ),
        )],
    ))
}

/// A sparse memory patch. `None` fields remain
/// unchanged.
#[derive(Debug, Clone, Default)]
pub struct MemoryUpdateParams {
    pub id: String,
    pub content: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub confidence: Option<f64>,
    pub tags: Option<Vec<String>>,
    pub visibility: Option<String>,
    pub valid_until: Option<String>,
    pub actor: String,
}

/// Semantic-search and metadata filters.
#[derive(Debug, Clone, Default)]
pub struct MemorySearchParams {
    pub project_name: String,
    pub query: String,
    pub kinds: Vec<String>,
    pub status: String,
    pub visibility: String,
    pub tags: Vec<String>,
    pub limit: usize,
    pub actor: String,
}

/// Metadata filters for browsing without an embedding provider
///
#[derive(Debug, Clone, Default)]
pub struct MemoryListParams {
    pub kinds: Vec<String>,
    pub status: String,
    pub visibility: String,
    pub limit: usize,
    pub actor: String,
}

/// Short workstream projection.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Workstream {
    pub id: String,
    pub name: String,
    pub status: String,
}

/// A memory plus its graph context.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MemoryExpansion {
    pub memory: Memory,
    pub supersedes_chain: Vec<Memory>,
    pub derived_from: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub workstream: Option<Workstream>,
}

impl crate::search::SearchService {
    /// Applies a sparse patch without changing the memory identity
    /// Content-bearing values bind (F-002).
    pub async fn update_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        params: MemoryUpdateParams,
    ) -> crate::Result<()> {
        let db = self.require_db()?;
        require_memory_record_id(&params.id)?;
        let memory = self.memory_for_governance(&params.id).await?;
        if memory.store == STORE_GIT
            && (params.content.is_some()
                || params.title.is_some()
                || params.kind.is_some()
                || params.visibility.is_some())
        {
            return Err(crate::SearchError::other(format!(
                "git-owned claim {} is immutable via the database (repo is SoT); supersede it with a new committed claim in .ibis/claims/",
                memory.id
            )));
        }
        authorize_memory_mutation(&memory, &mutation_actor(actor_ctx, &memory, &params.actor))?;
        if let Some(confidence) = params.confidence {
            validate_memory_confidence(confidence)?;
        }

        let mut bindings: Bindings = Vec::new();
        let mut sets: Vec<String> = vec!["updated_at = time::now()".to_string()];
        if let Some(content) = &params.content {
            bindings.push((
                "content".into(),
                ibis_db::value::Value::from_t(content.as_str()),
            ));
            sets.push("content = $content".to_string());
            let embedding = self.memory_embedding(content, None).await?;
            bindings.push((
                "embedding".into(),
                crate::commit_meaning::f64_vec_value(&embedding),
            ));
            sets.push("embedding = $embedding".to_string());
        }
        if let Some(title) = &params.title {
            bindings.push((
                "title".into(),
                ibis_db::value::Value::from_t(title.as_str()),
            ));
            sets.push("title = $title".to_string());
        }
        if let Some(kind) = &params.kind {
            bindings.push(("kind".into(), ibis_db::value::Value::from_t(kind.as_str())));
            sets.push("kind = $kind".to_string());
        }
        if let Some(status) = &params.status {
            bindings.push((
                "status".into(),
                ibis_db::value::Value::from_t(status.as_str()),
            ));
            sets.push("status = $status".to_string());
        }
        if let Some(confidence) = params.confidence {
            bindings.push((
                "confidence".into(),
                ibis_db::value::Value::from_t(confidence),
            ));
            sets.push("confidence = $confidence".to_string());
        }
        if let Some(tags) = &params.tags {
            bindings.push((
                "tags".into(),
                ibis_db::value::Value::from_t(
                    tags.iter()
                        .map(|t| ibis_db::value::Value::from_t(t.as_str()))
                        .collect::<Vec<ibis_db::value::Value>>(),
                ),
            ));
            sets.push("tags = $tags".to_string());
        }
        if let Some(visibility) = &params.visibility {
            bindings.push((
                "visibility".into(),
                ibis_db::value::Value::from_t(visibility.as_str()),
            ));
            sets.push("visibility = $visibility".to_string());
        }
        if let Some(valid_until) = &params.valid_until {
            let value = valid_until.trim();
            if value.is_empty() {
                sets.push("valid_until = NONE".to_string());
            } else {
                bindings.push(("valid_until".into(), ibis_db::value::Value::from_t(value)));
                sets.push("valid_until = type::datetime($valid_until)".to_string());
            }
        }

        let sql = format!("UPDATE {} SET {};", params.id, sets.join(", "));
        if bindings.is_empty() {
            db.execute(&sql)
                .await
                .map_err(|e| SearchError::other(format!("update memory: {e}")))?;
        } else {
            db.smart_query(&sql, bind(bindings))
                .await
                .map_err(|e| SearchError::other(format!("update memory: {e}")))?;
        }
        Ok(())
    }

    /// Loads one memory row for governance checks.
    pub(crate) async fn memory_for_governance(&self, id: &str) -> crate::Result<Memory> {
        require_memory_record_id(id)?;
        let db = self.require_db()?;
        let sql = format!("SELECT * FROM {id};");
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("load memory governance: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let memories = memories_from_rows(&rows_values);
        memories
            .into_iter()
            .next()
            .ok_or_else(|| SearchError::other(format!("memory {id} not found")))
    }

    /// Archives a memory while retaining its history and graph edges
    ///
    pub async fn delete_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        id: &str,
        explicit_actor: &str,
    ) -> crate::Result<()> {
        let db = self.require_db()?;
        require_memory_record_id(id)?;
        let memory = self.memory_for_governance(id).await?;
        if memory.store == STORE_GIT {
            let key = memory_record_key(&memory.id).unwrap_or_else(|_| memory.id.clone());
            return Err(SearchError::other(format!(
                "git-owned claim {} can only be withdrawn by deleting .ibis/claims/{}.md in a commit (repo is SoT)",
                memory.id, key
            )));
        }
        authorize_memory_mutation(&memory, &mutation_actor(actor_ctx, &memory, explicit_actor))?;
        let sql = format!("UPDATE {id} SET status = 'archived', updated_at = time::now();");
        db.execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("archive memory: {e}")))?;
        Ok(())
    }

    /// `MemoryDelete: soft deletion archives; hard deletion removes the
    /// record and all of its graph edges.
    pub async fn memory_delete(
        &self,
        actor_ctx: &crate::ActorContext,
        id: &str,
        hard: bool,
        actor: &str,
    ) -> crate::Result<String> {
        if !hard {
            self.delete_memory(actor_ctx, id, actor).await?;
            return Ok("archived".to_string());
        }
        let db = self.require_db()?;
        require_memory_record_id(id)?;
        let memory = self.memory_for_governance(id).await?;
        if memory.store == STORE_GIT {
            return Err(SearchError::other(format!(
                "git-owned claim {} can only be withdrawn by deleting .ibis/claims/ in a commit (repo is SoT)",
                memory.id
            )));
        }
        let resolved_actor = mutation_actor(actor_ctx, &memory, actor);
        if memory.owner.trim() != resolved_actor {
            return Err(SearchError::other(format!(
                "hard delete requires owner {:?}; resolved actor is {:?}",
                memory.owner, resolved_actor
            )));
        }
        let sql = format!(
            "BEGIN TRANSACTION;\nDELETE {ES_SUPERSEDES} WHERE in = {id} OR out = {id};\nDELETE {ES_DERIVED_FROM} WHERE in = {id};\nDELETE {ES_PART_OF_WORKSTREAM} WHERE in = {id};\nDELETE {id};\nCOMMIT TRANSACTION;"
        );
        db.execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("delete memory: {e}")))?;
        Ok("deleted".to_string())
    }

    /// Full memory rows ordered by most recent update.
    pub async fn list_memories(
        &self,
        actor_ctx: &crate::ActorContext,
        params: MemoryListParams,
    ) -> crate::Result<Vec<Memory>> {
        let db = self.require_db()?;
        let mut limit = params.limit;
        if limit == 0 {
            limit = 100;
        }
        limit = limit.min(1000);
        let actor = actor_ctx.resolve_trusted_actor(&params.actor);
        let (vis_filter, mut bindings) = memory_visibility_filter(&params.visibility, &actor)?;
        let mut where_parts = vec![vis_filter];
        if let Some((filter, more)) = kinds_filter(&params.kinds) {
            where_parts.push(filter);
            bindings.extend(more);
        }
        if !params.status.trim().is_empty() {
            let (filter, more) = temporal::memory_status_filter_sql(params.status.trim());
            where_parts.push(filter);
            bindings.extend(more);
        }
        let sql = format!(
            "SELECT id, kind, title, content, status, confidence, visibility, owner, tags, branch, valid_from, valid_until, created_at, updated_at FROM memory WHERE {} ORDER BY updated_at DESC LIMIT {limit};",
            where_parts.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("list memories: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        Ok(memories_from_rows(&rows_values))
    }

    /// Short semantic hits; call `expand_memory` for
    /// full content.
    pub async fn search_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        params: MemorySearchParams,
    ) -> crate::Result<Vec<Memory>> {
        let t0 = std::time::Instant::now();
        tracing::info!(
            "memory_search: start (query_len={}, kinds={:?})",
            params.query.len(),
            params.kinds
        );
        let db = self.require_db()?;
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;
        let vector = ai
            .embed_text(&params.query)
            .await
            .map_err(|e| {
                tracing::warn!(
                    "memory_search: embed_text failed after {:?}: {e}",
                    t0.elapsed()
                );
                SearchError::wrap("embedding failed", e)
            })?;
        tracing::info!("memory_search: embedded in {:?}", t0.elapsed());

        let mut status = params.status.trim().to_string();
        if status.is_empty() {
            status = "current".to_string();
        }
        let mut limit = params.limit;
        if limit == 0 {
            limit = 10;
        }
        let mut where_parts = vec!["embedding != NONE".to_string()];
        let mut bindings: Bindings = Vec::new();
        let (status_sql, status_bindings) = temporal::memory_status_filter_sql(&status);
        where_parts.push(status_sql);
        bindings.extend(status_bindings);
        if let Some((filter, more)) = kinds_filter(&params.kinds) {
            where_parts.push(filter);
            bindings.extend(more);
        }
        let project = params.project_name.trim();
        if !project.is_empty() {
            let repo_id = repo_record_id(project);
            where_parts.push(format!(
                "(project = {repo_id} OR id IN {repo_id}->{ES_HAS_MEMORY}->{TABLE_MEMORY})"
            ));
        }
        let actor = actor_ctx.resolve_trusted_actor(&params.actor);
        let (vis_filter, vis_bindings) = memory_visibility_filter(&params.visibility, &actor)?;
        where_parts.push(vis_filter);
        bindings.extend(vis_bindings);
        if !params.tags.is_empty() {
            if let Some((filter, more)) = tags_filter(&params.tags) {
                where_parts.push(filter);
                bindings.extend(more);
            }
        }

        // Wider candidate pool so keyword/entity re-ranking can surface
        // lexical matches just outside the pure vector top-K.
        let fetch_limit = (limit * 3).clamp(limit, 45);

        let sql = format!(
            "SELECT id, kind, title, string::slice(content, 0, {MEMORY_SEARCH_SUMMARY_RUNES}) AS content, status, confidence, visibility, owner, tags, branch, valid_from, valid_until, created_at, updated_at, vector::similarity::cosine(embedding, $vec) AS score FROM memory WHERE {} ORDER BY score DESC LIMIT {fetch_limit};",
            where_parts.join(" AND ")
        );
        bindings.push(("vec".into(), crate::commit_meaning::vec_value_f32(&vector)));
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("search memory: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let mut memories = memories_from_rows(&rows_values);
        let (keywords, entities) = crate::retrieval_boost::tokenize_query(&params.query);
        let linked_symbols =
            crate::retrieval_boost::resolve_linked_symbols(self.db.as_ref(), &entities).await;
        let mut hits: Vec<crate::retrieval_boost::ScoredHit> = memories
            .iter()
            .enumerate()
            .map(|(i, mem)| crate::retrieval_boost::ScoredHit {
                text: format!("{} {} {}", mem.title, mem.content, mem.kind),
                path: String::new(),
                tags: mem.tags.clone(),
                score: mem.score,
                payload: i,
            })
            .collect();
        crate::retrieval_boost::apply_multi_signal_boost(
            &mut hits,
            &keywords,
            &entities,
            &linked_symbols,
        );
        let mut boosted: Vec<Memory> = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(mem) = memories.get_mut(hit.payload) else {
                continue;
            };
            let mut mem = mem.clone();
            mem.score = hit.score;
            mem.content = shorten_memory_content(&mem.content);
            boosted.push(mem);
            if boosted.len() >= limit {
                break;
            }
        }
        tracing::info!(
            "memory_search: done in {:?} hits={}",
            t0.elapsed(),
            boosted.len()
        );
        Ok(boosted)
    }

    /// Complete memory rows for the requested IDs.
    pub async fn expand_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        ids: &[String],
        explicit_actor: &str,
    ) -> crate::Result<Vec<Memory>> {
        let db = self.require_db()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut safe_ids: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.trim();
            require_memory_record_id(id)?;
            safe_ids.push(id.to_string());
        }
        let actor = actor_ctx.resolve_trusted_actor(explicit_actor);
        let (vis_filter, bindings) = memory_visibility_filter("", &actor)?;
        let sql = format!(
            "SELECT * FROM [{}] WHERE {vis_filter};",
            safe_ids.join(", ")
        );
        let outcome = db
            .smart_query(&sql, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("expand memory: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        Ok(memories_from_rows(&rows_values))
    }

    /// One full memory with provenance and history.
    pub async fn memory_expand(
        &self,
        actor_ctx: &crate::ActorContext,
        id: &str,
        depth: usize,
        explicit_actor: &str,
    ) -> crate::Result<MemoryExpansion> {
        let memories = self
            .expand_memory(actor_ctx, &[id.to_string()], explicit_actor)
            .await?;
        let memory = memories
            .into_iter()
            .next()
            .ok_or_else(|| SearchError::other(format!("memory {id} not found")))?;
        let history = self
            .memory_history(actor_ctx, id, depth, explicit_actor)
            .await?;

        let db = self.require_db()?;
        let sql = format!("SELECT out FROM {ES_DERIVED_FROM} WHERE in = {id};");
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("expand memory provenance: {e}")))?;
        let mut derived: Vec<String> = Vec::new();
        if let Some(rows_values) = rows::normalize_rows(&outcome) {
            for row in rows::object_rows(&rows_values) {
                let record_id = rows::record_id(rows::field(row, "out"));
                if !record_id.is_empty() {
                    derived.push(record_id);
                }
            }
        }

        let workstream = self.memory_workstream(id).await?;
        Ok(MemoryExpansion {
            memory,
            supersedes_chain: history,
            derived_from: derived,
            workstream,
        })
    }

    /// `memoryWorkstream.
    async fn memory_workstream(&self, id: &str) -> crate::Result<Option<Workstream>> {
        let db = self.require_db()?;
        let sql = format!("SELECT out.* FROM {ES_PART_OF_WORKSTREAM} WHERE in = {id};");
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("expand memory workstream: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Ok(None);
        };
        let objects = rows::object_rows(&rows_values);
        let Some(row) = objects.first() else {
            return Ok(None);
        };
        Ok(Some(Workstream {
            id: rows::record_id(rows::field(row, "id")),
            name: rows::scalar_string(rows::field(row, "name")),
            status: rows::scalar_string(rows::field(row, "status")),
        }))
    }

    /// Supersession lineage ordered oldest → newest, by id or subject phrase
    ///
    pub async fn memory_history(
        &self,
        actor_ctx: &crate::ActorContext,
        id_or_subject: &str,
        depth: usize,
        explicit_actor: &str,
    ) -> crate::Result<Vec<Memory>> {
        let db = self.require_db()?;
        let depth = depth.clamp(1, 5);
        let actor = actor_ctx.resolve_trusted_actor(explicit_actor);
        let (vis_filter, vis_bindings) = memory_visibility_filter("", &actor)?;
        let id = id_or_subject.trim();
        let id = if !ibis_db::ids::is_safe_record_id(id) {
            self.memory_id_for_subject(id, &vis_filter, bind(vis_bindings.clone()))
                .await?
        } else {
            id.to_string()
        };
        require_memory_record_id(&id)?;

        let sql = format!("SELECT * FROM {id} WHERE {vis_filter};");
        let outcome = db
            .smart_query(&sql, bind(vis_bindings.clone()))
            .await
            .map_err(|e| SearchError::other(format!("query memory history: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let self_memories = memories_from_rows(&rows_values);
        if self_memories.is_empty() {
            return Ok(Vec::new());
        }
        let mut visited = std::collections::HashSet::new();
        visited.insert(id.clone());
        let mut older = self
            .walk_memory_supersession(
                &id,
                depth,
                true,
                &mut visited,
                &vis_filter,
                bind(vis_bindings.clone()),
            )
            .await?;
        let newer = self
            .walk_memory_supersession(
                &id,
                depth,
                false,
                &mut visited,
                &vis_filter,
                bind(vis_bindings),
            )
            .await?;
        older.reverse();
        let mut history = Vec::with_capacity(older.len() + self_memories.len() + newer.len());
        history.extend(older);
        history.extend(self_memories);
        history.extend(newer);
        Ok(history)
    }

    /// `walkMemorySupersession.
    async fn walk_memory_supersession(
        &self,
        start_id: &str,
        depth: usize,
        older: bool,
        visited: &mut std::collections::HashSet<String>,
        vis_filter: &str,
        vis_bindings: ibis_db::executor::Vars,
    ) -> crate::Result<Vec<Memory>> {
        let db = self.require_db()?;
        let mut lineage = Vec::with_capacity(depth);
        let mut current_id = start_id.to_string();
        for _hop in 0..depth {
            let sql = if older {
                format!("SELECT * FROM {current_id}->{ES_SUPERSEDES}->{TABLE_MEMORY} WHERE {vis_filter};")
            } else {
                format!("SELECT * FROM {current_id}<-{ES_SUPERSEDES}<-{TABLE_MEMORY} WHERE {vis_filter};")
            };
            let outcome = db
                .smart_query(&sql, vis_bindings.clone())
                .await
                .map_err(|e| SearchError::other(format!("query memory history: {e}")))?;
            let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
            let memories = memories_from_rows(&rows_values);
            let Some(next) = memories.first() else {
                break;
            };
            if next.id.is_empty() || visited.contains(&next.id) {
                break;
            }
            visited.insert(next.id.clone());
            lineage.push(next.clone());
            current_id = next.id.clone();
        }
        Ok(lineage)
    }

    /// `memoryIDForSubject.
    async fn memory_id_for_subject(
        &self,
        subject: &str,
        vis_filter: &str,
        vis_bindings: ibis_db::executor::Vars,
    ) -> crate::Result<String> {
        if subject.is_empty() {
            return Err(SearchError::other("memory id or subject is required"));
        }
        let db = self.require_db()?;
        let lowered = subject.to_lowercase();
        let mut bindings = vis_bindings;
        bindings.insert(
            "subject".into(),
            ibis_db::value::Value::from_t(lowered.as_str()),
        );
        let sql = format!(
            "SELECT id FROM memory WHERE ({vis_filter}) AND (string::contains(string::lowercase(title OR ''), $subject) OR string::contains(string::lowercase(content), $subject)) ORDER BY updated_at DESC LIMIT 1;"
        );
        let outcome = db
            .smart_query(&sql, bindings)
            .await
            .map_err(|e| SearchError::other(format!("find memory history subject: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            return Err(SearchError::other(format!(
                "memory subject {subject:?} not found"
            )));
        };
        let objects = rows::object_rows(&rows_values);
        let id = objects
            .first()
            .map(|row| rows::record_id(rows::field(row, "id")))
            .unwrap_or_default();
        if id.is_empty() {
            return Err(SearchError::other(format!(
                "memory subject {subject:?} not found"
            )));
        }
        require_memory_record_id(&id)
            .map_err(|_| SearchError::other("memory subject query returned invalid id"))?;
        Ok(id)
    }
}

/// `authorizeMemoryMutation.
pub(crate) fn authorize_memory_mutation(memory: &Memory, actor: &str) -> crate::Result<()> {
    if memory.owner.trim() == actor {
        return Ok(());
    }
    if memory.visibility == "personal" {
        return Err(SearchError::other(format!(
            "personal memory {} is writable only by owner {:?}; share it before team changes",
            memory.id, memory.owner
        )));
    }
    Err(SearchError::other(format!(
        "team memory {} is writable only by owner {:?}; create a new memory that supersedes it",
        memory.id, memory.owner
    )))
}

/// `mutationActor: personal rows require a trusted non-anonymous actor;
/// team rows keep the anonymous-local "unknown" owner path.
pub(crate) fn mutation_actor(
    actor_ctx: &crate::ActorContext,
    memory: &Memory,
    explicit: &str,
) -> String {
    if memory.visibility == "personal" {
        return actor_ctx.resolve_trusted_actor(explicit);
    }
    if actor_ctx.actor_is_remote_untrusted() {
        return String::new();
    }
    actor_ctx.resolve_actor(explicit)
}

/// Edge table names used by the memory flows.
pub(crate) const ES_SUPERSEDES: &str = "supersedes";
pub(crate) const ES_DERIVED_FROM: &str = "derived_from";
pub(crate) const ES_PART_OF_WORKSTREAM: &str = "part_of_workstream";
pub(crate) const ES_HAS_MEMORY: &str = "has_memory";
pub(crate) const ES_HAS_REASONING: &str = "has_reasoning";
pub(crate) const ES_CHANGED: &str = "changed";
pub(crate) const ES_IMPLEMENTS: &str = "implements";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_record_key_strips_wrapping() {
        assert_eq!(memory_record_key("memory:⟨mem_x⟩").unwrap(), "mem_x");
        assert_eq!(memory_record_key("memory:mem_x").unwrap(), "mem_x");
        assert!(memory_record_key("commit:abc").is_err());
    }

    #[test]
    fn first_line_caps_runes() {
        assert_eq!(first_line("alpha\nbeta", 80), "alpha");
        assert_eq!(first_line(&"x".repeat(100), 80), "x".repeat(80));
    }

    #[test]
    fn repo_record_id_sanitizes() {
        assert_eq!(repo_record_id("Ibis Assistant"), "repo:⟨ibis_assistant⟩");
        assert_eq!(repo_record_id("  "), "");
    }

    #[test]
    fn id_validation_rejects_other_tables() {
        assert!(require_memory_record_id("memory:⟨a⟩").is_ok());
        assert!(require_memory_record_id("key_usage:one").is_err());
        assert!(require_memory_record_id("not an id").is_err());
        assert!(validate_memory_confidence(2.0).is_err());
        assert!(validate_memory_confidence(0.7).is_ok());
    }

    #[test]
    fn shorten_memory_content_is_rune_bounded() {
        assert_eq!(
            shorten_memory_content(&"x".repeat(300)).chars().count(),
            240
        );
        assert_eq!(shorten_memory_content("short"), "short");
    }
}
