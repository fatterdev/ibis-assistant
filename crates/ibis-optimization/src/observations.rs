//! C8 step 5 — derived-observation synthesis (`observations_pass`).
//!
//! Runs at the tail of [`crate::raft::Optimizer::consolidate_loop`] when
//! `ai.observations.enabled` is on: reconciles the deterministic components
//! (`ReconcileMode::Full`), then upgrades the derived backlog (stale /
//! `generated_by = 'fallback'` / embedding-less rows) with one bounded LLM
//! summary each (`local:M`, personal `local_only:M`), a deterministic
//! fallback when the provider is absent, in error, or produces an invalid
//! summary, and the content embedding.
//!
//! Cost model: ≤ `max_llm_summaries_per_pass` LLM calls per pass on
//! `system:memory_observations`; zero calls when the evidence hash is
//! unchanged (the row upgrades once to `generated_by = 'llm'` and leaves the
//! backlog). The model never controls membership: it only writes name +
//! summary; the footer is ours and validation rejects foreign record ids.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ibis_ai::decide::{decide_with_hint, DecideQuestion};
use ibis_ai::types::{GenerationConfig, Part};
use ibis_db::executor::{Executor, Vars};
use ibis_db::ids::is_safe_record_id;
use ibis_db::value::Value;
use ibis_search::memory::observations::{
    fallback_summary as deterministic_fallback_summary, render_observation_content,
    validate_observation_summary, EvidenceMember, ObsScope, ReconcileMode, Signature,
    OBSERVATION_FAMILIES,
};
use ibis_search::rows;
use ibis_search::search::SearchService;

use crate::raft::Optimizer;
use crate::store::DbCostStore;
use crate::tasks::PassCheckpoint;

/// D13 byte-stable instructions block for the observation synthesis call
/// site. The evidence list is appended after it; `Return only JSON` stays the
/// final line of the prompt.
pub const OBSERVATION_INSTRUCTIONS: &str = r#"Summarize one cluster of related software-project memories that share a structural signature.
Write a name of at most 8 words and a summary of at most 120 words describing the recurring pattern they reveal.
Use only facts stated in the memories. Do not invent files, commits, tickets, identifiers or numbers.
Do not list the memories: the evidence list is appended separately.
"#;

/// D10 per-task routing: observation synthesis is a mid-tier local task
/// (cloud fallback preserved by the `local:` base); personal clusters keep
/// the no-cloud guarantee through `local_only:`.
const OBSERVATION_ROUTE_HINT: &str = "local:M";
const OBSERVATION_PERSONAL_ROUTE_HINT: &str = "local_only:M";
/// Deterministic summaries: `0.0` is the "unset" sentinel of the provider
/// layer (D11), so 0.001 is the smallest greedy temperature that survives
/// serialization.
const OBSERVATION_TEMPERATURE: f64 = 0.001;
const OBSERVATION_MAX_OUTPUT_TOKENS: usize = 512;
/// Per-member evidence text bound (L3 §4.8).
const EVIDENCE_TEXT_RUNES: usize = 400;
/// Backlog read bound per scope: the reconcile already caps components at 25;
/// this bounds the leftover retry queue of one pass.
const SYNTHESIS_BACKLOG_LIMIT: usize = 100;
/// Cost bucket of the synthesis call site.
pub const OBSERVATION_SUMMARY_BUCKET: &str = "system:memory_observations";
/// D11 shadow rubric for the deterministic accept filters (log-only).
pub const OBSERVATION_ACCEPT_QUESTION_ID: &str = "observation_accept";
pub const OBSERVATION_ACCEPT_RUBRIC_VERSION: &str = "observation_accept.v1";
const DECIDE_SHADOW_ROUTE_HINT: &str = "local_only:S";

/// One member line fed to the prompt/footer (title fallback resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationEvidenceLine {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub text: String,
}

/// One derived row from the synthesis backlog.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationSynthCandidate {
    pub id: String,
    pub status: String,
    pub title: String,
    pub content: String,
    pub visibility: String,
    pub owner: String,
    pub anchor_signature: String,
    pub signatures: Vec<String>,
    pub members: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub generated_by: String,
    pub has_embedding: bool,
}

impl ObservationSynthCandidate {
    fn is_personal(&self) -> bool {
        self.visibility.trim().eq_ignore_ascii_case("personal")
    }
}

/// One pass over one project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationPassReport {
    pub scopes: usize,
    /// Create + refresh actions performed by the reconcile stages.
    pub reconciled: usize,
    pub summarized: usize,
    pub fallbacks: usize,
    pub embedding_retries: usize,
    /// Backlog rows left for the next pass (LLM cap reached).
    pub deferred: usize,
    /// Rows skipped after a hard failure (DB/embedding), retried next pass.
    pub failed: usize,
}

/// D11 shadow record for one synthesized candidate: what the bounded
/// `observation_accept` decision would have answered next to the
/// deterministic filters (which always accept by construction).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ObservationDecideShadow {
    pub observation: String,
    pub rubric: String,
    pub best: String,
    pub probs: std::collections::BTreeMap<String, f64>,
    pub confidence: f64,
    /// `best == "accept"` — the deterministic filters accepted the component.
    pub agreement: bool,
}

/// The closed `observation_accept` question (rubric version pinned in the
/// text, like `is_noise.v1`).
pub fn observation_accept_question() -> DecideQuestion {
    DecideQuestion::choice(
        OBSERVATION_ACCEPT_QUESTION_ID,
        &format!(
            "Rubric {OBSERVATION_ACCEPT_RUBRIC_VERSION}. Several project memories were linked into one cluster by deterministic structural signatures. Is this a genuine recurring pattern worth summarizing, or an artifact of signature chaining?"
        ),
        &[
            (
                "accept",
                "the shared signature reveals a real recurring pattern across the memories",
            ),
            (
                "reject",
                "the link is coincidental chaining (generic key, hub path) with no shared meaning",
            ),
        ],
    )
}

// ---------------------------------------------------------------------------
// Pure helpers (prompt, parse, fallback)
// ---------------------------------------------------------------------------

/// Raw summary shape (name + summary), after parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationSummary {
    pub name: String,
    pub summary: String,
}

