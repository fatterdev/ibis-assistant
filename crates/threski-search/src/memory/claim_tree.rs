//! Tree claim files: parse `.threski/claims/*.md`
//! from `tree(ref)` and union them into as_of / ask at_ref / knowledge-diff
//! results (claim store 4d: Σ_R = tree claims ∪ HOLDAt claims).
//!
//! Deltas (PR #34): `cap_at_ref_results` keeps a 3-slot floor for
//! claim_file hits — a minimum, not a maximum: capacity ranked hits leave
//! unused is filled with additional tree claims, and unused floor returns to
//! ranked hits.

use std::collections::HashMap;

use super::memories_from_rows;
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::{ContextData, SearchResult, SearchService};

/// One parsed claim file from `tree(ref)` (the claim-store addendum: file
/// presence is the truth predicate for git-owned claims).
#[derive(Debug, Clone)]
pub(crate) struct TreeClaim {
    pub path: String,
    pub claim: threski_core::claims::Claim,
}

/// `at_ref` reply cap (the world-state spec).
pub(crate) const AT_REF_RESULT_CAP: usize = 15;

/// Tree-claim floor at the capped ask at_ref reply (claim store 4d parking
/// list): claim files are repo-SoT content, so a full ranked board must not
/// crowd them out. The floor is a minimum, not a maximum.
pub(crate) const CLAIM_FILE_RESERVED_SLOTS: usize = 3;

impl SearchService {
    /// Parses every `.threski/claims/*.md` file in `tree(ref)` (
    /// `treeClaimFiles`). Malformed files are skipped (the addendum: warn +
    /// skip, ingest never fails on one bad claim).
    pub(crate) async fn tree_claim_files(
        &self,
        repo_path: &str,
        reference: &str,
    ) -> Result<Vec<TreeClaim>> {
        let anchors = self
            .git_anchors
            .as_ref()
            .ok_or_else(|| SearchError::other("tree claims require the git anchor source"))?;
        let tree_paths = anchors
            .list_tree(repo_path, reference)
            .await
            .map_err(|e| SearchError::other(format!("list tree at {reference}: {e}")))?;
        let mut out = Vec::with_capacity(8);
        for path in &tree_paths {
            if !path.starts_with(&format!("{}/", threski_core::claims::CLAIMS_DIR))
                || !path.ends_with(".md")
            {
                continue;
            }
            let content = match anchors.file_at_ref(repo_path, reference, path).await {
                Ok(Some(bytes)) => bytes,
                _ => continue,
            };
            let claim = match threski_core::claims::parse(&content) {
                Ok(c) => c,
                Err(_) => continue,
            };
            out.push(TreeClaim {
                path: path.clone(),
                claim,
            });
        }
        Ok(out)
    }

    /// Maps a claim id to its memory record id.
    pub(crate) fn tree_claim_record_id(claim: &threski_core::claims::Claim) -> String {
        let record_id = threski_db::ids::format_record_id(super::TABLE_MEMORY, &claim.id);
        if !threski_db::ids::is_safe_record_id(&record_id) {
            return String::new();
        }
        record_id
    }

    /// Loads a memory row for privacy/status merge; `None` when absent. A
    /// `None` DB (clone-only degraded mode, the addendum) also yields `None` —
    /// callers handle the degraded owner/status defaults.
    pub(crate) async fn claim_row_by_id(&self, record_id: &str) -> Option<super::Memory> {
        let db = self.db.as_ref()?;
        let sql = format!("SELECT * FROM {record_id};");
        let outcome = db.execute(&sql).await.ok()?;
        let rows_values = rows::normalize_rows(&outcome)?;
        let memories = memories_from_rows(&rows_values);
        memories.into_iter().next()
    }

    /// I(m) = oldest commit touching the claim file.
    pub(crate) async fn file_introduced_in(&self, repo_path: &str, path: &str) -> String {
        let Some(anchors) = &self.git_anchors else {
            return String::new();
        };
        match anchors.file_history(repo_path, path).await {
            Ok(history) if !history.is_empty() => history[history.len() - 1].clone(),
            _ => String::new(),
        }
    }

    /// X(m) = newest commit touching a file that is no longer present at the
    /// queried ref.
    pub(crate) async fn file_removed_in(&self, repo_path: &str, path: &str) -> String {
        let Some(anchors) = &self.git_anchors else {
            return String::new();
        };
        match anchors.file_history(repo_path, path).await {
            Ok(history) if !history.is_empty() => history[0].clone(),
            _ => String::new(),
        }
    }

