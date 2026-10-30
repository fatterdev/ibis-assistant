//! Typed-memory creation: validation, governance for
//! supersession, embedding, provenance, git-bound anchoring, the git-tree
//! claim store commit (file lands BEFORE the Surreal row) and the
//! relationship transaction.
//!
//! F-002: every content-bearing field binds (`$content`, `$title`, …);
//! sanitized record identifiers (`repo:⟨…⟩`, `commit:⟨oid⟩`) and
//! IsSafeRecordID-vetted source refs stay inline.

use ibis_db::value::Value;

use super::{
    bind, first_non_empty, repo_record_id, require_memory_record_id, validate_memory_confidence,
    Bindings, Memory, ES_DERIVED_FROM, ES_HAS_MEMORY, ES_HAS_REASONING, ES_SUPERSEDES, STORE_GIT,
    STORE_SURREAL, TABLE_MEMORY,
};
use crate::error::{Result, SearchError};
use crate::search::SearchService;

/// Describes a typed memory and its graph relationships (
/// `AddMemoryParams`).
#[derive(Debug, Clone, Default)]
pub struct AddMemoryParams {
    pub project_name: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub status: String,
    pub confidence: Option<f64>,
    pub visibility: String,
    pub owner: String,
    pub actor: String,
    pub tags: Vec<String>,
    pub branch: String,
    pub valid_from: String,
    pub valid_until: String,

    pub provenance_agent: String,
    pub provenance_session: String,
    pub provenance_tool: String,
    pub provenance_imported_from: String,
    pub provenance_source_hash: String,
    pub provenance_source_path: String,

    // Git-bound claim anchoring (the world-state spec).
    /// Resolved working repo path; empty = skip file-based binding.
    pub workspace_root: String,
    /// `require_clean` | `worktree` | `promote_on_commit` (default worktree).
    pub anchor_mode: String,
    /// Explicit anchor; wins over derived `commit:<oid>` refs.
    pub introduced_in_oid: String,

    // Set by resolve_memory_git_anchor; emitted by append_git_bound_fields.
    pub provenance_introduced_in_oid: String,
    pub provenance_dirty: bool,
    pub provenance_worktree_hash: String,
    pub provenance_promote_on_commit: bool,

    pub derived_from: Vec<String>,
    pub supersedes: String,
    /// Precomputed embedding; empty = embed `content` via the AI provider.
    pub embedding: Vec<f64>,
}

/// Generates a random, deterministically-safe memory record ID client-side
/// so AddMemory can report the created ID without a
/// second round-trip to parse multi-statement transaction results.
fn new_memory_record_id() -> Result<String> {
    let random = uuid::Uuid::new_v4();
    let hex: String = random
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(ibis_db::ids::format_record_id(
        TABLE_MEMORY,
        &format!("mem_{hex}"),
    ))
}