/// Assembles the D13 prompt: byte-stable instructions → `SIGNATURE:` (anchor +
/// signatures) → `EVIDENCE:` (members in id order, each text capped) → the
/// return schema last.
pub fn build_observation_prompt(
    anchor_signature: &str,
    signatures: &[String],
    evidence: &[ObservationEvidenceLine],
) -> String {
    let mut prompt = String::from(OBSERVATION_INSTRUCTIONS);
    prompt.push_str("\nSIGNATURE: ");
    prompt.push_str(anchor_signature.trim());
    prompt.push('\n');
    if !signatures.is_empty() {
        prompt.push_str("SIGNATURES: ");
        prompt.push_str(&signatures.join(", "));
        prompt.push('\n');
    }
    prompt.push_str("EVIDENCE:\n");
    for line in evidence {
        let text = collapsed(&line.text);
        if line.label.trim().is_empty() {
            prompt.push_str(&format!(
                "- {} [{}]: {}\n",
                empty_or(&line.kind, "memory"),
                line.id,
                cap_runes(&text, EVIDENCE_TEXT_RUNES)
            ));
        } else {
            prompt.push_str(&format!(
                "- {} [{}]: {} — {}\n",
                empty_or(&line.kind, "memory"),
                line.id,
                line.label.trim(),
                cap_runes(&text, EVIDENCE_TEXT_RUNES)
            ));
        }
    }
    prompt.push_str("\nReturn only JSON:\n{\"name\":\"...\",\"summary\":\"...\"}");
    prompt
}

/// Raw wire shape of the model response.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ObservationSummaryJson {
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
}

/// Parses the model's `{"name": ..., "summary": ...}` (Markdown fences
/// stripped, `parse_consolidation_decision` pattern). Semantic validation
/// (non-empty, caps, foreign record ids) stays in
/// [`validate_observation_summary`].
pub fn parse_observation_summary(text: &str) -> Result<ObservationSummary, String> {
    let text = strip_fences(text);
    let parsed: ObservationSummaryJson =
        serde_json::from_str(&text).map_err(|e| format!("parse observation summary JSON: {e}"))?;
    Ok(ObservationSummary {
        name: parsed.name,
        summary: parsed.summary,
    })
}

/// Parses + validates one model response into a usable summary.
pub fn parse_and_validate_summary(
    text: &str,
    candidate: &ObservationSynthCandidate,
) -> Result<ObservationSummary, String> {
    let parsed = parse_observation_summary(text)?;
    let validated = validate_observation_summary(
        &parsed.name,
        &parsed.summary,
        &candidate.members,
        &candidate.evidence_refs,
    )
    .ok_or_else(|| "observation summary refused by validation".to_string())?;
    Ok(ObservationSummary {
        name: validated.name,
        summary: validated.summary,
    })
}

fn strip_fences(text: &str) -> String {
    let mut text = text.trim().to_string();
    if let Some(stripped) = text.strip_prefix("```json") {
        text = stripped.to_string();
    }
    if let Some(stripped) = text.strip_prefix("```") {
        text = stripped.to_string();
    }
    if let Some(stripped) = text.strip_suffix("```") {
        text = stripped.to_string();
    }
    text.trim().to_string()
}

/// Deterministic fallback summary (AI absent, provider error, tier-M missing
/// on `local_only`, validation failed): `<family_label>: <anchor_key>` +
/// `<n> memories share <signature>.` + member labels.
pub fn fallback_summary(
    candidate: &ObservationSynthCandidate,
    evidence: &[ObservationEvidenceLine],
) -> ObservationSummary {
    let members = evidence_members(evidence);
    let anchor = parse_anchor(&candidate.anchor_signature);
    match anchor {
        Some(anchor) => {
            let fallback = deterministic_fallback_summary(&anchor, &members);
            ObservationSummary {
                name: fallback.name,
                summary: fallback.summary,
            }
        }
        None => {
            let labels: Vec<String> = evidence.iter().map(label_of).collect();
            let title = candidate.title.trim();
            let name = if title.is_empty() {
                cap_runes(&format!("Observation: {}", candidate.id), 80)
            } else {
                cap_runes(title, 80)
            };
            let mut summary = format!(
                "{} memories share a structural signature.",
                candidate.members.len()
            );
            let first: Vec<&str> = labels
                .iter()
                .map(String::as_str)
                .filter(|label| !label.is_empty())
                .take(5)
                .collect();
            if !first.is_empty() {
                summary.push(' ');
                summary.push_str(&first.join(" · "));
            }
            ObservationSummary {
                name,
                summary: cap_runes(&summary, 1200),
            }
        }
    }
}

/// Converts prompt evidence lines into the reviewed `EvidenceMember` shape
/// used by the shared fallback/footer helpers.
fn evidence_members(evidence: &[ObservationEvidenceLine]) -> Vec<EvidenceMember> {
    evidence
        .iter()
        .map(|line| EvidenceMember {
            id: line.id.clone(),
            kind: line.kind.clone(),
            status: "current".to_string(),
            content: line.text.clone(),
            title: line.label.clone(),
            details: serde_json::Value::Null,
        })
        .collect()
}

fn label_of(line: &ObservationEvidenceLine) -> String {
    let label = line.label.trim();
    if !label.is_empty() {
        label.to_string()
    } else {
        first_line(&line.text, 80)
    }
}

fn parse_anchor(anchor_signature: &str) -> Option<Signature> {
    let (family, key) = anchor_signature.split_once('|')?;
    let family = family.trim();
    if !OBSERVATION_FAMILIES.contains(&family) {
        return None;
    }
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some(Signature::new(family, key))
}

fn collapsed(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_runes(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn first_line(value: &str, max: usize) -> String {
    value
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(max)
        .collect()
}

fn empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value.trim()
    }
}