    /// Builds the AsOfClaim for a tree claim, enforcing personal visibility
    /// via the DB row (files carry no owner — the row is the owner registry;
    /// unattributable personal files stay hidden in DB-backed mode) and
    /// git-wins for kind/title (the addendum). Returns `None` when the claim
    /// must be skipped.
    ///
    /// Clone-only degraded mode (the addendum, `db == None`) has no owner
    /// registry: the file itself is then the only truth — team claims surface
    /// from the file, personal stubs surface title-only (a stub file carries
    /// no body, so the title IS the full content).
    pub(crate) async fn tree_claim_as_of(
        &self,
        repo_path: &str,
        tc: &TreeClaim,
        invalidated: &str,
        actor: &str,
    ) -> Option<crate::memory::asof::AsOfClaim> {
        let record_id = Self::tree_claim_record_id(&tc.claim);
        if record_id.is_empty() {
            return None;
        }
        let mut visibility = tc.claim.visibility.clone();
        let mut owner = String::new();
        let mut status = "current".to_string();
        if let Some(row) = self.claim_row_by_id(&record_id).await {
            owner = row.owner.clone();
            status = row.status.clone();
            if !row.visibility.is_empty() {
                visibility = row.visibility.clone();
            }
        } else if !invalidated.is_empty() {
            // Degraded removal path: the file is absent at the queried ref
            // and no DB row carries the reconcile status — report archived
            // (the withdrawal convention, the addendum) instead of a phantom
            // current.
            status = "archived".to_string();
        }
        if visibility == threski_core::claims::VISIBILITY_PERSONAL
            && owner != actor.trim()
            && self.db.is_some()
        {
            return None;
        }
        Some(crate::memory::asof::AsOfClaim {
            id: record_id,
            kind: tc.claim.kind.clone(),
            title: tc.claim.title.clone(),
            status,
            visibility,
            owner,
            introduced_in: self.file_introduced_in(repo_path, &tc.path).await,
            invalidated_at: invalidated.to_string(),
            source_kind: "claim_file".to_string(),
        })
    }

