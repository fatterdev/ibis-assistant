//! Claim pointer notes reconcile: rebuild
//! `refs/notes/ibis-claims` from the Surreal claim index (claim store
//! the addendum, phase 5a). Notes are cross-links enriching commit meaning:
//! pointer only (`claim=<id>` lines), never claim storage, never required
//! for correctness, and rebuildable from the DB alone.
//!
//! The production `GitNotesSource` lives in `ibis-git` (lane D); this module
//! carries the reconcile logic. Every content-bearing value binds (F-002).

use std::collections::HashMap;

use super::{memory_record_key, STORE_GIT, TABLE_MEMORY};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Caps the notes reconcile on distinct anchor commits so a pathological
/// claim index cannot produce an unbounded notes write (
/// `claimNotesMaxAnchors`).
pub(crate) const CLAIM_NOTES_MAX_ANCHORS: usize = 2000;

/// Reports one `RebuildGitNotes` run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ClaimNotesResult {
    pub notes_ref: String,
    pub anchors: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unresolved_anchors: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
    pub dry_run: bool,
}

impl SearchService {
    /// Reconciles the notes ref from the Surreal claim index (
    /// `RebuildGitNotes`). Every historical commit bound to a git-owned
    /// claim — introduced_in_oid and invalidated_at_oid anchors — gets one
    /// pointer note listing `claim=<id>` lines (sorted). Existing notes with
    /// the desired content are kept; mismatched notes are rewritten; notes
    /// on commits no row references are removed; anchors absent from the
    /// object database (rewritten history) are skipped and counted as
    /// unresolved.
    pub async fn rebuild_git_notes(
        &self,
        repo_path: &str,
        project_name: &str,
        dry_run: bool,
    ) -> Result<ClaimNotesResult> {
        let mut result = ClaimNotesResult {
            notes_ref: crate::NOTES_REF.to_string(),
            dry_run,
            ..Default::default()
        };
        let db = self.require_db()?;
        if repo_path.is_empty() || project_name.is_empty() {
            return Ok(result);
        }
        let (Some(anchors), Some(notes)) = (&self.git_anchors, &self.notes) else {
            return Ok(result);
        };
        // Git-owned rows only (the addendum): the store field is authoritative;
        // legacy rows without it fall back to the kind/visibility map.
        // Surreal-only rows are filtered in SQL so they cannot consume the
        // anchor cap.
        let repo_id = super::repo_record_id(project_name);
        let sql = format!(
            "SELECT id, kind, visibility, store, introduced_in_oid, invalidated_at_oid FROM {TABLE_MEMORY} WHERE project = {repo_id} AND (introduced_in_oid != NONE OR invalidated_at_oid != NONE) AND kind IN ['{0}','{1}','{2}'] AND (store = '{STORE_GIT}' OR store IS NONE) ORDER BY id LIMIT {limit};",
            ibis_core::claims::KIND_DECISION,
            ibis_core::claims::KIND_RULE,
            ibis_core::claims::KIND_FIX,
            limit = CLAIM_NOTES_MAX_ANCHORS + 1,
        );
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("git notes rebuild: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        // desired: anchor commit → claim-id set (the note content). Rows
        // arrive in ORDER BY id order; the cap applies to distinct anchors
        // AFTER the store/policy filter (a row contributes at most two
        // anchors).
        let mut desired: HashMap<String, Vec<String>> = HashMap::new();
        let mut anchor_order: Vec<String> = Vec::new();
        for row in rows::object_rows(&rows_values) {
            let kind = rows::scalar_string(rows::field(row, "kind"))
                .trim()
                .to_string();
            let visibility = rows::scalar_string(rows::field(row, "visibility"))
                .trim()
                .to_string();
            let store = rows::scalar_string(rows::field(row, "store"))
                .trim()
                .to_string();
            if !store.is_empty() {
                if store != STORE_GIT {
                    continue;
                }
            } else if self.claim_store_decision(&kind, &visibility) != STORE_GIT {
                continue;
            }
            let mut id = rows::scalar_string(rows::field(row, "id"))
                .trim()
                .to_string();
            if id.is_empty() {
                id = rows::record_id(rows::field(row, "id"));
            }
            let key = match memory_record_key(&id) {
                Ok(k) => k,
                Err(_) => continue,
            };
            for oid in [
                rows::scalar_string(rows::field(row, "introduced_in_oid"))
                    .trim()
                    .to_string(),
                rows::scalar_string(rows::field(row, "invalidated_at_oid"))
                    .trim()
                    .to_string(),
            ] {
                if oid.is_empty() {
                    continue;
                }
                if !desired.contains_key(&oid) {
                    if desired.len() >= CLAIM_NOTES_MAX_ANCHORS {
                        result.truncated = true;
                        continue;
                    }
                    desired.insert(oid.clone(), Vec::new());
                    anchor_order.push(oid.clone());
                }
                let entry = desired.get_mut(&oid).expect("just inserted");
                if !entry.contains(&key) {
                    entry.push(key.clone());
                }
            }
        }
        // Anchors absent from the object database (rewritten/dropped history)
        // cannot be annotated: skip and count (the world-state spec honesty).
        if !anchor_order.is_empty() {
            let missing = anchors
                .verify_commits(repo_path, &anchor_order)
                .await
                .map_err(|e| {
                    SearchError::other(format!("git notes rebuild: verify anchors: {e}"))
                })?;
            for oid in &anchor_order {
                if !missing.contains(oid) {
                    continue;
                }
                desired.remove(oid);
                result.unresolved_anchors += 1;
            }
        }
        result.anchors = desired.len();
        let current = notes
            .notes_list(repo_path, crate::NOTES_REF)
            .await
            .map_err(|e| SearchError::other(format!("git notes rebuild: {e}")))?;
        let mut oids: Vec<String> = desired.keys().cloned().collect();
        oids.sort();
        for oid in &oids {
            let ids = &desired[oid];
            let want = ibis_git::claim_note_message(ids.iter().map(String::as_str));
            match current.get(oid) {
                Some(have) if have == &want => continue,
                Some(_) => result.updated += 1,
                None => result.added += 1,
            }
            if !dry_run {
                notes
                    .notes_add(repo_path, crate::NOTES_REF, oid, &want)
                    .await?;
            }
        }
        let mut stale: Vec<String> = current
            .keys()
            .filter(|oid| !desired.contains_key(*oid))
            .cloned()
            .collect();
        stale.sort();
        for oid in &stale {
            result.removed += 1;
            if !dry_run {
                notes.notes_remove(repo_path, crate::NOTES_REF, oid).await?;
            }
        }
        Ok(result)
    }
}