/// Canonical writer form of a memory record id (`memory:⟨key⟩`), mirroring
/// the search crate's identity comparisons.
fn canonical_memory_id(id: &str) -> String {
    let raw = id.trim();
    let key = raw.strip_prefix("memory:").unwrap_or(raw);
    let key = key
        .trim_start_matches('?')
        .trim_start_matches('⟨')
        .trim_start_matches('`');
    let key = key
        .trim_end_matches('?')
        .trim_end_matches('⟩')
        .trim_end_matches('`');
    if key.is_empty() {
        return String::new();
    }
    ibis_db::ids::format_record_id("memory", key)
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

impl Optimizer {
    /// C8 step 5: reconcile + synthesis of the derived backlog for one
    /// project. No-op unless `ai.observations.enabled`; bounded by
    /// `max_llm_summaries_per_pass`.
    pub async fn observations_pass(
        &self,
        project_name: &str,
        checkpoint: Option<&dyn PassCheckpoint>,
    ) -> Result<ObservationPassReport, String> {
        let mut report = ObservationPassReport::default();
        if !self.observations.enabled {
            return Ok(report);
        }
        let db = self.db.as_ref().ok_or("database not connected")?;
        let project = project_name.trim();
        if project.is_empty() {
            return Err("observation synthesis requires a project name".to_string());
        }
        let service: Arc<SearchService> = match &self.search {
            Some(service) => service.clone(),
            None => Arc::new(SearchService::new(self.db.clone(), self.ai.clone())),
        };
        let scopes = ibis_search::memory::observations::observation_scopes_for_project(
            db.as_ref(),
            project,
            self.observations.max_scopes_per_project,
        )
        .await
        .map_err(|e| e.to_string())?;
        report.scopes = scopes.len();
        let mut summarized = 0usize;
        // The task checkpoint owns the residual budget; without a checkpoint
        // the per-pass config cap applies (existing behavior).
        let summary_cap = match checkpoint.and_then(|cp| cp.remaining_summaries()) {
            Some(remaining) => remaining,
            None => self.observations.max_llm_summaries_per_pass.max(1),
        };
        let mut shadow_skipped = false;
        for scope in &scopes {
            if checkpoint.is_some_and(|cp| !cp.should_continue()) {
                break;
            }
            let reconcile = service
                .reconcile_observations_with_settings(
                    project,
                    scope,
                    ReconcileMode::Full,
                    &self.observations,
                )
                .await
                .map_err(|e| format!("observations reconcile: {e}"))?;
            report.reconciled += reconcile.create + reconcile.refresh;
            let backlog = self
                .observation_synthesis_backlog(db, project, scope)
                .await?;
            for candidate in backlog {
                if checkpoint.is_some_and(|cp| !cp.should_continue()) {
                    break;
                }
                let needs_llm = candidate.generated_by.trim() != "llm";
                if needs_llm && summarized >= summary_cap {
                    report.deferred += 1;
                    continue;
                }
                match self
                    .synthesize_candidate(db, &candidate, &mut shadow_skipped)
                    .await
                {
                    Ok(SynthesisOutcome::Summarized) => {
                        summarized += 1;
                        report.summarized += 1;
                    }
                    Ok(SynthesisOutcome::Fallback) => report.fallbacks += 1,
                    Ok(SynthesisOutcome::EmbeddingRetry) => report.embedding_retries += 1,
                    Err(err) => {
                        report.failed += 1;
                        tracing::warn!("ObservationsPass: {}: {err}", candidate.id);
                    }
                }
            }
            if let Some(cp) = checkpoint {
                cp.scope_done(&report);
            }
        }
        Ok(report)
    }

    /// Derived rows in the synthesis backlog of one scope: stale, fallback or
    /// embedding-less. Ordered by id for determinism.
    async fn observation_synthesis_backlog(
        &self,
        db: &Arc<dyn Executor>,
        project_name: &str,
        scope: &ObsScope,
    ) -> Result<Vec<ObservationSynthCandidate>, String> {
        let repo_id =
            ibis_db::ids::format_record_id("repo", &ibis_db::ids::sanitize_id(project_name));
        let mut vars = Vars::new();
        let scope_sql = match scope {
            ObsScope::Team => "visibility = 'team'".to_string(),
            ObsScope::Personal(owner) => {
                vars.insert("scope_owner".into(), Value::from_t(owner.trim()));
                "visibility = 'personal' AND owner = $scope_owner".to_string()
            }
        };
        let query = format!(
            "SELECT id, status, title, content, visibility, owner, \
             details.members AS members, details.evidence_refs AS evidence_refs, \
             details.anchor_signature AS anchor_signature, \
             details.signatures AS signatures, details.generated_by AS generated_by, \
             embedding != NONE AS has_embedding \
             FROM memory WHERE project = {repo_id} AND kind = 'observation' \
             AND store = 'derived' AND {scope_sql} \
             AND status IN ['current', 'stale'] \
             AND (status = 'stale' OR details.generated_by = 'fallback' OR embedding = NONE) \
             ORDER BY id LIMIT {SYNTHESIS_BACKLOG_LIMIT};"
        );
        let outcome = db
            .smart_query(&query, vars)
            .await
            .map_err(|e| format!("select observation backlog: {e}"))?;
        let rows = rows::normalize_rows(&outcome)
            .ok_or_else(|| "observation backlog: unexpected result shape".to_string())?;
        let mut backlog = Vec::new();
        for row in rows::object_rows(&rows) {
            let id = canonical_memory_id(&rows::record_id(rows::field(row, "id")));
            if !is_safe_record_id(&id) {
                continue;
            }
            backlog.push(ObservationSynthCandidate {
                id,
                status: rows::scalar_string(rows::field(row, "status")),
                title: rows::scalar_string(rows::field(row, "title")),
                content: rows::scalar_string(rows::field(row, "content")),
                visibility: rows::scalar_string(rows::field(row, "visibility")),
                owner: rows::scalar_string(rows::field(row, "owner")),
                anchor_signature: rows::scalar_string(rows::field(row, "anchor_signature")),
                signatures: rows::string_slice(rows::field(row, "signatures")),
                members: rows::string_slice(rows::field(row, "members")),
                evidence_refs: rows::string_slice(rows::field(row, "evidence_refs")),
                generated_by: rows::scalar_string(rows::field(row, "generated_by")),
                has_embedding: matches!(rows::field(row, "has_embedding"), Value::Bool(true)),
            });
        }
        Ok(backlog)
    }

    /// Loads the member evidence of one candidate, in member order.
    async fn load_observation_evidence(
        &self,
        db: &Arc<dyn Executor>,
        members: &[String],
    ) -> Result<Vec<ObservationEvidenceLine>, String> {
        let wanted: Vec<String> = {
            let mut seen = HashSet::new();
            members
                .iter()
                .map(|member| canonical_memory_id(member))
                .filter(|member| is_safe_record_id(member) && seen.insert(member.clone()))
                .collect()
        };
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let query = format!(
            "SELECT id, kind, title, content FROM memory WHERE id IN [{}];",
            wanted.join(", ")
        );
        let outcome = db
            .smart_query(&query, Vars::new())
            .await
            .map_err(|e| format!("select observation evidence: {e}"))?;
        let rows = rows::normalize_rows(&outcome)
            .ok_or_else(|| "observation evidence: unexpected result shape".to_string())?;
        let mut by_id: HashMap<String, (String, String, String)> = HashMap::new();
        for row in rows::object_rows(&rows) {
            let id = canonical_memory_id(&rows::record_id(rows::field(row, "id")));
            if !is_safe_record_id(&id) {
                continue;
            }
            by_id.insert(
                id,
                (
                    rows::scalar_string(rows::field(row, "kind")),
                    rows::scalar_string(rows::field(row, "title")),
                    rows::scalar_string(rows::field(row, "content")),
                ),
            );
        }
        let mut evidence = Vec::new();
        for member in members {
            let key = canonical_memory_id(member);
            let Some((kind, title, content)) = by_id.get(&key) else {
                continue;
            };
            let label = if title.trim().is_empty() {
                first_line(content, 80)
            } else {
                title.trim().to_string()
            };
            evidence.push(ObservationEvidenceLine {
                id: key,
                kind: kind.clone(),
                label,
                text: content.clone(),
            });
        }
        Ok(evidence)
    }

    /// One backlog row → (LLM summary | deterministic fallback) → embedding →
    /// single-row UPDATE.
    async fn synthesize_candidate(
        &self,
        db: &Arc<dyn Executor>,
        candidate: &ObservationSynthCandidate,
        shadow_skipped: &mut bool,
    ) -> Result<SynthesisOutcome, String> {
        let evidence = self
            .load_observation_evidence(db, &candidate.members)
            .await?;

        // §4.7 row 6: an `llm` row missing only the embedding is a retry —
        // zero LLM, one embedding.
        if candidate.generated_by.trim() == "llm" && !candidate.has_embedding {
            let embedding = self
                .embed_observation_content(&candidate.content)
                .await?
                .ok_or_else(|| "embedding retry failed".to_string())?;
            apply_embedding_only(db, &candidate.id, &embedding).await?;
            return Ok(SynthesisOutcome::EmbeddingRetry);
        }

        // D11 shadow: log-only measurement of the deterministic filters.
        if self.observations.decide_shadow && !*shadow_skipped {
            match self.shadow_decide_observation(candidate, &evidence).await {
                Ok(shadow) => tracing::info!(
                    target: "observation_decide_shadow",
                    observation = %shadow.observation,
                    rubric = %shadow.rubric,
                    best = %shadow.best,
                    probs = ?shadow.probs,
                    confidence = shadow.confidence,
                    agreement = shadow.agreement,
                    "observation_decide_shadow"
                ),
                Err(err) => {
                    *shadow_skipped = true;
                    tracing::warn!("ObservationsPass: decide shadow disabled for this pass: {err}");
                }
            }
        }

        let (summary, generated_by) =
            match self.distill_observation_summary(candidate, &evidence).await {
                Some(summary) => (summary, "llm"),
                None => {
                    tracing::info!(
                        "ObservationsPass: deterministic fallback for {} (no usable summary)",
                        candidate.id
                    );
                    (fallback_summary(candidate, &evidence), "fallback")
                }
            };
        let members = evidence_members(&evidence);
        let content = render_observation_content(&summary.summary, &members);
        // A failed embedding still persists the row (embedding = NONE); the
        // next pass retries it (§4.7 row 6 / §4.8 edge case).
        let embedding = self.embed_observation_content(&content).await?;
        apply_observation_synthesis(
            db,
            &candidate.id,
            &summary.name,
            &content,
            generated_by,
            embedding.as_ref(),
        )
        .await?;
        Ok(if generated_by == "llm" {
            SynthesisOutcome::Summarized
        } else {
            SynthesisOutcome::Fallback
        })
    }

    /// The single LLM synthesis call (one retry only on invalid JSON /
    /// refused summary; provider errors fall back immediately). Each attempt
    /// is billed on `system:memory_observations`. `None` means "use the
    /// deterministic fallback": AI absent, provider error or refused after
    /// the retry.
    async fn distill_observation_summary(
        &self,
        candidate: &ObservationSynthCandidate,
        evidence: &[ObservationEvidenceLine],
    ) -> Option<ObservationSummary> {
        let ai = self.ai.as_ref()?;
        let route_hint = if candidate.is_personal() {
            OBSERVATION_PERSONAL_ROUTE_HINT
        } else {
            OBSERVATION_ROUTE_HINT
        };
        let prompt =
            build_observation_prompt(&candidate.anchor_signature, &candidate.signatures, evidence);
        let contents = [ibis_ai::Content {
            role: "user".to_string(),
            parts: vec![Part { text: prompt }],
        }];
        let mut last_refusal = String::new();
        for attempt in 0..2 {
            let response = match ai
                .generate_content(
                    &contents,
                    GenerationConfig {
                        route_hint: route_hint.to_string(),
                        temperature: OBSERVATION_TEMPERATURE,
                        max_output_tokens: OBSERVATION_MAX_OUTPUT_TOKENS,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    tracing::warn!(
                        "ObservationsPass: provider error for {} ({err}); using fallback",
                        candidate.id
                    );
                    return None;
                }
            };
            if let Some(usage) = &response.usage_metadata {
                if let Some(db) = &self.db {
                    let tracker = ibis_ai::CostTracker::new(Some(
                        Arc::new(DbCostStore(db.clone())) as Arc<dyn ibis_ai::CostStore>,
                    ));
                    tracker
                        .record_usage(
                            OBSERVATION_SUMMARY_BUCKET,
                            usage.prompt_token_count.max(0) as u64,
                            usage.candidates_token_count.max(0) as u64,
                            usage.cached_tokens(),
                        )
                        .await;
                }
            }
            let text: String = response
                .content
                .parts
                .iter()
                .map(|p| p.text.as_str())
                .collect();
            match parse_and_validate_summary(&text, candidate) {
                Ok(summary) => return Some(summary),
                Err(err) => {
                    last_refusal = err;
                    if attempt == 0 {
                        tracing::warn!(
                            "ObservationsPass: summary refused for {} ({last_refusal}); retrying once",
                            candidate.id
                        );
                    }
                }
            }
        }
        tracing::warn!(
            "ObservationsPass: summary refused for {} after retry ({last_refusal}); using fallback",
            candidate.id
        );
        None
    }

    /// Embedding through the provider (B6 L2 cache transparent). `Ok(None)`
    /// when the provider is absent, fails or returns an empty vector — the
    /// row persists with `embedding = NONE` and retries next pass.
    async fn embed_observation_content(&self, content: &str) -> Result<Option<Value>, String> {
        let Some(ai) = &self.ai else {
            return Ok(None);
        };
        match ai.embed_text(content).await {
            Ok(vector) if !vector.is_empty() => {
                Ok(Some(ibis_search::commit_meaning::vec_value(&vector)))
            }
            Ok(_) => Ok(None),
            Err(err) => {
                tracing::warn!("ObservationsPass: embedding failed: {err}");
                Ok(None)
            }
        }
    }

    /// D11 shadow measurement of the deterministic accept filters (log-only;
    /// calibration for `observation_accept.v1`).
    async fn shadow_decide_observation(
        &self,
        candidate: &ObservationSynthCandidate,
        evidence: &[ObservationEvidenceLine],
    ) -> Result<ObservationDecideShadow, String> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| "AI provider not configured".to_string())?;
        let mut state = format!("SIGNATURE: {}\nMEMBERS:\n", candidate.anchor_signature);
        for line in evidence {
            state.push_str(&format!(
                "- {}: {}\n",
                empty_or(&line.kind, "memory"),
                label_of(line)
            ));
        }
        let result = decide_with_hint(
            &state,
            &[observation_accept_question()],
            ai.as_ref(),
            DECIDE_SHADOW_ROUTE_HINT,
        )
        .await
        .map_err(|e| format!("decide({OBSERVATION_ACCEPT_QUESTION_ID}): {e}"))?;
        if let Some(usage) = &result.usage {
            if let Some(db) = &self.db {
                let tracker = ibis_ai::CostTracker::new(Some(
                    Arc::new(DbCostStore(db.clone())) as Arc<dyn ibis_ai::CostStore>
                ));
                tracker
                    .record_usage(
                        "system:decide_shadow",
                        usage.prompt_token_count.max(0) as u64,
                        usage.candidates_token_count.max(0) as u64,
                        usage.cached_tokens(),
                    )
                    .await;
            }
        }
        let answer = result
            .answers
            .get(OBSERVATION_ACCEPT_QUESTION_ID)
            .ok_or_else(|| {
                format!("decide({OBSERVATION_ACCEPT_QUESTION_ID}): answer missing from the result")
            })?;
        Ok(ObservationDecideShadow {
            observation: candidate.id.clone(),
            rubric: OBSERVATION_ACCEPT_RUBRIC_VERSION.to_string(),
            best: answer.best.clone(),
            probs: answer.probs.clone(),
            confidence: answer.confidence,
            agreement: answer.best == "accept",
        })
    }
}

