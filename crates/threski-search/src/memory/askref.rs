//! Ask at-ref: answers under the world slice at ref
//! R (git-indexed the world-state spec — honest retrieval contract):
//!
//! - claims: vector search over git-bound candidates, filtered by
//!   HOLDAt(m,R) which wins over Oct `status=current` for historical refs;
//! - Δ commit_chunk: semantic changelog hits, noise-filtered to commits
//!   reachable from R (AncestorSet);
//! - reasoning: HEAD-contract utility signal (not git-bound);
//! - code: HEAD embeddings ∩ tree(R) path filter + git-show bodies
//!   (HEAD bytes must never masquerade as bytes at R).
//!
//! Every returned hit carries `as_of_ref`; commit_chunk hits are never
//! `is_current`. The embedding binds as `$vec` and the actor as `$actor`
//! (F-002).

use super::ancestry::{ancestor_set_for, row_score};
use crate::error::{Result, SearchError};
use crate::memory::claim_tree::cap_at_ref_results;
use crate::memory::temporal::ask_memory_visibility_sql;
use crate::rows;
use crate::search::SearchService;
use crate::search::{ContextData, SearchResult};
use crate::ActorContext;

impl SearchService {
    /// Answers under the world slice at ref R.
    pub async fn ask_project_at_ref(
        &self,
        actor_ctx: &ActorContext,
        query: &str,
        project_name: &str,
        repo_path: &str,
        at_ref: &str,
        actor: &str,
    ) -> Result<Vec<SearchResult>> {
        if self.git_anchors.is_none() {
            return Err(SearchError::other(
                "ask at_ref requires the git anchor source",
            ));
        }
        if repo_path.trim().is_empty() {
            return Err(SearchError::other(
                "ask at_ref requires a resolvable repo path",
            ));
        }
        let ancestors = ancestor_set_for(self, repo_path, at_ref).await?;
        // Clone-only degraded mode (the claim-store addendum): with no
        // SurrealDB the git-native channels still answer — claims parsed
        // from .threski/claims/ at tree(R) (stubs title-only) and exact-path
        // code bytes at R. Vector channels are DB-bound and are skipped.
        if self.db.is_none() {
            return self
                .ask_project_at_ref_clone_only(query, repo_path, &ancestors)
                .await;
        }
        let db = self.db.as_ref().expect("checked above");
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;
        let vec = ai
            .embed_text(query)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;
        crate::search::cost_tracker(self)
            .record_embedding_usage("system:search_vector", query)
            .await;

        let score_expr = crate::search::ask_score_expr();
        let trusted_actor = actor_ctx.resolve_trusted_actor(actor);
        let mut results: Vec<SearchResult> = Vec::with_capacity(15);

        // Claims channel: HOLDAt wins over Oct status for historical refs.
        let (vis_sql, vis_bindings) = ask_memory_visibility_sql(&trusted_actor);
        let mut claims_where =
            format!("embedding != NONE AND introduced_in_oid != NONE AND {vis_sql}");
        let mut claims_vars = threski_db::executor::Vars::new();
        for (k, v) in vis_bindings {
            claims_vars.insert(k, v);
        }
        let project = project_name.trim();
        if !project.is_empty() {
            claims_where.push_str(&format!(
                " AND project = {}",
                super::repo_record_id(project)
            ));
        }
        let sql = format!(
            "SELECT id, content, status, kind, visibility, owner, introduced_in_oid, invalidated_at_oid, {score_expr} as score FROM memory WHERE {claims_where} ORDER BY score DESC LIMIT 15;"
        );
        claims_vars.insert("vec".into(), crate::commit_meaning::vec_value(&vec));
        let outcome = db
            .smart_query(&sql, claims_vars)
            .await
            .map_err(|e| SearchError::other(format!("vector search failed on memory: {e}")))?;
        for row in rows::object_rows(&rows::normalize_rows(&outcome).unwrap_or_default()) {
            let introduced_in = rows::scalar_string(rows::field(row, "introduced_in_oid"))
                .trim()
                .to_string();
            let invalidated_at = rows::scalar_string(rows::field(row, "invalidated_at_oid"))
                .trim()
                .to_string();
            if !threski_git::hold_at(&introduced_in, &invalidated_at, Some(&ancestors)) {
                continue;
            }
            // Defense in depth: personal claims never surface without their
            // owner, even if the SQL visibility clause is bypassed
            // (the world-state spec).
            let visibility = rows::scalar_string(rows::field(row, "visibility"))
                .trim()
                .to_string();
            let owner = rows::scalar_string(rows::field(row, "owner"))
                .trim()
                .to_string();
            if visibility == "personal" && owner != trusted_actor {
                continue;
            }
            results.push(SearchResult {
                id: rows::record_id(rows::field(row, "id")),
                content: rows::scalar_string(rows::field(row, "content")),
                score: row_score(rows::field(row, "score")),
                is_current: false,
                source_kind: super::TABLE_MEMORY.to_string(),
                status: rows::scalar_string(rows::field(row, "status")),
                kind: rows::scalar_string(rows::field(row, "kind")),
                as_of_ref: ancestors.ref_oid().to_string(),
                context: ContextData::default(),
                ..Default::default()
            });
        }

        // Claim store 4d union: tree(ref) claim files join the claims
        // channel — catches git-owned claims whose rows lack embeddings;
        // unranked, deduped against vector hits, with reserved slots at the
        // shared result cap.
        results = self
            .merge_tree_claims_into_ask_ref(repo_path, ancestors.ref_oid(), &trusted_actor, results)
            .await?;

        // Δ commit_chunk channel: changelog hits reachable from R only.
        let sql = crate::commit_meaning::commit_chunk_ask_query();
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("vec".into(), crate::commit_meaning::vec_value(&vec));
        let outcome = db.smart_query(&sql, vars).await.map_err(|e| {
            SearchError::other(format!("vector search failed on commit_chunk: {e}"))
        })?;
        for row in rows::object_rows(&rows::normalize_rows(&outcome).unwrap_or_default()) {
            let commit_hash = rows::scalar_string(rows::field(row, "commit_hash"))
                .trim()
                .to_string();
            if !commit_hash.is_empty() && !ancestors.contains(&commit_hash) {
                // Commits on divergent lines or after R are noise in an
                // as_of ask.
                continue;
            }
            results.push(SearchResult {
                id: rows::record_id(rows::field(row, "id")),
                content: crate::commit_meaning::truncate_commit_meaning(&rows::scalar_string(
                    rows::field(row, "content"),
                )),
                score: row_score(rows::field(row, "score")),
                is_current: false,
                source_kind: "commit_chunk".to_string(),
                commit_hash,
                as_of_ref: ancestors.ref_oid().to_string(),
                context: ContextData::default(),
                ..Default::default()
            });
        }

        // Reasoning channel: HEAD-contract utility signal (not git-bound).
        let sql = format!(
            "SELECT id, NONE as path, string::concat(question OR '', '\n', outcome OR '') as content, NONE as hash, NONE as current_hash, {score_expr} as score FROM reasoning WHERE embedding != NONE ORDER BY score DESC LIMIT 15;"
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("vec".into(), crate::commit_meaning::vec_value(&vec));
        let outcome = db
            .smart_query(&sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("vector search failed on reasoning: {e}")))?;
        for row in rows::object_rows(&rows::normalize_rows(&outcome).unwrap_or_default()) {
            results.push(SearchResult {
                id: rows::record_id(rows::field(row, "id")),
                content: rows::scalar_string(rows::field(row, "content")),
                score: row_score(rows::field(row, "score")),
                is_current: true,
                source_kind: "reasoning".to_string(),
                as_of_ref: ancestors.ref_oid().to_string(),
                context: ContextData::default(),
                ..Default::default()
            });
        }

        // Code channel (phase 3d): honest hybrid — HEAD embeddings ∩ tree(R)
        // paths, bodies replaced by bytes at R via git show
        // (the world-state spec).
        let code_results = self
            .as_of_code_semantic(query, &vec, repo_path, ancestors.ref_oid())
            .await?;
        results.extend(code_results);

        // Shared result cap with reserved claim_file slots (claim store 4d
        // parking list): tree claims must not be evicted by a full ranked
        // board.
        Ok(cap_at_ref_results(results))
    }

