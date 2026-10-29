//! As-of reads: git-bound claims that HOLDAt(m, R) and
//! the honest hybrid code projection (semantic HEAD embeddings ∩ tree(R)
//! paths, bodies replaced by bytes at R via git show / the hot-ref cache).

use super::ancestry::{ancestor_set_for, git_bound_claims_for_project};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;
use crate::search::{ContextData, SearchResult};

/// A claim that holds at ref R (the world-state spec, projection π =
/// claims). The HOLDAt predicate — not Oct status — decides membership
///
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AsOfClaim {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub visibility: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub owner: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub introduced_in: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub invalidated_at: String,
    pub source_kind: String,
}

impl SearchService {
    /// Returns the git-bound claims that HOLDAt(m, R) for the ancestor set
    /// of R, under visibility rules (the world-state spec). Unanchored
    /// claims are excluded by construction (strict mode)
    ///
    pub async fn as_of_claims(
        &self,
        project_name: &str,
        repo_path: &str,
        reference: &str,
        actor: &str,
    ) -> Result<(Vec<AsOfClaim>, bool, String)> {
        if self.git_anchors.is_none() {
            return Err(SearchError::other("as_of requires the git anchor source"));
        }
        if repo_path.trim().is_empty() {
            return Err(SearchError::other("as_of requires a resolvable repo path"));
        }
        let ancestors = ancestor_set_for(self, repo_path, reference).await?;
        let (rows_values, truncated) = git_bound_claims_for_project(self, project_name, 0).await?;
        let actor = actor.trim();
        let mut claims: Vec<AsOfClaim> = Vec::new();
        for row in rows::object_rows(&rows_values) {
            let introduced_in = rows::scalar_string(rows::field(row, "introduced_in_oid"))
                .trim()
                .to_string();
            let invalidated_at = rows::scalar_string(rows::field(row, "invalidated_at_oid"))
                .trim()
                .to_string();
            if !threski_git::hold_at(&introduced_in, &invalidated_at, Some(&ancestors)) {
                continue;
            }
            let visibility = rows::scalar_string(rows::field(row, "visibility"))
                .trim()
                .to_string();
            let owner = rows::scalar_string(rows::field(row, "owner"))
                .trim()
                .to_string();
            if visibility == "personal" && owner != actor {
                // Personal claims never surface for non-owners.
                continue;
            }
            claims.push(AsOfClaim {
                id: rows::record_id(rows::field(row, "id")),
                kind: rows::scalar_string(rows::field(row, "kind"))
                    .trim()
                    .to_string(),
                title: rows::scalar_string(rows::field(row, "title"))
                    .trim()
                    .to_string(),
                status: rows::scalar_string(rows::field(row, "status"))
                    .trim()
                    .to_string(),
                visibility,
                owner,
                introduced_in,
                invalidated_at,
                source_kind: "claim".to_string(),
            });
        }
        // Claim store 4d: Σ_R = HOLDAt claims ∪ tree(R) claims (the addendum:
        // file presence is the truth predicate for git-owned claims).
        let claims = self
            .merge_tree_claims_into_as_of(repo_path, ancestors.ref_oid(), actor, claims)
            .await?;
        Ok((claims, truncated, ancestors.ref_oid().to_string()))
    }

    /// Answers code questions at ref R with the honest hybrid contract
    /// (the world-state spec): semantic search over existing HEAD file_chunk
    /// embeddings restricted to paths that exist in tree(R), with hit bodies
    /// replaced by bytes at R via git show. Exact path queries skip the
    /// semantic channel entirely.
    pub async fn as_of_code(
        &self,
        query: &str,
        repo_path: &str,
        reference: &str,
    ) -> Result<Vec<SearchResult>> {
        if self.git_anchors.is_none() {
            return Err(SearchError::other("as_of requires the git anchor source"));
        }
        // Clone-only degraded mode (the claim-store addendum): the semantic
        // channel is DB-bound — embedding is wasted work; degrade to the
        // exact-path channel alone.
        if self.db.is_none() {
            let ancestors = ancestor_set_for(self, repo_path, reference).await?;
            return Ok(self
                .as_of_code_exact_path(query, repo_path, ancestors.ref_oid())
                .await);
        }
        let ai = self.ai.as_ref().ok_or_else(|| {
            SearchError::other("as_of code requires the AI provider for embedding")
        })?;
        let vec = ai
            .embed_text(query)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;
        self.as_of_code_semantic(query, &vec, repo_path, reference)
            .await
    }

