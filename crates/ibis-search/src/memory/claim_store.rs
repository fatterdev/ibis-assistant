//! Claim store: the git-tree SoT for
//! decision/rule/fix claims. Files commit BEFORE the Surreal row so a DB
//! failure leaves the file to be reconciled by [`SearchService::index_claim_store`]
//! — never the reverse (the claim-store addendum one-way rule).
//!
//! Deltas (PR #34 + D-A12):
//! - every claim commit self-annotates `refs/notes/ibis-claims` with a
//!   `claim=<id>` pointer note (best-effort — see [`claim_notes`]);
//! - commit identity provisioning lives in `ibis_git::GitCommitter::set_commit_identity`.

use std::collections::HashMap;

use ibis_db::value::Value;

use super::{
    add::AddMemoryParams, first_line, memories_from_rows, memory_record_key, repo_record_id,
    Bindings, Memory, ES_HAS_MEMORY, STORE_GIT, STORE_SURREAL, TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::rows;
use crate::search::SearchService;

/// Personal-claims policy levels (the claim-store addendum). The empty value
/// resolves by runtime mode at wiring time: db_only on team/server, stub in
/// personal mode.
pub const PERSONAL_CLAIMS_DB_ONLY: &str = "db_only";
pub const PERSONAL_CLAIMS_STUB: &str = "stub";
pub const PERSONAL_CLAIMS_IN_REPO: &str = "in_repo";

/// Maps the configured (possibly empty) policy to an effective level for the
/// given runtime mode.
pub fn resolve_personal_claims_policy(policy: &str, runtime_mode: &str) -> String {
    match policy {
        PERSONAL_CLAIMS_DB_ONLY | PERSONAL_CLAIMS_STUB | PERSONAL_CLAIMS_IN_REPO => {
            policy.to_string()
        }
        _ => {
            if runtime_mode == "server" {
                PERSONAL_CLAIMS_DB_ONLY.to_string()
            } else {
                PERSONAL_CLAIMS_STUB.to_string()
            }
        }
    }
}

impl SearchService {
    /// Claim store scope map (the addendum): decision/rule/fix with team
    /// visibility are git-owned; git-eligible personal rows follow the
    /// personal-claims policy (stub/in_repo commit a file, db_only stays
    /// Surreal-only); everything else stays Surreal-only
    ///
    pub(crate) fn claim_store_decision(&self, kind: &str, visibility: &str) -> String {
        match kind {
            ibis_core::claims::KIND_DECISION
            | ibis_core::claims::KIND_RULE
            | ibis_core::claims::KIND_FIX => {}
            _ => return STORE_SURREAL.to_string(),
        }
        if visibility == ibis_core::claims::VISIBILITY_TEAM {
            return STORE_GIT.to_string();
        }
        if visibility == ibis_core::claims::VISIBILITY_PERSONAL
            && (self.personal_claims_policy == PERSONAL_CLAIMS_STUB
                || self.personal_claims_policy == PERSONAL_CLAIMS_IN_REPO)
        {
            return STORE_GIT.to_string();
        }
        STORE_SURREAL.to_string()
    }

    /// Renders a git-owned claim into `.ibis/claims/` and commits it
    /// (path-scoped: unrelated dirty files are untouched). Returns the
    /// claims commit OID and the applied personal-claims policy level (empty
    /// for team rows), or `""` when the claim stays Surreal-only. When the
    /// superseded memory is git-owned the old file is deleted in the same
    /// commit.
    pub(crate) async fn commit_claim_for_memory(
        &self,
        params: &mut AddMemoryParams,
        id: &str,
        visibility: &str,
        superseded: Option<&Memory>,
    ) -> Result<(String, String)> {
        if params.workspace_root.is_empty()
            || self.claim_writer.is_none()
            || self.git_anchors.is_none()
        {
            if let Some(superseded) = superseded {
                if superseded.store == STORE_GIT {
                    return Err(SearchError::other(format!(
                        "git-owned claim {} can only be superseded by a commit in the project repository; no claim writer available for {}",
                        superseded.id, params.project_name
                    )));
                }
            }
            return Ok((String::new(), String::new()));
        }
        if params.anchor_mode == super::gitbound::ANCHOR_MODE_REQUIRE_CLEAN {
            let anchors = self.git_anchors.as_ref().unwrap();
            let clean = anchors
                .is_clean(&params.workspace_root)
                .await
                .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
            if !clean {
                return Err(SearchError::other(format!(
                    "claim store refused: working tree of {} is dirty (anchor_mode=require_clean)",
                    params.project_name
                )));
            }
        }
        let key = memory_record_key(id)?;
        let mut title = params.title.trim().to_string();
        if title.is_empty() {
            title = first_line(&params.content, 80);
        }
        let mut policy_applied = String::new();
        let mut claim = ibis_core::claims::Claim {
            schema: ibis_core::claims::SCHEMA_VERSION,
            id: key.clone(),
            kind: params.kind.clone(),
            title,
            visibility: visibility.to_string(),
            related_paths: Vec::new(),
            supersedes: String::new(),
            created: Some(chrono::Utc::now()),
            redacted: String::new(),
            body: String::new(),
        };
        if visibility == ibis_core::claims::VISIBILITY_PERSONAL {
            policy_applied = self.personal_claims_policy.clone();
            if self.personal_claims_policy == PERSONAL_CLAIMS_STUB {
                claim.redacted = ibis_core::claims::REDACTED_STUB.to_string();
                claim.body = String::new();
            } else {
                claim.body = params.content.clone();
            }
        } else {
            claim.body = params.content.clone();
        }
        let path = params.provenance_source_path.trim();
        if !path.is_empty() {
            claim.related_paths = vec![path.to_string()];
        }
        let mut deletes: Vec<String> = Vec::new();
        if let Some(superseded) = superseded {
            if superseded.store == STORE_GIT {
                let old_key = memory_record_key(&superseded.id)?;
                let old_path = ibis_core::claims::path(&old_key)
                    .map_err(|e| SearchError::other(format!("{e}")))?;
                claim.supersedes = old_key;
                deletes.push(old_path);
            }
        }
        let data =
            ibis_core::claims::render(&claim).map_err(|e| SearchError::other(format!("{e}")))?;
        let claim_path =
            ibis_core::claims::path(&key).map_err(|e| SearchError::other(format!("{e}")))?;
        let mut message = format!("chore(claims): add {key}");
        if !deletes.is_empty() {
            message = format!("chore(claims): add {key} supersedes {}", claim.supersedes);
        }
        let mut writes: HashMap<String, Vec<u8>> = HashMap::new();
        writes.insert(claim_path, data);
        let writer = self.claim_writer.as_ref().unwrap();
        let oid = writer
            .commit_claim(&params.workspace_root, &message, &writes, &deletes)
            .await
            .map_err(|e| SearchError::other(format!("claim store commit: {e}")))?;
        // Post-claim-commit notes hook (the addendum / PR #34): the commit
        // anchors the new claim (I) and, on supersession, the old claim (its
        // file is removed here, so X(old) lands on this commit once
        // supersedeOldSet runs against the new HEAD).
        let mut note_ids = vec![key.as_str(), claim.supersedes.as_str()];
        note_ids.retain(|s| !s.is_empty());
        self.annotate_claim_commit_notes(&params.workspace_root, &oid, &note_ids)
            .await;
        Ok((oid, policy_applied))
    }

    /// Post-claim-commit notes hook (the claim-store addendum, PR #34):
    /// writes the pointer note for a just-created claims commit so
    /// `refs/notes/ibis-claims` stays fresh between explicit rebuilds.
    /// Best-effort by design — notes are cross-links, never required for
    /// correctness.
    pub(crate) async fn annotate_claim_commit_notes(
        &self,
        repo_path: &str,
        oid: &str,
        claim_ids: &[&str],
    ) {
        let Some(notes) = &self.notes else {
            return;
        };
        if repo_path.is_empty() || oid.is_empty() {
            return;
        }
        let mut ids: Vec<String> = claim_ids
            .iter()
            .filter(|id| !id.is_empty())
            .map(|id| id.to_string())
            .collect();
        if ids.is_empty() {
            return;
        }
        ids.sort();
        ids.dedup();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let _ = notes
            .notes_add(
                repo_path,
                crate::NOTES_REF,
                oid,
                &ibis_git::claim_note_message(refs),
            )
            .await;
    }

    /// Reconciles the Surreal index with `tree(HEAD)`'s `.ibis/claims/`
    /// (the addendum ingest step and git-wins rule): present claim files
    /// upsert rows with `store='git'` and I/X denormalized from file history;
    /// `store='git'` rows whose file vanished are marked archived with X from
    /// history (withdrawal). Malformed files are skipped, never fatal
    /// Content binds (F-002).
    pub async fn index_claim_store(&self, repo_path: &str, project_name: &str) -> Result<usize> {
        self.require_db()?;
        if repo_path.is_empty() || project_name.is_empty() || self.git_anchors.is_none() {
            return Ok(0);
        }
        let anchors = self.git_anchors.as_ref().unwrap();
        let head = anchors
            .rev_parse_head(repo_path)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let tree_paths = anchors
            .list_tree(repo_path, &head)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let mut present: HashMap<String, bool> = HashMap::new();
        let mut count = 0usize;
        for path in &tree_paths {
            if !path.starts_with(&format!("{}/", ibis_core::claims::CLAIMS_DIR))
                || !path.ends_with(".md")
            {
                continue;
            }
            let content = match anchors.file_at_ref(repo_path, &head, path).await {
                Ok(Some(bytes)) => bytes,
                _ => continue,
            };
            let claim = match ibis_core::claims::parse(&content) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let record_id = ibis_db::ids::format_record_id(TABLE_MEMORY, &claim.id);
            if !ibis_db::ids::is_safe_record_id(&record_id) {
                continue;
            }
            present.insert(claim.id.clone(), true);
            let history = anchors
                .file_history(repo_path, path)
                .await
                .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
            let introduced = history.last().cloned().unwrap_or_default();
            let reconciled = self
                .reconcile_claim_row(&claim, &record_id, project_name, &introduced)
                .await?;
            if reconciled {
                count += 1;
            }
        }
        let withdrawn = self
            .withdraw_missing_claims(repo_path, project_name, &present)
            .await?;
        Ok(count + withdrawn)
    }

    /// Upserts one claim file's index row. Creates
    /// a minimal row when the claim id is unknown (hand-written or
    /// third-party files are legitimate writes — the repo is SoT), otherwise
    /// binds I/X denormalized from file history.
    async fn reconcile_claim_row(
        &self,
        claim: &ibis_core::claims::Claim,
        record_id: &str,
        project_name: &str,
        introduced: &str,
    ) -> Result<bool> {
        let db = self.require_db()?;
        let sql = format!("SELECT * FROM {record_id};");
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let existing = memories_from_rows(&rows::normalize_rows(&outcome).unwrap_or_default());
        let mut introduced_bindings: Bindings = Vec::new();
        let mut introduced_sets = String::new();
        if !introduced.is_empty() {
            introduced_bindings.push(("introduced".into(), Value::from_t(introduced)));
            introduced_sets = format!(
                ", introduced_in_oid = $introduced, provenance.introduced_in_commit = {}",
                ibis_db::ids::format_record_id("commit", introduced)
            );
        }
        if existing.is_empty() {
            let mut visibility = claim.visibility.clone();
            if visibility != ibis_core::claims::VISIBILITY_TEAM
                && visibility != ibis_core::claims::VISIBILITY_PERSONAL
            {
                visibility = ibis_core::claims::VISIBILITY_TEAM.to_string();
            }
            let mut policy_applied = String::new();
            if claim.redacted == ibis_core::claims::REDACTED_STUB {
                policy_applied = PERSONAL_CLAIMS_STUB.to_string();
            } else if visibility == ibis_core::claims::VISIBILITY_PERSONAL {
                policy_applied = PERSONAL_CLAIMS_IN_REPO.to_string();
            }
            let repo_id = repo_record_id(project_name);
            let mut bindings: Bindings = vec![
                ("kind".into(), Value::from_t(claim.kind.as_str())),
                ("title".into(), Value::from_t(claim.title.as_str())),
                ("content".into(), Value::from_t(claim.body.as_str())),
            ];
            let mut sets = String::from("kind = $kind, title = $title, content = $content");
            if !policy_applied.is_empty() {
                bindings.push(("policy".into(), Value::from_t(policy_applied.as_str())));
                sets.push_str(", policy_applied = $policy");
            }
            if !introduced_bindings.is_empty() {
                bindings.extend(introduced_bindings);
            }
            let query = format!(
                "BEGIN TRANSACTION;\nUPSERT {repo_id} SET name = $repo_name;\nLET $m = CREATE {record_id} SET {sets}, store = '{STORE_GIT}', visibility = '{visibility}', owner = '', status = 'current', project = {repo_id}{introduced_sets}, created_at = time::now(), updated_at = time::now();\nRELATE {repo_id}->{ES_HAS_MEMORY}->$m;\nCOMMIT TRANSACTION;"
            );
            let mut create_bindings = super::bind(bindings);
            create_bindings.insert(
                "repo_name".to_string(),
                ibis_db::value::Value::from_t(project_name),
            );
            db.smart_query(&query, create_bindings)
                .await
                .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
            return Ok(true);
        }
        if introduced_bindings.is_empty() {
            return Ok(false);
        }
        let sql = format!(
            "UPDATE {record_id} SET store = '{STORE_GIT}'{introduced_sets}, updated_at = time::now();"
        );
        db.smart_query(&sql, super::bind(introduced_bindings))
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        Ok(true)
    }

    /// Flips `store='git'` rows whose claim file is no longer present in
    /// `tree(HEAD)` to archived with X from file history (the addendum
    /// withdrawal). Only rows still 'current' are touched
    ///
    async fn withdraw_missing_claims(
        &self,
        repo_path: &str,
        project_name: &str,
        present: &HashMap<String, bool>,
    ) -> Result<usize> {
        let db = self.require_db()?;
        let anchors = self.git_anchors.as_ref().unwrap();
        let repo_id = repo_record_id(project_name);
        let sql = format!(
            "SELECT id, title FROM {TABLE_MEMORY} WHERE store = '{STORE_GIT}' AND project = {repo_id} AND status = 'current';"
        );
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let mut withdrawn = 0usize;
        for row in rows::object_rows(&rows_values) {
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
            if present.get(&key).copied().unwrap_or(false) {
                continue;
            }
            let path = match ibis_core::claims::path(&key) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let history = match anchors.file_history(repo_path, &path).await {
                Ok(h) => h,
                Err(_) => continue,
            };
            if history.is_empty() {
                continue;
            }
            let last_touch = history[0].clone();
            let sql = format!(
                "UPDATE {id} SET status = 'archived', invalidated_at_oid = $last_touch, provenance.invalidated_at_commit = {}, updated_at = time::now();",
                ibis_db::ids::format_record_id("commit", &last_touch)
            );
            let mut vars = ibis_db::executor::Vars::new();
            vars.insert("last_touch".into(), Value::from_t(last_touch.as_str()));
            db.smart_query(&sql, vars)
                .await
                .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
            withdrawn += 1;
        }
        Ok(withdrawn)
    }

    /// Renders existing durable claims (git-eligible kinds, visibility per
    /// personal-claims policy) into `.ibis/claims/` and creates ONE commit
    /// (the addendum). Claims already present in the tree are skipped, so a
    /// second run produces zero new commits; with zero new files nothing is
    /// committed at all. Dry run only reports the planned files
    ///
    pub async fn bootstrap_claim_store(
        &self,
        actor_ctx: &crate::ActorContext,
        repo_path: &str,
        project_name: &str,
        dry_run: bool,
    ) -> Result<ClaimBootstrapResult> {
        let _ = actor_ctx;
        let mut result = ClaimBootstrapResult {
            dry_run,
            ..Default::default()
        };
        let db = self.require_db()?;
        if repo_path.is_empty()
            || project_name.is_empty()
            || self.git_anchors.is_none()
            || self.claim_writer.is_none()
        {
            return Ok(result);
        }
        let anchors = self.git_anchors.as_ref().unwrap();
        let head = anchors
            .rev_parse_head(repo_path)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let tree_paths = anchors
            .list_tree(repo_path, &head)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let mut present: HashMap<String, bool> = HashMap::new();
        for path in &tree_paths {
            if path.starts_with(&format!("{}/", ibis_core::claims::CLAIMS_DIR)) {
                present.insert(path.clone(), true);
            }
        }
        let repo_id = repo_record_id(project_name);
        let sql = format!(
            "SELECT id, kind, title, content, visibility, owner, status, store FROM {TABLE_MEMORY} WHERE project = {repo_id} AND status = 'current' AND kind IN ['{0}','{1}','{2}'];",
            ibis_core::claims::KIND_DECISION,
            ibis_core::claims::KIND_RULE,
            ibis_core::claims::KIND_FIX
        );
        let outcome = db
            .execute(&sql)
            .await
            .map_err(|e| SearchError::other(format!("claim store: {e}")))?;
        let rows_values = rows::normalize_rows(&outcome).unwrap_or_default();
        let mut writes: HashMap<String, Vec<u8>> = HashMap::new();
        let mut keys: Vec<String> = Vec::new();
        for row in rows::object_rows(&rows_values) {
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
            let path = match ibis_core::claims::path(&key) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if present.contains_key(&path) {
                continue;
            }
            let kind = rows::scalar_string(rows::field(row, "kind"))
                .trim()
                .to_string();
            let visibility = rows::scalar_string(rows::field(row, "visibility"))
                .trim()
                .to_string();
            if self.claim_store_decision(&kind, &visibility) != STORE_GIT {
                continue;
            }
            let mut title = rows::scalar_string(rows::field(row, "title"))
                .trim()
                .to_string();
            if title.is_empty() {
                title = first_line(&rows::scalar_string(rows::field(row, "content")), 80);
            }
            let mut claim = ibis_core::claims::Claim {
                schema: ibis_core::claims::SCHEMA_VERSION,
                id: key.clone(),
                kind,
                title,
                visibility: visibility.clone(),
                related_paths: Vec::new(),
                supersedes: String::new(),
                created: Some(chrono::Utc::now()),
                redacted: String::new(),
                body: String::new(),
            };
            if visibility == ibis_core::claims::VISIBILITY_PERSONAL {
                if self.personal_claims_policy == PERSONAL_CLAIMS_IN_REPO {
                    claim.body = rows::scalar_string(rows::field(row, "content"));
                } else {
                    claim.redacted = ibis_core::claims::REDACTED_STUB.to_string();
                    claim.body = String::new();
                }
            } else {
                claim.body = rows::scalar_string(rows::field(row, "content"));
            }
            let data = match ibis_core::claims::render(&claim) {
                Ok(d) => d,
                Err(_) => continue,
            };
            writes.insert(path.clone(), data);
            keys.push(key);
            result.paths.push(path);
        }
        result.count = writes.len();
        if result.count == 0 {
            return Ok(result);
        }
        if dry_run {
            return Ok(result);
        }
        let writer = self.claim_writer.as_ref().unwrap();
        let head = writer
            .commit_claim(
                repo_path,
                &format!(
                    "chore(claims): bootstrap git claim store ({} claims)",
                    result.count
                ),
                &writes,
                &[],
            )
            .await
            .map_err(|e| SearchError::other(format!("claim store commit: {e}")))?;
        result.committed = true;
        result.head = head.clone();
        // Post-claim-commit notes hook (the addendum): the bootstrap commit is
        // the I(m) anchor for every claim it just landed.
        let id_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.annotate_claim_commit_notes(repo_path, &head, &id_refs)
            .await;
        Ok(result)
    }
}

/// Reports one `BootstrapClaimStore` run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ClaimBootstrapResult {
    pub count: usize,
    pub paths: Vec<String>,
    pub committed: bool,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub head: String,
}
