//! Git-bound claim anchoring: the anchor-mode
//! decision for a new claim, orphaned-bound refresh, and the supersede
//! UPDATE-set builder. The production [`threski_git::GitAnchorSource`] lives in
//! the git crate; this module carries the search-side logic.

use threski_db::value::Value;

use super::add::AddMemoryParams;
use crate::error::{Result, SearchError};
use crate::search::SearchService;

/// Anchor mode: refuse dirty files, bind HEAD only on clean trees
/// (the world-state spec).
pub const ANCHOR_MODE_REQUIRE_CLEAN: &str = "require_clean";
/// Anchor mode: mark dirty provenance, keep the claim valid under HEAD rules.
pub const ANCHOR_MODE_WORKTREE: &str = "worktree";
/// Anchor mode: dirty provenance + promotion once the bytes land in a commit.
pub const ANCHOR_MODE_PROMOTE_ON_COMMIT: &str = "promote_on_commit";

/// `commitRefOid: extracts a commit OID from derived_from `commit:<oid>`
/// refs (angle-wrapped ids unwrapped).
pub(crate) fn commit_ref_oid(sources: &[String]) -> String {
    let prefix = "commit:";
    for source in sources {
        let source = source.trim();
        if !source.starts_with(prefix) || !threski_db::ids::is_safe_record_id(source) {
            continue;
        }
        let key = source
            .strip_prefix(prefix)
            .unwrap_or(source)
            .trim_start_matches('⟨')
            .trim_end_matches('⟩');
        if !key.is_empty() {
            return key.to_string();
        }
    }
    String::new()
}

/// `normalizeCommitOid: accepts a raw OID or a `commit:<oid>` record ref.
pub(crate) fn normalize_commit_oid(value: &str) -> String {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("commit:") {
        return rest
            .trim_start_matches('⟨')
            .trim_end_matches('⟩')
            .to_string();
    }
    value.to_string()
}