    /// Degraded exact-path channel: the query names a path in tree(R) → its
    /// bytes at R; anything else yields an empty slice. DB-free and AI-free
    /// by design (the claim-store addendum).
    pub(crate) async fn as_of_code_exact_path(
        &self,
        query: &str,
        repo_path: &str,
        reference: &str,
    ) -> Vec<SearchResult> {
        let exact_path = query.trim();
        if exact_path.is_empty() {
            return Vec::new();
        }
        let Some(anchors) = &self.git_anchors else {
            return Vec::new();
        };
        let tree_paths = match anchors.list_tree(repo_path, reference).await {
            Ok(paths) => paths,
            Err(_) => return Vec::new(),
        };
        for p in &tree_paths {
            if p == exact_path {
                return self
                    .as_of_code_exact_result(repo_path, reference, exact_path)
                    .await;
            }
        }
        Vec::new()
    }

    /// Builds the exact-path hit for a path already known to exist in
    /// tree(R); an empty slice when the bytes are unreadable at the ref.
    /// Reads go through the hot-ref byte cache (the spec strategy B)
    ///
    pub(crate) async fn as_of_code_exact_result(
        &self,
        repo_path: &str,
        reference: &str,
        exact_path: &str,
    ) -> Vec<SearchResult> {
        let mut results = Vec::with_capacity(1);
        if let Ok((content, true)) = self
            .cached_file_at_ref(repo_path, reference, exact_path)
            .await
        {
            results.push(SearchResult {
                path: exact_path.to_string(),
                score: 1.0,
                is_current: false,
                source_kind: "file_chunk".to_string(),
                as_of_ref: reference.to_string(),
                content_source: "git_show".to_string(),
                content,
                context: ContextData::default(),
                ..Default::default()
            });
        }
        results
    }

    /// Shared engine for AskProjectAtRef (precomputed vector) and the AsOfCode
    /// MCP projection. The vector binds as `$vec` (F-002).
    pub(crate) async fn as_of_code_semantic(
        &self,
        query: &str,
        vec: &[f32],
        repo_path: &str,
        reference: &str,
    ) -> Result<Vec<SearchResult>> {
        let ancestors = ancestor_set_for(self, repo_path, reference).await?;
        let anchors = self
            .git_anchors
            .as_ref()
            .expect("ancestor_set_for requires anchors");
        let tree_paths = anchors
            .list_tree(repo_path, ancestors.ref_oid())
            .await
            .map_err(|e| {
                SearchError::other(format!("list tree at {}: {e}", ancestors.ref_oid()))
            })?;
        let tree_set: std::collections::HashSet<&str> =
            tree_paths.iter().map(String::as_str).collect();

        // Exact channel: the query names a path in tree(R) — skip semantic.
        let exact_path = query.trim();
        if !exact_path.is_empty() && tree_set.contains(exact_path) {
            return Ok(self
                .as_of_code_exact_result(repo_path, ancestors.ref_oid(), exact_path)
                .await);
        }

        // Clone-only degraded mode (the claim-store addendum): the semantic
        // channel is DB-bound — degrade to the exact-path channel alone
        // instead of calling into a nil DB.
        let Some(db) = self.db.as_ref() else {
            return Ok(Vec::new());
        };

        // Semantic channel: HEAD embeddings ∩ paths in tree(R), bytes at R.
        let sql = format!(
            "SELECT id, file.path as path, content, hash, file.hash as current_hash, {} as score FROM file_chunk WHERE embedding != NONE ORDER BY score DESC LIMIT 15;",
            crate::search::ask_score_expr()
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("vec".into(), crate::commit_meaning::vec_value(vec));
        let outcome = db
            .smart_query(&sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("vector search failed on file_chunk: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let mut results = Vec::with_capacity(15);
        for row in rows::object_rows(&rows_values) {
            let path = rows::scalar_string(rows::field(row, "path"))
                .trim()
                .to_string();
            if path.is_empty() {
                continue;
            }
            if !tree_set.contains(path.as_str()) {
                // Path does not exist at R: the HEAD embedding hit is dropped
                // (the world-state spec).
                continue;
            }
            let (content, found) = match self
                .cached_file_at_ref(repo_path, ancestors.ref_oid(), &path)
                .await
            {
                Ok((content, found)) => (content, found),
                Err(_) => continue,
            };
            if !found {
                continue;
            }
            results.push(SearchResult {
                id: rows::record_id(rows::field(row, "id")),
                path,
                score: rows::numeric_float(rows::field(row, "score")),
                is_current: false,
                source_kind: "file_chunk".to_string(),
                as_of_ref: ancestors.ref_oid().to_string(),
                content_source: "git_show".to_string(),
                content,
                context: ContextData::default(),
                ..Default::default()
            });
        }
        Ok(results)
    }
}