enum SynthesisOutcome {
    Summarized,
    Fallback,
    EmbeddingRetry,
}

/// Single-row UPDATE of the synthesized row (name/summary/footer +
/// `generated_by` + optional embedding). An embedding write rejected by the
/// engine (dimension mismatch, provider drift) degrades to a write without
/// the embedding — the row persists with `embedding = NONE` and the next
/// pass retries the vector only (§4.7 row 6); the summary is never lost.
async fn apply_observation_synthesis(
    db: &Arc<dyn Executor>,
    id: &str,
    name: &str,
    content: &str,
    generated_by: &str,
    embedding: Option<&Value>,
) -> Result<(), String> {
    if !is_safe_record_id(id) {
        return Err(format!("unsafe observation record id {id:?}"));
    }
    let run = |embedding: Option<&Value>| {
        let mut fields = vec![
            "title = $obs_title".to_string(),
            "content = $obs_content".to_string(),
            "status = 'current'".to_string(),
            "details.generated_by = $obs_generated_by".to_string(),
        ];
        let mut vars = Vars::new();
        vars.insert("obs_title".into(), Value::from_t(name));
        vars.insert("obs_content".into(), Value::from_t(content));
        vars.insert("obs_generated_by".into(), Value::from_t(generated_by));
        if let Some(embedding) = embedding {
            vars.insert("obs_embedding".into(), embedding.clone());
            fields.push("embedding = $obs_embedding".to_string());
        }
        fields.push("updated_at = time::now()".to_string());
        let sql = format!("UPDATE {id} SET {};", fields.join(", "));
        let db = db.clone();
        async move {
            db.smart_query(&sql, vars)
                .await
                .map(|_| ())
                .map_err(|e| format!("apply observation synthesis: {e}"))
        }
    };
    match run(embedding).await {
        Ok(()) => Ok(()),
        Err(err) if embedding.is_some() => {
            tracing::warn!(
                "ObservationsPass: embedding write rejected for {id} ({err}); persisting without embedding"
            );
            run(None).await
        }
        Err(err) => Err(err),
    }
}