    /// Unions `tree(ref)` claims into the HOLDAt result (
    /// `mergeTreeClaimsIntoAsOf`), deduped by record id.
    pub(crate) async fn merge_tree_claims_into_as_of(
        &self,
        repo_path: &str,
        reference: &str,
        actor: &str,
        mut out: Vec<crate::memory::asof::AsOfClaim>,
    ) -> Result<Vec<crate::memory::asof::AsOfClaim>> {
        let tree_claims = self.tree_claim_files(repo_path, reference).await?;
        let mut seen: HashMap<String, bool> = HashMap::with_capacity(out.len());
        for c in &out {
            seen.insert(c.id.clone(), true);
        }
        for tc in &tree_claims {
            let entry = self.tree_claim_as_of(repo_path, tc, "", actor).await;
            if let Some(entry) = entry {
                if seen.contains_key(&entry.id) {
                    continue;
                }
                seen.insert(entry.id.clone(), true);
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Unions `tree(ref)` claim files into the at_ref ask results (claim
    /// store 4d): entries already returned by the vector/HOLDAt channel are
    /// kept as-is; new tree claims append unranked (score 0) as claim_file
    /// hits carrying the file body (stub files title-only, the addendum).
    pub(crate) async fn merge_tree_claims_into_ask_ref(
        &self,
        repo_path: &str,
        reference: &str,
        actor: &str,
        mut results: Vec<SearchResult>,
    ) -> Result<Vec<SearchResult>> {
        let tree_claims = self.tree_claim_files(repo_path, reference).await?;
        let mut seen: HashMap<String, bool> = HashMap::with_capacity(results.len());
        for r in &results {
            if r.source_kind == super::TABLE_MEMORY {
                seen.insert(r.id.clone(), true);
            }
        }
        for tc in &tree_claims {
            let record_id = Self::tree_claim_record_id(&tc.claim);
            if record_id.is_empty() || seen.contains_key(&record_id) {
                continue;
            }
            let entry = self.tree_claim_as_of(repo_path, tc, "", actor).await;
            let Some(entry) = entry else {
                continue;
            };
            seen.insert(record_id.clone(), true);
            results.push(claim_file_result(&record_id, tc, &entry, reference));
        }
        Ok(results)
    }

    /// Unions tree additions/removals into the HOLDAt diff (claim store 4d):
    /// files present at B but not A are added claims, files present at A but
    /// not B are removed claims carrying X from the removing commit; entries
    /// already listed by HOLDAt (whose bounds were reconciled from the same
    /// file history) are kept as-is to avoid duplicates
    ///
    pub(crate) async fn merge_tree_claims_into_diff(
        &self,
        repo_path: &str,
        from_set: &threski_git::AncestorSet,
        to_set: &threski_git::AncestorSet,
        actor: &str,
        mut added: Vec<crate::memory::asof::AsOfClaim>,
        mut removed: Vec<crate::memory::asof::AsOfClaim>,
    ) -> Result<(
        Vec<crate::memory::asof::AsOfClaim>,
        Vec<crate::memory::asof::AsOfClaim>,
    )> {
        let tree_from = self.tree_claim_files(repo_path, from_set.ref_oid()).await?;
        let tree_to = self.tree_claim_files(repo_path, to_set.ref_oid()).await?;
        let mut set_from: HashMap<String, TreeClaim> = HashMap::with_capacity(tree_from.len());
        for tc in tree_from {
            if let Some(id) = Self::tree_claim_record_id(&tc.claim).into_option_if_safe() {
                set_from.insert(id, tc);
            }
        }
        let mut set_to: HashMap<String, TreeClaim> = HashMap::with_capacity(tree_to.len());
        for tc in tree_to {
            if let Some(id) = Self::tree_claim_record_id(&tc.claim).into_option_if_safe() {
                set_to.insert(id, tc);
            }
        }
        let hold_added: std::collections::HashSet<String> =
            added.iter().map(|c| c.id.clone()).collect();
        let hold_removed: std::collections::HashSet<String> =
            removed.iter().map(|c| c.id.clone()).collect();
        for (id, tc) in &set_from {
            if set_to.contains_key(id) || hold_removed.contains(id) {
                continue;
            }
            let removed_in = self.file_removed_in(repo_path, &tc.path).await;
            let entry = self
                .tree_claim_as_of(repo_path, tc, &removed_in, actor)
                .await;
            if let Some(entry) = entry {
                removed.push(entry);
            }
        }
        for (id, tc) in &set_to {
            if set_from.contains_key(id) || hold_added.contains(id) || hold_removed.contains(id) {
                continue;
            }
            let entry = self.tree_claim_as_of(repo_path, tc, "", actor).await;
            if let Some(entry) = entry {
                added.push(entry);
            }
        }
        Ok((added, removed))
    }
}

/// Renders a tree claim as a claim_file ask hit: the file body is the
/// content, except stub files which show title only (the addendum /
/// acceptance test 10 — the title IS the stub's visible content)
///
pub(crate) fn claim_file_result(
    record_id: &str,
    tc: &TreeClaim,
    entry: &crate::memory::asof::AsOfClaim,
    as_of_ref: &str,
) -> SearchResult {
    let content = if entry.visibility == threski_core::claims::VISIBILITY_PERSONAL
        && tc.claim.redacted == threski_core::claims::REDACTED_STUB
    {
        tc.claim.title.clone()
    } else {
        tc.claim.body.clone()
    };
    SearchResult {
        id: record_id.to_string(),
        content,
        score: 0.0,
        is_current: false,
        source_kind: "claim_file".to_string(),
        status: entry.status.clone(),
        kind: entry.kind.clone(),
        as_of_ref: as_of_ref.to_string(),
        path: String::new(),
        commit_hash: String::new(),
        content_source: String::new(),
        context: ContextData::default(),
    }
}

/// Sorts by score (stable) and applies [`AT_REF_RESULT_CAP`]. Tree claim_file
/// hits keep a floor of [`CLAIM_FILE_RESERVED_SLOTS`] slots when the cap
/// bites: ranked hits have priority above the floor, tree claims beyond the
/// floor fill capacity ranked hits leave unused.
pub(crate) fn cap_at_ref_results(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    if results.len() <= AT_REF_RESULT_CAP {
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        return results;
    }
    let mut tree: Vec<SearchResult> = Vec::with_capacity(results.len());
    let mut ranked: Vec<SearchResult> = Vec::with_capacity(results.len());
    for r in results.drain(..) {
        if r.source_kind == "claim_file" {
            tree.push(r);
        } else {
            ranked.push(r);
        }
    }
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut tree_floor = CLAIM_FILE_RESERVED_SLOTS;
    if tree.len() < tree_floor {
        tree_floor = tree.len();
    }
    let mut keep_ranked = ranked.len();
    let max_ranked = AT_REF_RESULT_CAP - tree_floor;
    if keep_ranked > max_ranked {
        keep_ranked = max_ranked;
    }
    let mut keep_tree = tree.len();
    let max_tree = AT_REF_RESULT_CAP - keep_ranked;
    if keep_tree > max_tree {
        keep_tree = max_tree;
    }
    let mut out = Vec::with_capacity(AT_REF_RESULT_CAP);
    out.extend(ranked.drain(..keep_ranked));
    out.extend(tree.drain(..keep_tree));
    out
}

/// Scenario: helper shape: an empty record id means "unsafe" — represent it as
/// `None` so callers can skip.
trait IntoOptionIfSafe {
    fn into_option_if_safe(self) -> Option<String>;
}

impl IntoOptionIfSafe for String {
    fn into_option_if_safe(self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, score: f64, source_kind: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            score,
            source_kind: source_kind.to_string(),
            ..Default::default()
        }
    }

    /// `TestCapAtRefResultsUnderCapSortsOnly.
    #[test]
    fn cap_under_cap_sorts_only() {
        let results = vec![
            result("low", 0.1, "memory"),
            result("tree", 0.0, "claim_file"),
            result("high", 0.9, "commit_chunk"),
        ];
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, "high");
        assert_eq!(out[1].id, "low");
        assert_eq!(out[2].id, "tree");
    }

    /// `TestCapAtRefResultsReservesClaimFileSlots.
    #[test]
    fn cap_reserves_claim_file_slots() {
        let mut results = Vec::with_capacity(AT_REF_RESULT_CAP + 7);
        for i in 0..AT_REF_RESULT_CAP + 5 {
            results.push(result(
                &format!("m{i:02}"),
                0.99 - i as f64 * 0.01,
                "memory",
            ));
        }
        for i in 0..5 {
            results.push(result(&format!("tree{i}"), 0.0, "claim_file"));
        }
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), AT_REF_RESULT_CAP);
        for (i, r) in out.iter().enumerate() {
            if i < AT_REF_RESULT_CAP - CLAIM_FILE_RESERVED_SLOTS {
                assert_eq!(r.id, format!("m{i:02}"), "ranked slot {i}");
            } else {
                assert_eq!(
                    r.id,
                    format!(
                        "tree{}",
                        i - (AT_REF_RESULT_CAP - CLAIM_FILE_RESERVED_SLOTS)
                    ),
                    "reserved slot {i}"
                );
            }
        }
    }