    /// Serves the git-native channels when SurrealDB is unreachable
    /// (the claim-store addendum): claims parsed from `.threski/claims/` at
    /// tree(R) — team claims fully readable from the file, personal stubs
    /// title-only — plus the exact-path code channel, which is DB-free by
    /// design (the world-state spec). Vector channels stay DB-bound:
    /// degraded reads remain honest about what git alone can answer
    ///
    pub(crate) async fn ask_project_at_ref_clone_only(
        &self,
        query: &str,
        repo_path: &str,
        ancestors: &threski_git::AncestorSet,
    ) -> Result<Vec<SearchResult>> {
        let reference = ancestors.ref_oid().to_string();
        let tree_claims = self
            .tree_claim_files(repo_path, &reference)
            .await
            .map_err(|e| SearchError::other(format!("parse claim store at {reference}: {e}")))?;
        let mut results: Vec<SearchResult> = Vec::with_capacity(tree_claims.len() + 1);
        let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
        for tc in &tree_claims {
            let record_id = Self::tree_claim_record_id(&tc.claim);
            if record_id.is_empty() || seen.contains_key(&record_id) {
                continue;
            }
            seen.insert(record_id.clone(), true);
            let entry = self.tree_claim_as_of(repo_path, tc, "", "").await;
            let Some(entry) = entry else {
                continue;
            };
            results.push(super::claim_tree::claim_file_result(
                &record_id, tc, &entry, &reference,
            ));
        }
        let exact_path = query.trim();
        if !exact_path.is_empty() {
            // Blob membership guard (same contract as the DB-backed exact
            // channel): `ls-tree -r` lists files only, so a directory query
            // must not surface the tree listing as code bytes.
            if let Some(anchors) = &self.git_anchors {
                if let Ok(tree_paths) = anchors.list_tree(repo_path, &reference).await {
                    if tree_paths.iter().any(|p| p == exact_path) {
                        if let Ok((content, true)) = self
                            .cached_file_at_ref(repo_path, &reference, exact_path)
                            .await
                        {
                            results.push(SearchResult {
                                path: exact_path.to_string(),
                                score: 1.0,
                                is_current: false,
                                source_kind: "file_chunk".to_string(),
                                as_of_ref: reference.clone(),
                                content_source: "git_show".to_string(),
                                content,
                                context: ContextData::default(),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }
        Ok(results)
    }
}
