//! Search-side ancestry + git-bound claim prefilter (
//! ). The DAG primitives (`AncestorSet`, `hold_at`, the
//! TTL cache, `GitAnchorSource`) live in `threski-git` (lane D); this module
//! carries the `SearchService`-level wrappers and the claim-row prefilter.
//!
//! Deltas: `ancestor_set_for` delegates to `threski_git::ancestor_set_for`
//! with the service's lazy [`threski_git::AncestorCache`]; shallow-clone
//! fail-closed surfaces as the typed `GitError::ShallowHistory` marker.

use threski_db::value::Value;

use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Builds (or fetches from the TTL cache) the ancestor set of R with exactly
/// one rev-list invocation per query.
pub(crate) async fn ancestor_set_for(
    service: &SearchService,
    repo_path: &str,
    reference: &str,
) -> Result<threski_git::AncestorSet> {
    let anchors = service.git_anchors.as_ref().ok_or_else(|| {
        SearchError::other("ancestor set requires a git anchor source, repo path, and ref")
    })?;
    if repo_path.trim().is_empty() || reference.trim().is_empty() {
        return Err(SearchError::other(
            "ancestor set requires a git anchor source, repo path, and ref",
        ));
    }
    threski_git::ancestor_set_for(
        anchors.as_ref(),
        service.ancestor_cache(),
        repo_path,
        reference,
    )
    .await
    .map_err(SearchError::Git)
}

/// Drops cached ancestor sets for a repo (call after fetch/sync/ingest so
/// bounds derived from stale history cannot leak).
pub fn invalidate_ancestors(service: &SearchService, repo_path: &str) {
    service.ancestor_cache().invalidate(repo_path);
}

/// Loads the git-bound claim rows for the ancestry prefilter (world-state
/// the spec step 2): hard LIMIT with overflow flag. Clone-only degraded
/// mode (the claim-store addendum): with no SurrealDB the HOLDAt channel
/// yields nothing and the tree channel still serves — degrade to empty rows
/// instead of failing the whole read.
pub(crate) async fn git_bound_claims_for_project(
    service: &SearchService,
    project_name: &str,
    limit: usize,
) -> Result<(Vec<threski_db::value::Value>, bool)> {
    let Some(db) = service.db.as_ref() else {
        return Ok((Vec::new(), false));
    };
    let mut limit = limit;
    if limit == 0 {
        limit = 2000;
    }
    let repo_id = super::repo_record_id(project_name);
    let sql = format!(
        "SELECT id, kind, title, status, visibility, owner, introduced_in_oid, invalidated_at_oid FROM memory WHERE project = {repo_id} AND introduced_in_oid != NONE LIMIT {};",
        limit + 1
    );
    let outcome = db
        .smart_query(&sql, threski_db::executor::Vars::new())
        .await
        .map_err(|e| SearchError::other(format!("query git-bound claims: {e}")))?;
    let mut normalized = rows::normalize_rows(&outcome).unwrap_or_default();
    let mut truncated = false;
    if normalized.len() > limit {
        normalized.truncate(limit);
        truncated = true;
    }
    Ok((normalized, truncated))
}

/// Coercion helper mirroring the row-passing shape: the prefilter rows are
/// projected objects consumed by the as_of/kdiff loops.
pub(crate) fn row_score(value: &Value) -> f64 {
    rows::numeric_float(value)
}
