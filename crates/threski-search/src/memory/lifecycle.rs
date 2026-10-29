//! Memory lifecycle: supersession, dirty-anchor
//! promotion, and stale refresh driven by provenance source hashes.

use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Bounds the per-candidate commit scan for promotion.
pub(crate) const PROMOTE_SCAN_LIMIT: usize = 50;

/// `lookupCurrentFileHashByPath: returns the HEAD content hash for a
/// path. Prefers `is_head = true` rows written by code ingest; falls back to
/// the newest `updated_at` row. Both path values bind as `$path` (F-002);
/// the table is `source_file` (the canonical post-migration name).
pub(crate) async fn lookup_current_file_hash_by_path(
    service: &SearchService,
    path: &str,
) -> Result<(String, bool)> {
    let db = service.require_db()?;
    let sql_head = "SELECT hash FROM source_file WHERE (path = $path OR rel_path = $path) AND is_head = true LIMIT 1;";
    let mut vars = threski_db::executor::Vars::new();
    vars.insert("path".into(), threski_db::value::Value::from_t(path));
    let outcome = db
        .smart_query(sql_head, vars.clone())
        .await
        .map_err(|e| SearchError::other(format!("lookup source_file head hash for {path}: {e}")))?;
    if let Some(rows_values) = rows::normalize_rows(&outcome) {
        for row in rows::object_rows(&rows_values) {
            let h = rows::scalar_string(rows::field(row, "hash"))
                .trim()
                .to_string();
            if !h.is_empty() {
                return Ok((h, true));
            }
        }
    }
    let sql_recent =
        "SELECT hash FROM source_file WHERE path = $path OR rel_path = $path ORDER BY updated_at DESC LIMIT 1;";
    let outcome = db
        .smart_query(sql_recent, vars)
        .await
        .map_err(|e| SearchError::other(format!("lookup source_file hash for {path}: {e}")))?;
    if let Some(rows_values) = rows::normalize_rows(&outcome) {
        for row in rows::object_rows(&rows_values) {
            let h = rows::scalar_string(rows::field(row, "hash"))
                .trim()
                .to_string();
            if !h.is_empty() {
                return Ok((h, true));
            }
        }
    }
    Ok((String::new(), false))
}

impl SearchService {
    /// `SupersedeMemory: records that newID replaces oldID and retires
    /// the old memory.
    pub async fn supersede_memory(&self, old_id: &str, new_id: &str) -> Result<()> {
        self.supersede_memory_for_repo(old_id, new_id, "").await
    }

    /// `SupersedeMemoryForRepo: additionally binds the git axis when a
    /// working repo is supplied (the world-state spec): old row X := HEAD,
    /// new row I := HEAD only when the tip is clean.
    pub async fn supersede_memory_for_repo(
        &self,
        old_id: &str,
        new_id: &str,
        repo_path: &str,
    ) -> Result<()> {
        if self.db.is_none() {
            return Err(SearchError::DatabaseNotConnected);
        }
        super::validate_memory_id(old_id)
            .map_err(|e| SearchError::other(format!("invalid old memory id: {e}")))?;
        super::validate_memory_id(new_id)
            .map_err(|e| SearchError::other(format!("invalid new memory id: {e}")))?;
        if old_id == new_id {
            return Err(SearchError::other("a memory cannot supersede itself"));
        }

        let mut head_oid = String::new();
        if !repo_path.is_empty() {
            if let Some(anchors) = &self.git_anchors {
                if let Ok(h) = anchors.rev_parse_head(repo_path).await {
                    head_oid = h.trim().to_string();
                }
            }
        }
        let old_set = if head_oid.is_empty() {
            "status = 'superseded', updated_at = time::now()".to_string()
        } else {
            format!(
                "status = 'superseded', invalidated_at_oid = '{head_oid}', provenance.invalidated_at_commit = {}, updated_at = time::now()",
                threski_db::ids::format_record_id("commit", &head_oid)
            )
        };
        let mut new_update = String::new();
        if !head_oid.is_empty() {
            if let Some(anchors) = &self.git_anchors {
                if let Ok(true) = anchors.is_clean(repo_path).await {
                    new_update = format!(
                        "UPDATE {new_id} SET introduced_in_oid = '{head_oid}', provenance.introduced_in_commit = {}, updated_at = time::now();",
                        threski_db::ids::format_record_id("commit", &head_oid)
                    );
                }
            }
        }

        let mut query = format!(
            "BEGIN TRANSACTION;\nUPDATE {old_id} SET {old_set};\nRELATE {new_id}->supersedes->{old_id};\n"
        );
        if !new_update.is_empty() {
            query.push_str(&new_update);
            query.push('\n');
        }
        query.push_str("COMMIT TRANSACTION;");
        let db = self.require_db()?;
        db.execute(&query)
            .await
            .map_err(|e| SearchError::other(format!("supersede memory: {e}")))?;
        Ok(())
    }