/// Embedding-only retry UPDATE (`generated_by = 'llm'` rows; zero LLM).
async fn apply_embedding_only(
    db: &Arc<dyn Executor>,
    id: &str,
    embedding: &Value,
) -> Result<(), String> {
    if !is_safe_record_id(id) {
        return Err(format!("unsafe observation record id {id:?}"));
    }
    let mut vars = Vars::new();
    vars.insert("obs_embedding".into(), embedding.clone());
    let sql = format!(
        "UPDATE {id} SET embedding = $obs_embedding, status = 'current', updated_at = time::now();"
    );
    db.smart_query(&sql, vars)
        .await
        .map_err(|e| format!("apply observation embedding: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ibis_ai::types::{Candidate, Content};
    use ibis_ai::{AIProvider, AiError};
    use ibis_db::executor::QueryOutcome;
    use ibis_db::value::Object;
    use ibis_search::memory::observations::ObservationSettings;

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        let mut o = Object::new();
        for (k, v) in pairs {
            o.insert(k, v);
        }
        Value::Object(o)
    }

    fn arr(values: Vec<Value>) -> Value {
        Value::from_t(values)
    }

    fn settings(enabled: bool, cap: usize) -> ObservationSettings {
        ObservationSettings {
            enabled,
            max_llm_summaries_per_pass: cap,
            ..Default::default()
        }
    }

    fn evidence_line(id: &str, title: &str) -> ObservationEvidenceLine {
        ObservationEvidenceLine {
            id: id.to_string(),
            kind: "fix".to_string(),
            label: title.to_string(),
            text: format!("{title}: the wizard crashes when the discovery root is empty."),
        }
    }

    fn candidate() -> ObservationSynthCandidate {
        ObservationSynthCandidate {
            id: "memory:⟨obs_test1⟩".to_string(),
            status: "current".to_string(),
            title: "Fix root cause: empty discovery root".to_string(),
            content: "2 memories share fix_root_cause|empty discovery root.".to_string(),
            visibility: "team".to_string(),
            owner: "ibis.observation_engine".to_string(),
            anchor_signature: "fix_root_cause|empty discovery root".to_string(),
            signatures: vec!["fix_root_cause|empty discovery root".to_string()],
            members: vec!["memory:⟨mem_a⟩".to_string(), "memory:⟨mem_b⟩".to_string()],
            evidence_refs: vec![],
            generated_by: "fallback".to_string(),
            has_embedding: false,
        }
    }

    /// Fake DB routing the reconcile extraction + backlog queries.
    struct ObservationsDb {
        candidates: Vec<Value>,
        members: Vec<Value>,
        reject_embedding_write: bool,
        calls: std::sync::Mutex<Vec<(String, Vars)>>,
    }

    impl ObservationsDb {
        fn new(candidates: Vec<Value>, members: Vec<Value>) -> Self {
            ObservationsDb {
                candidates,
                members,
                reject_embedding_write: false,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn sql_calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(sql, _)| sql.clone())
                .collect()
        }

        fn calls_with(&self, fragment: &str) -> Vec<String> {
            self.sql_calls()
                .into_iter()
                .filter(|sql| sql.contains(fragment))
                .collect()
        }
    }

    #[async_trait]
    impl Executor for ObservationsDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
            self.calls
                .lock()
                .unwrap()
                .push((sql.to_string(), Vars::new()));
            if sql.contains("kind = 'observation'") {
                return Ok(QueryOutcome::Single(Value::None));
            }
            if sql.contains("SELECT * FROM memory:") {
                return Ok(QueryOutcome::Single(arr(vec![obj(vec![
                    ("id", Value::from_t("memory:mem_a")),
                    ("visibility", Value::from_t("team")),
                    ("owner", Value::from_t("agent")),
                ])])));
            }
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(
            &self,
            sql: &str,
            vars: Vars,
        ) -> Result<QueryOutcome, ibis_db::DbError> {
            self.calls
                .lock()
                .unwrap()
                .push((sql.to_string(), vars.clone()));
            if self.reject_embedding_write
                && sql.contains("SET title = $obs_title")
                && sql.contains("embedding = $obs_embedding")
            {
                return Err(ibis_db::DbError::Query {
                    message: "Incorrect vector dimension".to_string(),
                    duration_ms: 0,
                });
            }
            if sql.starts_with("SELECT owner FROM memory") {
                return Ok(QueryOutcome::Single(arr(vec![])));
            }
            if sql.contains("details.root_cause AS root_cause") {
                return Ok(QueryOutcome::Single(arr(vec![
                    obj(vec![
                        ("id", Value::from_t("memory:mem_a")),
                        ("kind", Value::from_t("fix")),
                        ("title", Value::from_t("Crash on empty discovery")),
                        (
                            "content",
                            Value::from_t("The wizard crashes on an empty discovery root."),
                        ),
                        ("confidence", Value::from_t(0.7f64)),
                        ("visibility", Value::from_t("team")),
                        ("owner", Value::from_t("agent")),
                        (
                            "details",
                            obj(vec![("root_cause", Value::from_t("empty discovery root"))]),
                        ),
                        ("root_cause", Value::from_t("empty discovery root")),
                        ("source_path", Value::from_t("")),
                        ("introduced_in", Value::from_t("")),
                    ]),
                    obj(vec![
                        ("id", Value::from_t("memory:mem_b")),
                        ("kind", Value::from_t("fix")),
                        ("title", Value::from_t("Wizard fails without root")),
                        (
                            "content",
                            Value::from_t("The wizard fails when no discovery root is set."),
                        ),
                        ("confidence", Value::from_t(0.7f64)),
                        ("visibility", Value::from_t("team")),
                        ("owner", Value::from_t("agent")),
                        (
                            "details",
                            obj(vec![("root_cause", Value::from_t("empty discovery root"))]),
                        ),
                        ("root_cause", Value::from_t("empty discovery root")),
                        ("source_path", Value::from_t("")),
                        ("introduced_in", Value::from_t("")),
                    ]),
                ])));
            }
            if sql.contains("details.signature_hash AS signature_hash") {
                return Ok(QueryOutcome::Single(arr(vec![])));
            }
            if sql.contains("details.generated_by = 'fallback'") {
                return Ok(QueryOutcome::Single(arr(self.candidates.clone())));
            }
            if sql.contains("SELECT id, kind, title, content FROM memory WHERE id IN") {
                return Ok(QueryOutcome::Single(arr(self.members.clone())));
            }
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    fn backlog_row(id: &str, visibility: &str, generated_by: &str, has_embedding: bool) -> Value {
        obj(vec![
            ("id", Value::from_t(id)),
            ("status", Value::from_t("current")),
            (
                "title",
                Value::from_t("Fix root cause: empty discovery root"),
            ),
            (
                "content",
                Value::from_t("2 memories share fix_root_cause|empty discovery root."),
            ),
            ("visibility", Value::from_t(visibility)),
            (
                "owner",
                Value::from_t(if visibility == "personal" {
                    "alice"
                } else {
                    "ibis.observation_engine"
                }),
            ),
            (
                "members",
                arr(vec![
                    Value::from_t("memory:mem_a"),
                    Value::from_t("memory:mem_b"),
                ]),
            ),
            (
                "evidence_refs",
                arr(vec![Value::from_t(
                    "source_file:[repo:⟨x⟩, 'src/wizard.rs']",
                )]),
            ),
            (
                "anchor_signature",
                Value::from_t("fix_root_cause|empty discovery root"),
            ),
            (
                "signatures",
                arr(vec![Value::from_t("fix_root_cause|empty discovery root")]),
            ),
            ("generated_by", Value::from_t(generated_by)),
            ("has_embedding", Value::Bool(has_embedding)),
        ])
    }

    fn member_rows() -> Vec<Value> {
        vec![
            obj(vec![
                ("id", Value::from_t("memory:mem_a")),
                ("kind", Value::from_t("fix")),
                ("title", Value::from_t("Crash on empty discovery")),
                (
                    "content",
                    Value::from_t("The wizard crashes on an empty discovery root."),
                ),
            ]),
            obj(vec![
                ("id", Value::from_t("memory:mem_b")),
                ("kind", Value::from_t("fix")),
                ("title", Value::from_t("Wizard fails without root")),
                (
                    "content",
                    Value::from_t("The wizard fails when no discovery root is set."),
                ),
            ]),
        ]
    }

    /// Fake AI recording prompts/hints; scripted responses.
    struct ObservationsAi {
        responses: std::sync::Mutex<Vec<String>>,
        fail: bool,
        embed_fail: bool,
        route_hints: std::sync::Mutex<Vec<String>>,
        prompts: std::sync::Mutex<Vec<String>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ObservationsAi {
        fn scripted(responses: Vec<&str>) -> Self {
            ObservationsAi {
                responses: std::sync::Mutex::new(responses.into_iter().map(String::from).collect()),
                fail: false,
                embed_fail: false,
                route_hints: std::sync::Mutex::new(Vec::new()),
                prompts: std::sync::Mutex::new(Vec::new()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn failing() -> Self {
            let mut ai = Self::scripted(vec![]);
            ai.fail = true;
            ai
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AIProvider for ObservationsAi {
        fn name(&self) -> &str {
            "observations-ai"
        }

        async fn is_functional(&self) -> bool {
            true
        }

        async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, AiError> {
            if self.embed_fail {
                return Err(AiError::other("embed offline"));
            }
            Ok(vec![0.1, 0.2, 0.3])
        }

        async fn generate_content(
            &self,
            contents: &[Content],
            config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.route_hints
                .lock()
                .unwrap()
                .push(config.route_hint.clone());
            if let Some(first) = contents.first() {
                if let Some(part) = first.parts.first() {
                    self.prompts.lock().unwrap().push(part.text.clone());
                }
            }
            if self.fail {
                return Err(AiError::other("offline"));
            }
            let response = {
                let mut guard = self.responses.lock().unwrap();
                if guard.is_empty() {
                    String::new()
                } else {
                    guard.remove(0)
                }
            };
            Ok(Candidate {
                content: Content {
                    role: "model".into(),
                    parts: vec![Part { text: response }],
                },
                finish_reason: "STOP".into(),
                usage_metadata: Some(ibis_ai::types::UsageMetadata {
                    prompt_token_count: 120,
                    candidates_token_count: 40,
                    total_token_count: 160,
                    cached_content_token_count: None,
                }),
            })
        }
    }

    fn optimizer(settings: ObservationSettings) -> Optimizer {
        let mut optimizer = Optimizer::new(None, None, None);
        optimizer.observations = settings;
        optimizer
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn observation_prompt_has_stable_prefix_and_ordered_evidence() {
        let evidence = vec![
            evidence_line("memory:⟨mem_a⟩", "Crash on empty discovery"),
            evidence_line("memory:⟨mem_b⟩", "Wizard fails without root"),
        ];
        let prompt = build_observation_prompt(
            "fix_root_cause|empty discovery root",
            &["fix_root_cause|empty discovery root".to_string()],
            &evidence,
        );
        assert!(prompt.starts_with(OBSERVATION_INSTRUCTIONS));
        assert!(prompt.contains("SIGNATURE: fix_root_cause|empty discovery root"));
        assert!(prompt.contains("SIGNATURES: fix_root_cause|empty discovery root"));
        let first = prompt.find("memory:⟨mem_a⟩").expect("member a");
        let second = prompt.find("memory:⟨mem_b⟩").expect("member b");
        assert!(first < second, "members in id order");
        assert!(prompt.ends_with("{\"name\":\"...\",\"summary\":\"...\"}"));
        assert!(!prompt.contains("{OBSERVATION_INSTRUCTIONS}"));
    }

    /// D13: accidental edits to the instructions block must update the pin.
    #[test]
    fn observation_instructions_hash_snapshot() {
        // FNV-1a keeps the snapshot dependency-free (sha2 lives in
        // ibis-search, not here).
        fn fnv1a(text: &str) -> u64 {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in text.replace("\r\n", "\n").bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash
        }
        assert_eq!(fnv1a(OBSERVATION_INSTRUCTIONS), 0x9cd2_92e4_7cec_a147);
    }

    #[test]
    fn parse_observation_summary_strips_fences_and_rejects_garbage() {
        let parsed = parse_observation_summary(
            "```json\n{\"name\":\"Empty root crashes\",\"summary\":\"Two fixes share the cause.\"}\n```",
        )
        .expect("fenced json parses");
        assert_eq!(parsed.name, "Empty root crashes");
        assert_eq!(parsed.summary, "Two fixes share the cause.");
        assert!(parse_observation_summary("not json").is_err());
        assert!(parse_observation_summary("{\"name\":\"x\"}").is_ok());
    }

    #[test]
    fn parse_and_validate_summary_rejects_foreign_ids() {
        let candidate = candidate();
        assert!(parse_and_validate_summary(
            "{\"name\":\"ok\",\"summary\":\"Uses memory:⟨mem_a⟩ as evidence.\"}",
            &candidate
        )
        .is_ok());
        assert!(parse_and_validate_summary(
            "{\"name\":\"ok\",\"summary\":\"Invented memory:⟨other⟩ reference.\"}",
            &candidate
        )
        .is_err());
        assert!(
            parse_and_validate_summary("{\"name\":\"\",\"summary\":\"x\"}", &candidate).is_err(),
            "empty name refused"
        );
    }

    #[test]
    fn fallback_summary_is_deterministic() {
        let candidate = candidate();
        let evidence = vec![
            evidence_line("memory:⟨mem_a⟩", "Crash on empty discovery"),
            evidence_line("memory:⟨mem_b⟩", "Wizard fails without root"),
        ];
        let first = fallback_summary(&candidate, &evidence);
        let second = fallback_summary(&candidate, &evidence);
        assert_eq!(first, second);
        assert_eq!(first.name, "Fix root cause: empty discovery root");
        assert!(first
            .summary
            .starts_with("2 memories share fix_root_cause|empty discovery root."));
        assert!(first.summary.contains("Crash on empty discovery"));
        let content = render_observation_content(&first.summary, &evidence_members(&evidence));
        assert!(content.contains("\n\nMembers:\n"));
        assert!(content.contains("- fix: Crash on empty discovery"));
    }

    #[test]
    fn fallback_summary_survives_a_broken_anchor() {
        let mut candidate = candidate();
        candidate.anchor_signature = "bogus".to_string();
        let summary = fallback_summary(&candidate, &[]);
        assert_eq!(
            summary.name, "Fix root cause: empty discovery root",
            "title kept"
        );
        assert!(summary
            .summary
            .contains("2 memories share a structural signature."));
    }

    // ---- the pass ---------------------------------------------------------

    #[tokio::test]
    async fn observations_pass_disabled_is_a_noop() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let mut optimizer = optimizer(settings(false, 10));
        optimizer.db = Some(db.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report, ObservationPassReport::default());
        assert!(db.sql_calls().is_empty(), "no query when disabled");
    }

    #[tokio::test]
    async fn observations_pass_summarizes_fallback_row_with_llm() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"Empty root crashes\",\"summary\":\"Two fixes share the empty-root cause.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");

        assert_eq!(report.summarized, 1);
        assert_eq!(report.fallbacks, 0);
        assert_eq!(report.scopes, 1);
        assert_eq!(ai.calls(), 1);
        assert_eq!(
            ai.route_hints.lock().unwrap().as_slice(),
            ["local:M".to_string()]
        );
        let prompt = ai.prompts.lock().unwrap()[0].clone();
        assert!(prompt.contains("mem_a"), "member ids visible: {prompt}");

        let apply: Vec<String> = db.calls_with("SET title = $obs_title");
        assert_eq!(apply.len(), 1, "one apply");
        assert!(apply[0].contains("details.generated_by = $obs_generated_by"));
        assert!(apply[0].contains("embedding = $obs_embedding"));
        // The bound values carry the LLM summary and the llm marker.
        let vars = db
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(sql, _)| sql.contains("SET title = $obs_title"))
            .map(|(_, vars)| vars.clone())
            .expect("apply vars");
        assert_eq!(
            vars.get("obs_title")
                .and_then(Value::as_string)
                .map(String::as_str),
            Some("Empty root crashes")
        );
        assert_eq!(
            vars.get("obs_generated_by")
                .and_then(Value::as_string)
                .map(String::as_str),
            Some("llm")
        );
        // Cost bucket recorded.
        let costs: Vec<String> = db
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(sql, _)| sql.clone())
            .filter(|sql| sql.contains("system:memory_observations"))
            .collect();
        assert_eq!(costs.len(), 1, "{costs:?}");
    }

    #[tokio::test]
    async fn observations_pass_personal_uses_local_only() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row(
                "memory:⟨obs_test1⟩",
                "personal",
                "fallback",
                false,
            )],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"Empty root\",\"summary\":\"Personal pattern.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db);
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 1);
        assert_eq!(
            ai.route_hints.lock().unwrap().as_slice(),
            ["local_only:M".to_string()]
        );
    }

    #[tokio::test]
    async fn observations_pass_falls_back_when_ai_fails() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::failing());
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai);
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 0);
        assert_eq!(report.fallbacks, 1);
        let vars = db
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(sql, _)| sql.contains("SET title = $obs_title"))
            .map(|(_, vars)| vars.clone())
            .expect("fallback apply");
        assert_eq!(
            vars.get("obs_generated_by")
                .and_then(Value::as_string)
                .map(String::as_str),
            Some("fallback")
        );
        assert_eq!(
            vars.get("obs_title")
                .and_then(Value::as_string)
                .map(String::as_str),
            Some("Fix root cause: empty discovery root")
        );
    }

    #[tokio::test]
    async fn observations_pass_retries_once_on_invalid_summary() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"bad\",\"summary\":\"Invented memory:⟨other⟩ reference.\"}",
            "{\"name\":\"Empty root crashes\",\"summary\":\"Two fixes share the cause.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db);
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 1, "second attempt accepted");
        assert_eq!(ai.calls(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn observations_pass_refuses_after_retry_and_falls_back() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"bad\",\"summary\":\"Invented memory:⟨other⟩ reference.\"}",
            "{\"name\":\"still bad\",\"summary\":\"Invented memory:⟨other2⟩ again.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db);
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 0);
        assert_eq!(report.fallbacks, 1);
        assert_eq!(ai.calls(), 2, "one retry only");
    }

    #[tokio::test]
    async fn observations_pass_embedding_retry_skips_llm() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "llm", false)],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.embedding_retries, 1);
        assert_eq!(ai.calls(), 0, "zero LLM for an embedding retry");
        let retries: Vec<String> = db.calls_with("SET embedding = $obs_embedding");
        assert_eq!(retries.len(), 1, "{retries:?}");
        assert!(
            !retries[0].contains("title"),
            "no title rewrite: {}",
            retries[0]
        );
    }

    #[tokio::test]
    async fn observations_pass_backlog_excludes_archived_and_superseded() {
        let db = Arc::new(ObservationsDb::new(vec![], vec![]));
        let ai = Arc::new(ObservationsAi::scripted(vec![]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai);
        optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        let backlog: Vec<String> = db
            .calls_with("details.generated_by = 'fallback'")
            .into_iter()
            .filter(|sql| sql.contains("kind = 'observation'"))
            .collect();
        assert_eq!(backlog.len(), 1, "{backlog:?}");
        assert!(
            backlog[0].contains("status IN ['current', 'stale']"),
            "archived/superseded rows must not be resurrected by synthesis: {}",
            backlog[0]
        );
    }

    #[tokio::test]
    async fn observations_pass_caps_llm_summaries() {
        let db = Arc::new(ObservationsDb::new(
            vec![
                backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false),
                backlog_row("memory:⟨obs_test2⟩", "team", "fallback", false),
            ],
            member_rows(),
        ));
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"One\",\"summary\":\"First.\"}",
            "{\"name\":\"Two\",\"summary\":\"Second.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 1));
        optimizer.db = Some(db);
        optimizer.ai = Some(ai.clone());
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 1);
        assert_eq!(report.deferred, 1, "second row waits for the next pass");
        assert_eq!(ai.calls(), 1, "cap 1 → one LLM call");
    }

    #[tokio::test]
    async fn observations_pass_embedding_failure_still_persists_row() {
        let db = Arc::new(ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        ));
        let mut ai = ObservationsAi::scripted(vec![
            "{\"name\":\"Empty root\",\"summary\":\"Two fixes share the cause.\"}",
        ]);
        ai.embed_fail = true;
        let ai = Arc::new(ai);
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai);
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.summarized, 1);
        let apply: Vec<String> = db.calls_with("SET title = $obs_title");
        assert_eq!(apply.len(), 1);
        assert!(
            !apply[0].contains("embedding = $obs_embedding"),
            "no embedding bound when the provider fails: {}",
            apply[0]
        );
    }

    #[tokio::test]
    async fn observations_pass_degrades_when_embedding_write_is_rejected() {
        let mut db = ObservationsDb::new(
            vec![backlog_row("memory:⟨obs_test1⟩", "team", "fallback", false)],
            member_rows(),
        );
        db.reject_embedding_write = true;
        let db = Arc::new(db);
        let ai = Arc::new(ObservationsAi::scripted(vec![
            "{\"name\":\"Empty root\",\"summary\":\"Two fixes share the cause.\"}",
        ]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai);
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(
            report.summarized, 1,
            "the summary is never lost: {report:?}"
        );
        assert_eq!(report.failed, 0);
        let applies: Vec<String> = db.calls_with("SET title = $obs_title");
        assert_eq!(
            applies.len(),
            2,
            "embedding write rejected, retried without it: {applies:?}"
        );
        assert!(!applies[1].contains("embedding = $obs_embedding"));
    }

    #[tokio::test]
    async fn observations_pass_defers_extra_scopes() {
        let db = Arc::new(ObservationsDb::new(
            vec![], // no backlog rows anywhere
            vec![],
        ));
        // Scope discovery: two personal owners. `max_scopes_per_project = 2`
        // keeps team + the first owner and defers the second.
        let db = Arc::new(ScopedDb {
            inner: db,
            owners: vec!["alice", "bob"],
        });
        let ai = Arc::new(ObservationsAi::scripted(vec![]));
        let mut optimizer = optimizer(settings(true, 10));
        optimizer.observations.max_scopes_per_project = 2;
        optimizer.db = Some(db.clone());
        optimizer.ai = Some(ai);
        let report = optimizer
            .observations_pass("alpha", None)
            .await
            .expect("ok");
        assert_eq!(report.scopes, 2, "team + alice");
        let scope_owners: Vec<String> = db
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(sql, _)| sql.contains("owner = $scope_owner"))
            .filter_map(|(_, vars)| vars.get("scope_owner").and_then(Value::as_string).cloned())
            .collect();
        assert!(!scope_owners.is_empty());
        assert!(
            scope_owners.iter().all(|owner| owner == "alice"),
            "bob's scope deferred: {scope_owners:?}"
        );
    }

    struct ScopedDb {
        inner: Arc<ObservationsDb>,
        owners: Vec<&'static str>,
    }

    #[async_trait]
    impl Executor for ScopedDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
            self.inner.execute(sql).await
        }

        async fn smart_query(
            &self,
            sql: &str,
            vars: Vars,
        ) -> Result<QueryOutcome, ibis_db::DbError> {
            if sql.starts_with("SELECT owner FROM memory") {
                self.inner
                    .calls
                    .lock()
                    .unwrap()
                    .push((sql.to_string(), vars));
                let rows: Vec<Value> = self
                    .owners
                    .iter()
                    .map(|owner| obj(vec![("owner", Value::from_t(*owner))]))
                    .collect();
                return Ok(QueryOutcome::Single(arr(rows)));
            }
            self.inner.smart_query(sql, vars).await
        }
    }
}