impl SearchService {
    /// Creates a typed memory and links it to its repository and sources
    /// Returns the created memory's record ID
    /// (`memory:⟨mem_…⟩`).
    pub async fn add_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        params: &mut AddMemoryParams,
    ) -> Result<String> {
        let db = self.require_db()?;
        if params.content.trim().is_empty() {
            return Err(SearchError::other("memory content is required"));
        }
        if params.project_name.trim().is_empty() {
            return Err(SearchError::other("project_name is required"));
        }
        let identity = first_non_empty(&[&params.actor, &params.owner]);
        let mut actor = actor_ctx.resolve_actor(&identity);
        let trusted_actor = actor_ctx.resolve_trusted_actor(&identity);
        let mut superseded_target: Option<Memory> = None;
        let supersedes = params.supersedes.trim().to_string();
        if !supersedes.is_empty() {
            require_memory_record_id(&supersedes).map_err(|_| {
                SearchError::other(format!("invalid superseded memory id: {supersedes}"))
            })?;
            let target = self.memory_for_governance(&supersedes).await?;
            if target.visibility == "personal" && target.owner != trusted_actor {
                return Err(SearchError::other(format!(
                    "personal memory {} may be superseded only by owner {:?}; share it first",
                    target.id, target.owner
                )));
            }
            superseded_target = Some(target);
        }

        let mut kind = params.kind.trim().to_string();
        if kind.is_empty() {
            kind = "fact".to_string();
        }
        let mut status = params.status.trim().to_string();
        if status.is_empty() {
            status = "current".to_string();
        }
        let mut visibility = params.visibility.trim().to_lowercase();
        match visibility.as_str() {
            "" => visibility = "team".to_string(),
            "team" | "personal" => {}
            other => {
                return Err(SearchError::other(format!(
                    "visibility must be team or personal, got {other:?}"
                )));
            }
        }
        if visibility == "personal" {
            if trusted_actor.is_empty() {
                return Err(SearchError::other(
                    "personal memory requires a trusted actor identity",
                ));
            }
            actor = trusted_actor.clone();
            // `actor` rebinding is a copy below; params.owner keeps the
            // original for provenance parity.
        }
        let mut store = self.claim_store_decision(&kind, &visibility);
        if let Some(target) = &superseded_target {
            if target.store == STORE_GIT && store != STORE_GIT {
                return Err(SearchError::other(format!(
                    "git-owned claim {} can only be superseded by another git-owned claim (decision/rule/fix, team visibility); the old claim file would otherwise resurrect at ingest",
                    target.id
                )));
            }
        }
        let confidence = match params.confidence {
            Some(value) => {
                validate_memory_confidence(value)?;
                value
            }
            None => 0.7,
        };

        let embedding = self
            .memory_embedding(&params.content, Some(&params.embedding))
            .await?;

        let id = new_memory_record_id()?;
        let repo_id = repo_record_id(&params.project_name);
        let valid_from_sql = if params.valid_from.trim().is_empty() {
            "time::now()".to_string()
        } else {
            "type::datetime($valid_from)".to_string()
        };

        let mut bindings: Bindings = vec![
            (
                "repo_name".into(),
                Value::from_t(params.project_name.as_str()),
            ),
            ("kind".into(), Value::from_t(kind.as_str())),
            ("title".into(), Value::from_t(params.title.as_str())),
            ("content".into(), Value::from_t(params.content.as_str())),
            ("status".into(), Value::from_t(status.as_str())),
            ("confidence".into(), Value::from_t(confidence)),
            ("visibility".into(), Value::from_t(visibility.as_str())),
            ("owner".into(), Value::from_t(actor.as_str())),
            (
                "tags".into(),
                Value::from_t(
                    params
                        .tags
                        .iter()
                        .map(|t| Value::from_t(t.as_str()))
                        .collect::<Vec<Value>>(),
                ),
            ),
            ("branch".into(), Value::from_t(params.branch.as_str())),
            (
                "embedding".into(),
                crate::commit_meaning::f64_vec_value(&embedding),
            ),
        ];
        if !params.valid_from.trim().is_empty() {
            bindings.push(("valid_from".into(), Value::from_t(params.valid_from.trim())));
        }
        let mut fields: Vec<String> = vec![
            "kind = $kind".to_string(),
            "title = $title".to_string(),
            "content = $content".to_string(),
            "status = $status".to_string(),
            "confidence = $confidence".to_string(),
            "visibility = $visibility".to_string(),
            "owner = $owner".to_string(),
            "tags = $tags".to_string(),
            "branch = $branch".to_string(),
            format!("project = {repo_id}"),
            format!("valid_from = {valid_from_sql}"),
            "embedding = $embedding".to_string(),
            "created_at = time::now()".to_string(),
            "updated_at = time::now()".to_string(),
        ];
        if !params.valid_until.trim().is_empty() {
            bindings.push((
                "valid_until".into(),
                Value::from_t(params.valid_until.trim()),
            ));
            fields.push("valid_until = type::datetime($valid_until)".to_string());
        }
        self.resolve_memory_provenance_from_sources(params).await?;
        self.resolve_memory_git_anchor(params).await?;
        append_provenance_fields(&mut fields, &mut bindings, params);
        append_git_bound_fields(&mut fields, &mut bindings, params);

        // Git-tree claim store (the addendum): commit the claim file BEFORE
        // the Surreal row so the repo stays SoT — a DB failure leaves the
        // file to be reconciled by the next IndexClaimStore pass, never the
        // reverse. The claims commit overrides anchoring: I(m) comes
        // from file history.
        if store == STORE_GIT {
            let (claim_oid, policy_applied) = self
                .commit_claim_for_memory(params, &id, &visibility, superseded_target.as_ref())
                .await?;
            if !claim_oid.is_empty() {
                fields.push(format!("introduced_in_oid = '{claim_oid}'"));
                fields.push(format!(
                    "provenance.introduced_in_commit = {}",
                    ibis_db::ids::format_record_id("commit", &claim_oid)
                ));
                if !policy_applied.is_empty() {
                    bindings.push((
                        "policy_applied".into(),
                        Value::from_t(policy_applied.as_str()),
                    ));
                    fields.push("policy_applied = $policy_applied".to_string());
                }
            } else {
                store = STORE_SURREAL.to_string();
            }
        }
        fields.push(format!("store = '{store}'"));

        let mut query = String::from("BEGIN TRANSACTION;\n");
        // D17 R4: RELATE does not implicit-create the repo endpoint — persist
        // it first (legacy 2.x created it through the RELATE side effect).
        query.push_str(&format!("UPSERT {repo_id} SET name = $repo_name;\n"));
        query.push_str(&format!(
            "LET $mem = CREATE {id} SET {};\n",
            fields.join(", ")
        ));
        query.push_str(&format!("RELATE {repo_id}->{ES_HAS_MEMORY}->$mem;\n"));
        for source in &params.derived_from {
            let source = source.trim();
            if ibis_db::ids::is_safe_record_id(source) {
                query.push_str(&format!("RELATE $mem->{ES_DERIVED_FROM}->{source};\n"));
            }
        }
        let supersedes = params.supersedes.trim().to_string();
        if ibis_db::ids::is_safe_record_id(&supersedes) {
            query.push_str(&format!("RELATE $mem->{ES_SUPERSEDES}->{supersedes};\n"));
            let old_set = self.supersede_old_set(&params.workspace_root).await;
            query.push_str(&format!("UPDATE {supersedes} SET {old_set};\n"));
        }
        query.push_str("COMMIT TRANSACTION;");

        db.smart_query(&query, bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("add memory: {e}")))?;
        Ok(id)
    }

    /// `memoryEmbedding: precomputed embedding wins; otherwise embed the
    /// content (F-002: embeddings never interpolate, they bind).
    pub(crate) async fn memory_embedding(
        &self,
        content: &str,
        precomputed: Option<&Vec<f64>>,
    ) -> Result<Vec<f64>> {
        if let Some(pre) = precomputed {
            if !pre.is_empty() {
                return Ok(pre.clone());
            }
        }
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;
        let vec = ai
            .embed_text(content)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;
        Ok(crate::search::float32_to_f64(&vec))
    }

    /// `resolveMemoryProvenanceFromSources: fills source_hash/source_path
    /// from derived_from source_file refs when the caller omitted them.
    async fn resolve_memory_provenance_from_sources(
        &self,
        params: &mut AddMemoryParams,
    ) -> Result<()> {
        if self.db.is_none() {
            return Ok(());
        }
        let mut need_hash = params.provenance_source_hash.trim().is_empty();
        let mut need_path = params.provenance_source_path.trim().is_empty();
        if !need_hash && !need_path {
            return Ok(());
        }
        let db = self.db.as_ref().unwrap();
        for source in &params.derived_from {
            let source = source.trim();
            if !source.starts_with("source_file:") || !ibis_db::ids::is_safe_record_id(source) {
                continue;
            }
            let sql = format!("SELECT hash, path, rel_path FROM {source};");
            let outcome = db.execute(&sql).await.map_err(|e| {
                SearchError::other(format!("resolve provenance from {source}: {e}"))
            })?;
            let Some(rows_values) = crate::rows::normalize_rows(&outcome) else {
                continue;
            };
            let objects = crate::rows::object_rows(&rows_values);
            let Some(row) = objects.first() else {
                continue;
            };
            if need_hash {
                let hash = crate::rows::scalar_string(crate::rows::field(row, "hash"))
                    .trim()
                    .to_string();
                if !hash.is_empty() {
                    params.provenance_source_hash = hash;
                    need_hash = false;
                }
            }
            if need_path {
                let mut path = crate::rows::scalar_string(crate::rows::field(row, "path"))
                    .trim()
                    .to_string();
                if path.is_empty() {
                    path = crate::rows::scalar_string(crate::rows::field(row, "rel_path"))
                        .trim()
                        .to_string();
                }
                if !path.is_empty() {
                    params.provenance_source_path = path;
                    need_path = false;
                }
            }
            if !need_hash && !need_path {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Writes the reasoning outcome memory + legacy reinforcement transaction
    /// (`SaveReasoningOutcome, from ). The typed memory is
    /// written before the legacy semantic/reinforcement transaction so a
    /// failed AddMemory leaves no partial reinforce state.
    pub async fn save_reasoning_outcome(
        &self,
        actor_ctx: &crate::ActorContext,
        repo_name: &str,
        question: &str,
        outcome_text: &str,
        useful_sources: &[String],
    ) -> Result<()> {
        if self.db.is_none() {
            return Err(SearchError::DatabaseNotConnected);
        }
        let full_text = format!("Question: {question}\nOutcome: {outcome_text}");
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;
        let vec = ai
            .embed_text(&full_text)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;

        let mut safe_sources: Vec<String> = Vec::with_capacity(useful_sources.len());
        for source_id in useful_sources {
            let source_id = source_id.trim();
            if !ibis_db::ids::is_safe_record_id(source_id) {
                continue;
            }
            safe_sources.push(source_id.to_string());
        }

        // 1. Typed memory first (reuses the embedding; no second AI round-trip).
        let confidence = 0.6;
        self.add_memory(
            actor_ctx,
            &mut AddMemoryParams {
                project_name: repo_name.to_string(),
                kind: "reasoning".to_string(),
                content: full_text,
                confidence: Some(confidence),
                visibility: "team".to_string(),
                derived_from: safe_sources.clone(),
                provenance_tool: "save_reasoning_outcome".to_string(),
                embedding: crate::search::float32_to_f64(&vec),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| SearchError::wrap("link reasoning memory", e))?;

        // 2. Legacy semantic + structural reinforcement.
        let repo_id = repo_record_id(repo_name);
        self.run_reasoning_transaction(
            &repo_id,
            repo_name,
            question,
            outcome_text,
            &vec,
            &safe_sources,
        )
        .await
    }

    /// Legacy reasoning + reinforcement transaction (question/outcome/vector
    /// bind as `$question`/`$outcome`/`$vec` — F-002).
    async fn run_reasoning_transaction(
        &self,
        repo_id: &str,
        repo_name: &str,
        question: &str,
        outcome_text: &str,
        vec: &[f32],
        safe_sources: &[String],
    ) -> Result<()> {
        let db = self.require_db()?;
        let mut query = String::from("BEGIN TRANSACTION;\n");
        // D17 R4: persist the repo endpoint before the has_reasoning RELATE.
        query.push_str(&format!("UPSERT {repo_id} SET name = $repo_name;\n"));
        query.push_str(
            "LET $reas = CREATE reasoning SET question = $question, outcome = $outcome, embedding = $vec;\n",
        );
        query.push_str(&format!("RELATE {repo_id}->{ES_HAS_REASONING}->$reas;\n"));
        const DELTA: &str = "0.2";
        for source_id in safe_sources {
            query.push_str(&format!(
                "UPDATE changed SET usage_weight = (usage_weight OR 1.0) + {DELTA} WHERE out = {source_id};\n"
            ));
            query.push_str(&format!(
                "UPDATE implements SET usage_weight = (usage_weight OR 1.0) + {DELTA} WHERE out = {source_id};\n"
            ));
            query.push_str(&format!(
                "UPDATE {source_id} SET access_count = (access_count OR 0) + 1, last_accessed = time::now();\n"
            ));
        }
        query.push_str("COMMIT TRANSACTION;\n");

        let mut bindings = ibis_db::executor::Vars::new();
        bindings.insert("repo_name".into(), Value::from_t(repo_name));
        bindings.insert("question".into(), Value::from_t(question));
        bindings.insert("outcome".into(), Value::from_t(outcome_text));
        bindings.insert("vec".into(), crate::commit_meaning::vec_value(vec));
        db.smart_query(&query, bindings)
            .await
            .map_err(|e| SearchError::other(e.to_string()))?;
        Ok(())
    }

    /// `AddCollaborativeMemory: typed back-compat wrapper.
    pub async fn add_collaborative_memory(
        &self,
        actor_ctx: &crate::ActorContext,
        repo_name: &str,
        memory_text: &str,
        precomputed_embedding: Vec<f64>,
    ) -> Result<()> {
        self.add_memory(
            actor_ctx,
            &mut AddMemoryParams {
                project_name: repo_name.to_string(),
                kind: "fact".to_string(),
                content: memory_text.to_string(),
                visibility: "team".to_string(),
                embedding: precomputed_embedding,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
    }
}

/// `appendProvenanceFields.
fn append_provenance_fields(
    fields: &mut Vec<String>,
    bindings: &mut Bindings,
    params: &AddMemoryParams,
) {
    let items = [
        ("agent", params.provenance_agent.as_str()),
        ("session", params.provenance_session.as_str()),
        ("tool", params.provenance_tool.as_str()),
        ("imported_from", params.provenance_imported_from.as_str()),
        ("source_hash", params.provenance_source_hash.as_str()),
        ("source_path", params.provenance_source_path.as_str()),
    ];
    for (idx, (name, value)) in items.iter().enumerate() {
        if value.trim().is_empty() {
            continue;
        }
        let var = format!("prov_{idx}");
        bindings.push((var.clone(), Value::from_t(*value)));
        fields.push(format!("provenance.{name} = ${var}"));
    }
}

/// `appendGitBoundFields: emits the git-bound claim fields
/// (the world-state spec). Oids are git/DB-verified full hashes at
/// this point, so the `commit:⟨oid⟩` record literal stays inline
/// (identifier rule); the oid itself also binds for the scalar column.
fn append_git_bound_fields(
    fields: &mut Vec<String>,
    bindings: &mut Bindings,
    params: &AddMemoryParams,
) {
    let oid = params.provenance_introduced_in_oid.trim();
    if !oid.is_empty() {
        bindings.push(("introduced_oid".into(), Value::from_t(oid)));
        fields.push("introduced_in_oid = $introduced_oid".to_string());
        fields.push(format!(
            "provenance.introduced_in_commit = {}",
            ibis_db::ids::format_record_id("commit", oid)
        ));
    }
    if params.provenance_dirty {
        fields.push("provenance.dirty = true".to_string());
        let h = params.provenance_worktree_hash.trim();
        if !h.is_empty() {
            bindings.push(("worktree_hash".into(), Value::from_t(h)));
            fields.push("provenance.worktree_hash = $worktree_hash".to_string());
        }
        if params.provenance_promote_on_commit {
            fields.push("provenance.promote_on_commit = true".to_string());
        }
    }
}