    /// `MarkStaleWhenSourceChanged: marks a current memory stale when its
    /// recorded provenance hash no longer matches the current source hash.
    pub async fn mark_stale_when_source_changed(
        &self,
        memory_id: &str,
        source_hash: &str,
        current_hash: &str,
    ) -> Result<bool> {
        if self.db.is_none() {
            return Err(SearchError::DatabaseNotConnected);
        }
        super::validate_memory_id(memory_id)?;
        let source_hash = source_hash.trim();
        let current_hash = current_hash.trim();
        if source_hash.is_empty() || current_hash.is_empty() || source_hash == current_hash {
            return Ok(false);
        }
        let db = self.require_db()?;
        let sql = format!(
            "UPDATE {memory_id} SET status = 'stale', updated_at = time::now() WHERE status = 'current';"
        );
        db.execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("mark memory stale: {e}")))?;
        Ok(true)
    }

    /// `RefreshStaleMemories: marks current memories stale when the HEAD
    /// source_file for their provenance path has a different content hash
    /// than provenance.source_hash, or when that path no longer exists at
    /// HEAD.
    pub async fn refresh_stale_memories(&self) -> Result<usize> {
        self.refresh_stale_memories_for_repo("").await
    }

    /// `RefreshStaleMemoriesForRepo: behaves like
    /// [`SearchService::refresh_stale_memories`] and binds the git axis on
    /// every stale mark when a working repo is supplied (X(m) := HEAD).
    pub async fn refresh_stale_memories_for_repo(&self, repo_path: &str) -> Result<usize> {
        if self.db.is_none() {
            return Err(SearchError::DatabaseNotConnected);
        }
        let db = self.require_db()?;

        let mut head_oid = String::new();
        if !repo_path.is_empty() {
            if let Some(anchors) = &self.git_anchors {
                if let Ok(h) = anchors.rev_parse_head(repo_path).await {
                    head_oid = h.trim().to_string();
                }
            }
        }

        struct Candidate {
            memory_id: String,
            source_hash: String,
            path: String,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_memory = std::collections::HashSet::new();

        // Primary: durable provenance.source_path (survives deleted
        // edges/records).
        let path_sql = "SELECT id AS memory_id, provenance.source_hash AS source_hash, provenance.source_path AS path FROM memory WHERE status = 'current' AND provenance.source_hash != NONE AND provenance.source_path != NONE;";
        let outcome = db
            .execute(path_sql)
            .await
            .map_err(|e| SearchError::other(format!("query memory provenance paths: {e}")))?;
        if let Some(rows_values) = rows::normalize_rows(&outcome) {
            for row in rows::object_rows(&rows_values) {
                let memory_id = rows::record_id(rows::field(row, "memory_id"));
                if super::validate_memory_id(&memory_id).is_err() {
                    continue;
                }
                let path = rows::scalar_string(rows::field(row, "path"))
                    .trim()
                    .to_string();
                let source_hash = rows::scalar_string(rows::field(row, "source_hash"))
                    .trim()
                    .to_string();
                if path.is_empty() || source_hash.is_empty() {
                    continue;
                }
                candidates.push(Candidate {
                    memory_id: memory_id.clone(),
                    source_hash,
                    path,
                });
                seen_memory.insert(memory_id);
            }
        }

        // Fallback: derived_from → source_file, but only when source_path is
        // unset OR the edge path matches provenance.source_path (avoids
        // multi-edge false stale).
        let edge_sql = "SELECT in AS memory_id, out AS source_id, in.provenance.source_hash AS source_hash, in.provenance.source_path AS source_path, out.path AS path, out.rel_path AS rel_path FROM derived_from WHERE in.status = 'current' AND in.provenance.source_hash != NONE;";
        let outcome = db
            .execute(edge_sql)
            .await
            .map_err(|e| SearchError::other(format!("query memory source hashes: {e}")))?;
        if let Some(rows_values) = rows::normalize_rows(&outcome) {
            for row in rows::object_rows(&rows_values) {
                let source_id = rows::record_id(rows::field(row, "source_id"));
                if !source_id.starts_with("source_file:") {
                    continue;
                }
                let memory_id = rows::record_id(rows::field(row, "memory_id"));
                if super::validate_memory_id(&memory_id).is_err()
                    || seen_memory.contains(&memory_id)
                {
                    continue;
                }
                let source_hash = rows::scalar_string(rows::field(row, "source_hash"))
                    .trim()
                    .to_string();
                if source_hash.is_empty() {
                    continue;
                }
                let mut edge_path = rows::scalar_string(rows::field(row, "path"))
                    .trim()
                    .to_string();
                if edge_path.is_empty() {
                    edge_path = rows::scalar_string(rows::field(row, "rel_path"))
                        .trim()
                        .to_string();
                }
                if edge_path.is_empty() {
                    continue;
                }
                let source_path = rows::scalar_string(rows::field(row, "source_path"))
                    .trim()
                    .to_string();
                if !source_path.is_empty() && source_path != edge_path {
                    continue;
                }
                candidates.push(Candidate {
                    memory_id: memory_id.clone(),
                    source_hash,
                    path: edge_path,
                });
                seen_memory.insert(memory_id);
            }
        }

        let mut stale_count = 0;
        let mut marked = std::collections::HashSet::new();
        for candidate in &candidates {
            if marked.contains(&candidate.memory_id) {
                continue;
            }
            let (current_hash, found) =
                match lookup_current_file_hash_by_path(self, &candidate.path).await {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
            if !found {
                // Path gone from HEAD → memory is stale.
                let sql = stale_mark_statement(&candidate.memory_id, &head_oid);
                db.execute(&sql).await.map_err(|e| {
                    SearchError::other(format!("mark memory stale for missing path: {e}"))
                })?;
                marked.insert(candidate.memory_id.clone());
                stale_count += 1;
                continue;
            }
            if candidate.source_hash != current_hash {
                let sql = stale_mark_statement(&candidate.memory_id, &head_oid);
                db.execute(&sql)
                    .await
                    .map_err(|e| SearchError::other(format!("mark memory stale: {e}")))?;
                marked.insert(candidate.memory_id.clone());
                stale_count += 1;
            }
        }
        Ok(stale_count)
    }

    /// `PromoteDirtyAnchorsForRepo: completes the promote_on_commit
    /// contract (the world-state spec) — for memories flagged
    /// promote_on_commit with a worktree hash, once a commit containing the
    /// path with exactly those bytes lands (i.e. after ingest), bind I to
    /// that commit and clear the dirty flags. Returns the promoted count.
    pub async fn promote_dirty_anchors_for_repo(&self, repo_path: &str) -> Result<usize> {
        if self.db.is_none() {
            return Err(SearchError::DatabaseNotConnected);
        }
        if repo_path.is_empty() {
            return Ok(0);
        }
        let Some(anchors) = &self.git_anchors else {
            return Ok(0);
        };
        let db = self.require_db()?;
        let sql = "SELECT id, provenance.worktree_hash AS worktree_hash, provenance.source_path AS source_path FROM memory WHERE provenance.promote_on_commit = true AND introduced_in_oid = NONE;";
        let outcome = db
            .execute(sql)
            .await
            .map_err(|e| SearchError::other(format!("promote dirty anchors: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let mut promoted = 0;
        for row in rows::object_rows(&rows_values) {
            let mut id = rows::scalar_string(rows::field(row, "id"))
                .trim()
                .to_string();
            if id.is_empty() {
                id = rows::record_id(rows::field(row, "id"));
            }
            let path = rows::scalar_string(rows::field(row, "source_path"))
                .trim()
                .to_string();
            let want_hash = rows::scalar_string(rows::field(row, "worktree_hash"))
                .trim()
                .to_string();
            if id.is_empty() || path.is_empty() || want_hash.is_empty() {
                continue;
            }
            if super::validate_memory_id(&id).is_err() {
                continue;
            }
            let history = match anchors.file_history(repo_path, &path).await {
                Ok(h) => h,
                Err(_) => continue,
            };
            let history = &history[..history.len().min(PROMOTE_SCAN_LIMIT)];
            for oid in history {
                let content = match anchors.file_at_ref(repo_path, oid, &path).await {
                    Ok(Some(bytes)) => bytes,
                    _ => continue,
                };
                if super::gitbound::hash_bytes(&content) != want_hash {
                    continue;
                }
                let sql = format!(
                    "UPDATE {id} SET introduced_in_oid = '{oid}', provenance.introduced_in_commit = {}, provenance.dirty = false, provenance.promote_on_commit = false, updated_at = time::now();",
                    threski_db::ids::format_record_id("commit", oid)
                );
                if let Err(e) = db.execute(&sql).await {
                    return Err(SearchError::other(format!("promote dirty anchors: {e}")));
                }
                promoted += 1;
                break;
            }
        }
        Ok(promoted)
    }
}

/// `staleMarkStatement: marks a memory stale, binding X(m) := HEAD when
/// the git axis is available (the world-state spec). The HEAD oid is a
/// git-verified full hash, so the record literal stays inline.
pub(crate) fn stale_mark_statement(memory_id: &str, head_oid: &str) -> String {
    if head_oid.is_empty() {
        return format!(
            "UPDATE {memory_id} SET status = 'stale', updated_at = time::now() WHERE status = 'current';"
        );
    }
    format!(
        "UPDATE {memory_id} SET status = 'stale', invalidated_at_oid = '{head_oid}', provenance.invalidated_at_commit = {}, updated_at = time::now() WHERE status = 'current';",
        threski_db::ids::format_record_id("commit", head_oid)
    )
}
