//! Knowledge diff: the structured delta between two
//! world slices (git-indexed the world-state spec). Claims are compared
//! purely via the ± HOLDAt symmetric difference — no phantom
//! `content_changed` entries.
//!
//! Degraded observability (D8/PR #34): a clone-only diff sets `degraded`
//! and serves the tree channel only.

use super::ancestry::{ancestor_set_for, git_bound_claims_for_project};
use crate::error::{Result, SearchError};
use crate::memory::asof::AsOfClaim;
use crate::rows;
use crate::search::SearchService;

pub(crate) const KNOWLEDGE_DIFF_DEFAULT_MAX_CLAIMS: usize = 200;
pub(crate) const KNOWLEDGE_DIFF_MAX_PATHS: usize = 500;

/// Diff mode: explicit is the default (from_ref used as given);
/// `since_divergence` makes from_ref implicit: the merge base of from_ref
/// and to_ref (the world-state spec merge_base-relative mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffMode {
    #[default]
    Explicit,
    SinceDivergence,
}

impl DiffMode {
    /// Parses the wire string (the constants "explicit"/"since_divergence").
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "explicit" => Some(DiffMode::Explicit),
            "since_divergence" => Some(DiffMode::SinceDivergence),
            _ => None,
        }
    }
}

/// The structured delta between two world slices.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeDiff {
    pub from_ref: String,
    pub to_ref: String,
    pub added_claims: Vec<AsOfClaim>,
    pub removed_claims: Vec<AsOfClaim>,
    pub changed_paths: Vec<String>,
    pub truncated: bool,
    /// Clone-only mode (the claim-store addendum): no SurrealDB reachable —
    /// the HOLDAt delta is empty and only the tree channel served.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub degraded: bool,
}

impl SearchService {
    /// Computes Diff(A, B) = Σ_B \ Σ_A (added) ∪ Σ_A \ Σ_B (removed) plus
    /// the git path diff between the two refs. Churn is capped: claims over
    /// `max_claims` and paths over [`KNOWLEDGE_DIFF_MAX_PATHS`] set
    /// the world-state spec).
    // The argument list mirrors the original MCP tool surface 1:1 — a params struct
    // would hide the contract this lane must keep byte-compatible.
    #[allow(clippy::too_many_arguments)]
    pub async fn knowledge_diff(
        &self,
        project_name: &str,
        repo_path: &str,
        from_ref: &str,
        to_ref: &str,
        actor: &str,
        diff_mode: DiffMode,
        max_claims: usize,
    ) -> Result<KnowledgeDiff> {
        if self.git_anchors.is_none() {
            return Err(SearchError::other(
                "knowledge_diff requires the git anchor source",
            ));
        }
        if repo_path.trim().is_empty() {
            return Err(SearchError::other(
                "knowledge_diff requires a resolvable repo path",
            ));
        }
        let mut from_ref = from_ref.to_string();
        let max_claims = if max_claims == 0 {
            KNOWLEDGE_DIFF_DEFAULT_MAX_CLAIMS
        } else {
            max_claims
        };
        if diff_mode == DiffMode::SinceDivergence {
            let anchors = self.git_anchors.as_ref().expect("checked above");
            let base = anchors
                .merge_base(repo_path, &from_ref, to_ref)
                .await
                .map_err(|e| SearchError::other(format!("resolve divergence point: {e}")))?;
            if base.trim().is_empty() {
                return Err(SearchError::other(format!(
                    "no common ancestor between {from_ref} and {to_ref}"
                )));
            }
            from_ref = base;
        }
        let from_set = ancestor_set_for(self, repo_path, &from_ref)
            .await
            .map_err(|e| SearchError::other(format!("resolve from ref: {e}")))?;
        let to_set = ancestor_set_for(self, repo_path, to_ref)
            .await
            .map_err(|e| SearchError::other(format!("resolve to ref: {e}")))?;

        let (rows_values, _) = git_bound_claims_for_project(self, project_name, 0).await?;
        let actor = actor.trim();
        let mut added: Vec<AsOfClaim> = Vec::new();
        let mut removed: Vec<AsOfClaim> = Vec::new();
        for row in rows::object_rows(&rows_values) {
            let introduced_in = rows::scalar_string(rows::field(row, "introduced_in_oid"))
                .trim()
                .to_string();
            let invalidated_at = rows::scalar_string(rows::field(row, "invalidated_at_oid"))
                .trim()
                .to_string();
            let hold_from = ibis_git::hold_at(&introduced_in, &invalidated_at, Some(&from_set));
            let hold_to = ibis_git::hold_at(&introduced_in, &invalidated_at, Some(&to_set));
            if hold_from == hold_to {
                continue;
            }
            let visibility = rows::scalar_string(rows::field(row, "visibility"))
                .trim()
                .to_string();
            let owner = rows::scalar_string(rows::field(row, "owner"))
                .trim()
                .to_string();
            if visibility == "personal" && owner != actor {
                continue;
            }
            let claim = AsOfClaim {
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
            };
            if hold_to {
                added.push(claim);
            } else {
                removed.push(claim);
            }
        }

        let anchors = self.git_anchors.as_ref().expect("checked above");
        let paths = anchors
            .diff_paths(repo_path, from_set.ref_oid(), to_set.ref_oid())
            .await
            .map_err(|e| SearchError::other(format!("diff paths between refs: {e}")))?;

        // Claim store 4d: Σ_R = tree(R) ∪ HOLDAt — tree additions/removals
        // join the HOLDAt delta (removals carry X from the removing commit).
        let (added, removed) = self
            .merge_tree_claims_into_diff(repo_path, &from_set, &to_set, actor, added, removed)
            .await?;

        let mut diff = KnowledgeDiff {
            from_ref: from_set.ref_oid().to_string(),
            to_ref: to_set.ref_oid().to_string(),
            added_claims: added,
            removed_claims: removed,
            changed_paths: paths,
            truncated: false,
            degraded: self.db.is_none(),
        };
        // Churn caps: truncate deterministically (added first, then removed)
        // and flag instead of dumping (the world-state spec).
        let total = diff.added_claims.len() + diff.removed_claims.len();
        if total > max_claims {
            diff.truncated = true;
            let keep_added = diff.added_claims.len().min(max_claims);
            diff.added_claims.truncate(keep_added);
            let keep_removed = (max_claims - keep_added).min(diff.removed_claims.len());
            diff.removed_claims.truncate(keep_removed);
        }
        if diff.changed_paths.len() > KNOWLEDGE_DIFF_MAX_PATHS {
            diff.truncated = true;
            diff.changed_paths.truncate(KNOWLEDGE_DIFF_MAX_PATHS);
        }
        Ok(diff)
    }
}