    /// `TestCapAtRefResultsNoReservationWithoutTreeHits.
    #[test]
    fn cap_no_reservation_without_tree_hits() {
        let mut results = Vec::with_capacity(AT_REF_RESULT_CAP + 5);
        for i in 0..AT_REF_RESULT_CAP + 5 {
            results.push(result(
                &format!("m{i:02}"),
                0.99 - i as f64 * 0.01,
                "memory",
            ));
        }
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), AT_REF_RESULT_CAP);
        for (i, r) in out.iter().enumerate() {
            assert_eq!(r.id, format!("m{i:02}"), "slot {i}");
        }
    }

    /// `TestCapAtRefResultsTreeClaimsFillUnusedCapacity — the floor is a
    /// minimum, not a maximum.
    #[test]
    fn cap_tree_claims_fill_unused_capacity() {
        let mut results = Vec::with_capacity(16);
        for i in 0..10 {
            results.push(result(
                &format!("m{i:02}"),
                0.99 - i as f64 * 0.01,
                "memory",
            ));
        }
        for i in 0..6 {
            results.push(result(&format!("tree{i}"), 0.0, "claim_file"));
        }
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), AT_REF_RESULT_CAP);
        for (i, r) in out.iter().enumerate() {
            let expected = if i < 10 {
                format!("m{i:02}")
            } else {
                format!("tree{}", i - 10)
            };
            assert_eq!(r.id, expected);
        }
    }

    /// `TestCapAtRefResultsTreeOnlyStoreFillsReply.
    #[test]
    fn cap_tree_only_store_fills_reply() {
        let mut results = Vec::with_capacity(16);
        for i in 0..16 {
            results.push(result(&format!("tree{i}"), 0.0, "claim_file"));
        }
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), AT_REF_RESULT_CAP);
        for (i, r) in out.iter().enumerate() {
            assert_eq!(r.id, format!("tree{i}"));
            assert_eq!(r.source_kind, "claim_file");
        }
    }

    /// `TestCapAtRefResultsRankedBackfillsUnusedFloor.
    #[test]
    fn cap_ranked_backfills_unused_floor() {
        let mut results = Vec::with_capacity(22);
        for i in 0..20 {
            results.push(result(
                &format!("m{i:02}"),
                0.99 - i as f64 * 0.01,
                "memory",
            ));
        }
        for i in 0..2 {
            results.push(result(&format!("tree{i}"), 0.0, "claim_file"));
        }
        let out = cap_at_ref_results(results);
        assert_eq!(out.len(), AT_REF_RESULT_CAP);
        for (i, r) in out.iter().enumerate() {
            if i < AT_REF_RESULT_CAP - 2 {
                assert_eq!(r.id, format!("m{i:02}"));
            }
        }
        assert_eq!(out[AT_REF_RESULT_CAP - 2].id, "tree0");
        assert_eq!(out[AT_REF_RESULT_CAP - 1].id, "tree1");
    }
}