/// `hashBytes: sha256 hex of raw bytes — the content hash used by the
/// dirty-worktree provenance and the hot-ref cache population.
pub(crate) fn hash_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(data);
    let mut hex = String::with_capacity(sum.len() * 2);
    for byte in sum {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

impl SearchService {
    /// `ensureCommitKnown: rejects bindings to OIDs absent from the
    /// object database (the world-state spec). Git verification is
    /// preferred; the commit index (bound `$oid`) is the fallback when git
    /// itself is unavailable.
    pub(crate) async fn ensure_commit_known(&self, repo_path: &str, oid: &str) -> Result<()> {
        if let Some(anchors) = &self.git_anchors {
            if !repo_path.trim().is_empty() {
                match anchors.verify_commits(repo_path, &[oid.to_string()]).await {
                    Ok(missing) => {
                        if missing.contains(oid) {
                            return Err(SearchError::other(format!(
                                "unknown commit {oid}: absent from the git object database"
                            )));
                        }
                        return Ok(());
                    }
                    Err(_) => {
                        // git unavailable → fall through to the commit index.
                    }
                }
            }
        }
        let db = self.require_db()?;
        let sql = "SELECT id FROM commit WHERE hash = $oid LIMIT 1;";
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("oid".into(), Value::from_t(oid));
        let outcome = db
            .smart_query(sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("verify commit {oid}: {e}")))?;
        let count = crate::rows::normalize_rows(&outcome)
            .map(|rows| crate::rows::object_rows(&rows).len())
            .unwrap_or(0);
        if count == 0 {
            return Err(SearchError::other(format!(
                "unknown commit {oid}: not found in the commit index"
            )));
        }
        Ok(())
    }

    /// `resolveMemoryGitAnchor: decides the git binding for a new claim
    /// (the world-state spec +):
    /// 1. explicit introduced_in wins;
    /// 2. `commit:<oid>` source refs;
    /// 3. file-anchored claims follow the dirty-tree policy (clean → bind
    ///    HEAD, dirty → refuse or mark dirty depending on anchor_mode).
    pub(crate) async fn resolve_memory_git_anchor(
        &self,
        params: &mut AddMemoryParams,
    ) -> Result<()> {
        let Some(anchors) = self.git_anchors.clone() else {
            // Git binding disabled: commit refs stay pure provenance.
            return Ok(());
        };
        let mut mode = params.anchor_mode.trim().to_string();
        if mode.is_empty() {
            mode = ANCHOR_MODE_WORKTREE.to_string();
        }
        match mode.as_str() {
            ANCHOR_MODE_REQUIRE_CLEAN | ANCHOR_MODE_WORKTREE | ANCHOR_MODE_PROMOTE_ON_COMMIT => {}
            other => {
                return Err(SearchError::other(format!(
                    "invalid anchor_mode {other:?} (require_clean | worktree | promote_on_commit)"
                )));
            }
        }

        let mut oid = normalize_commit_oid(&params.introduced_in_oid);
        if oid.is_empty() {
            oid = commit_ref_oid(&params.derived_from);
        }
        if !oid.is_empty() {
            self.ensure_commit_known(&params.workspace_root, &oid)
                .await?;
            params.provenance_introduced_in_oid = oid;
            return Ok(());
        }

        if params.workspace_root.trim().is_empty() {
            // No repo context: leave the claim unanchored (excluded from
            // historical as_of under strict mode).
            return Ok(());
        }

        let path = params.provenance_source_path.trim().to_string();
        if path.is_empty() {
            // Nothing durable to anchor on; a superseding claim may still
            // bind its new row to a clean tip (the world-state spec).
            self.bind_supersede_tip(params).await;
            return Ok(());
        }
        let work_hash = anchors
            .work_file_hash(&params.workspace_root, &path)
            .await
            .map_err(|e| SearchError::other(format!("hash working file {path}: {e}")))?;
        let head_hash = anchors
            .head_file_hash(&params.workspace_root, &path)
            .await
            .map_err(|e| SearchError::other(format!("hash HEAD file {path}: {e}")))?;
        if let (Some(work), Some(head)) = (&work_hash, &head_hash) {
            if work == head {
                let head_oid = anchors
                    .rev_parse_head(&params.workspace_root)
                    .await
                    .map_err(|e| SearchError::other(format!("resolve HEAD for {path}: {e}")))?;
                let head_oid = head_oid.trim().to_string();
                if !head_oid.is_empty() {
                    params.provenance_introduced_in_oid = head_oid;
                    return Ok(());
                }
            }
        }
        // Dirty or untracked: never pretend I = HEAD.
        match mode.as_str() {
            ANCHOR_MODE_REQUIRE_CLEAN => Err(SearchError::other(format!(
                "git binding refused: {path} is dirty at HEAD (anchor_mode=require_clean)"
            ))),
            // worktree | promote_on_commit
            _ => {
                params.provenance_dirty = true;
                if let Some(work) = &work_hash {
                    params.provenance_worktree_hash = work.clone();
                }
                if mode == ANCHOR_MODE_PROMOTE_ON_COMMIT {
                    params.provenance_promote_on_commit = true;
                }
                Ok(())
            }
        }
    }

    /// `bindSupersedeTip: anchors a superseding claim to HEAD only when
    /// the tip is clean (the world-state spec).
    async fn bind_supersede_tip(&self, params: &mut AddMemoryParams) {
        if !params.provenance_introduced_in_oid.is_empty() || params.supersedes.trim().is_empty() {
            return;
        }
        let Some(anchors) = &self.git_anchors else {
            return;
        };
        if params.workspace_root.trim().is_empty() {
            return;
        }
        let Ok(clean) = anchors.is_clean(&params.workspace_root).await else {
            return;
        };
        if !clean {
            return;
        }
        if let Ok(head) = anchors.rev_parse_head(&params.workspace_root).await {
            let head = head.trim().to_string();
            if !head.is_empty() {
                params.provenance_introduced_in_oid = head;
            }
        }
    }

    /// `supersedeOldSet: the UPDATE set for the superseded (old) memory
    /// row in the AddMemory transaction, binding X(m) := HEAD when the git
    /// axis is available. The HEAD oid also binds as `$sup_head` (F-002).
    pub(crate) async fn supersede_old_set(&self, repo_path: &str) -> String {
        if let Some(anchors) = &self.git_anchors {
            if !repo_path.trim().is_empty() {
                if let Ok(head) = anchors.rev_parse_head(repo_path).await {
                    let head = head.trim().to_string();
                    if !head.is_empty() {
                        return format!(
                            "status = 'superseded', invalidated_at_oid = '{head}', provenance.invalidated_at_commit = {}, updated_at = time::now()",
                            threski_db::ids::format_record_id("commit", &head)
                        );
                    }
                }
            }
        }
        "status = 'superseded', updated_at = time::now()".to_string()
    }

    /// `RefreshOrphanedGitBounds: marks current git-bound memories stale
    /// when their introduced_in_oid no longer exists in the git object
    /// database (rewritten or dropped history; the world-state spec). Rows
    /// are flagged with provenance.orphan_oid and never deleted.
    pub async fn refresh_orphaned_git_bounds(&self, repo_path: &str) -> Result<usize> {
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
        let sql = "SELECT id AS memory_id, introduced_in_oid AS oid FROM memory WHERE introduced_in_oid != NONE;";
        let outcome = db
            .execute(sql)
            .await
            .map_err(|e| SearchError::other(format!("query git-bound memories: {e}")))?;
        let rows_values = crate::rows::normalize_rows(&outcome).unwrap_or_default();

        struct Bound {
            memory_id: String,
            oid: String,
        }
        let mut oids_seen = std::collections::HashSet::new();
        let mut oids: Vec<String> = Vec::new();
        let mut bounds: Vec<Bound> = Vec::new();
        for row in crate::rows::object_rows(&rows_values) {
            let memory_id = crate::rows::record_id(crate::rows::field(row, "memory_id"));
            let oid = crate::rows::scalar_string(crate::rows::field(row, "oid"))
                .trim()
                .to_string();
            if super::validate_memory_id(&memory_id).is_err() || oid.is_empty() {
                continue;
            }
            bounds.push(Bound {
                memory_id: memory_id.clone(),
                oid: oid.clone(),
            });
            if oids_seen.insert(oid.clone()) {
                oids.push(oid);
            }
        }
        if bounds.is_empty() {
            return Ok(0);
        }
        let missing = anchors
            .verify_commits(repo_path, &oids)
            .await
            .map_err(|e| SearchError::other(format!("verify git bounds: {e}")))?;
        let mut count = 0;
        for bound in &bounds {
            if !missing.contains(&bound.oid) {
                continue;
            }
            let sql = format!(
                "UPDATE {} SET status = 'stale', provenance.orphan_oid = $orphan_oid, updated_at = time::now() WHERE status = 'current';",
                bound.memory_id
            );
            let mut vars = threski_db::executor::Vars::new();
            vars.insert("orphan_oid".into(), Value::from_t(bound.oid.as_str()));
            if let Err(e) = db.smart_query(&sql, vars).await {
                return Err(SearchError::other(format!(
                    "mark memory {} stale for orphaned commit: {e}",
                    bound.memory_id
                )));
            }
            count += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_ref_oid_extracts_wrapped_keys() {
        let sources = vec![
            "source_file:readme".to_string(),
            "commit:⟨abc123⟩".to_string(),
        ];
        assert_eq!(commit_ref_oid(&sources), "abc123");
        assert_eq!(commit_ref_oid(&["memory:⟨x⟩".to_string()]), "");
        // Unsafe ids are skipped.
        assert_eq!(commit_ref_oid(&["commit:bad id".to_string()]), "");
    }

    #[test]
    fn normalize_commit_oid_unwraps() {
        assert_eq!(normalize_commit_oid("commit:⟨abc⟩"), "abc");
        assert_eq!(normalize_commit_oid("abc"), "abc");
        assert_eq!(normalize_commit_oid("  commit:abc  "), "abc");
    }
}
