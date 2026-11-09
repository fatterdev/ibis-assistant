//! Sleeptime memory consolidation (
//! ).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use ibis_ai::decide::{decide_with_hint, DecideAnswer, DecideQuestion};
use ibis_ai::types::GenerationConfig;
use ibis_db::executor::Executor;
use ibis_db::value::{Embedding, RecordId, SurrealValue, Value, F64};
use ibis_search::search::SearchService;

use crate::raft::Optimizer;
use crate::store::DbCostStore;

const DEFAULT_CONSOLIDATION_ITEMS: usize = 20;
const NOISE_RETENTION: &str = "7d";

/// Minimum cosine similarity for a duplicate candidate to reach the
/// distillation prompt (C9): candidates scoring below it are dropped
/// post-query, so irrelevant rows never push the model toward bad merges.
/// Overridable via `ai.consolidation.dup_similarity_min`.
pub(crate) const DUP_SIMILARITY_MIN: f64 = 0.80;

/// D10 per-task routing: distillation is a mid-tier local task (cloud
/// fallback preserved by the `local:` base). Personal observations keep the
/// no-cloud guarantee through the `local_only:` base.
const CONSOLIDATION_ROUTE_HINT: &str = "local:M";
const CONSOLIDATION_PERSONAL_ROUTE_HINT: &str = "local_only:M";

/// D11: bounded-decision question id and rubric version. The calibration
/// dataset (`evaluations/decisions/cases.yaml`) pins the version — a rubric
/// change requires a new dataset.
pub const IS_NOISE_QUESTION_ID: &str = "is_noise";
pub const IS_NOISE_RUBRIC_VERSION: &str = "is_noise.v1";

/// D11 shadow-mode routing: the measurement must run on the tier-S local
/// model. A cloud fallback would calibrate a different model and spend cloud
/// budget; `local_only` also enforces the L3 edge case (missing tier-S model
/// → skip shadow, log once).
const DECIDE_SHADOW_ROUTE_HINT: &str = "local_only:S";

const VALID_CONSOLIDATION_KINDS: [&str; 9] = [
    "decision",
    "fix",
    "constraint",
    "rule",
    "gotcha",
    "design",
    "fact",
    "working_state",
    "reasoning",
];

/// The AI classification verdict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsolidationDecision {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default, rename = "merge_with")]
    pub merge_with: String,
    #[serde(default, rename = "valid_until")]
    pub valid_until: String,
    /// C7 structured attributes (per-kind rubric); null when the model
    /// omitted them.
    #[serde(default)]
    pub details: serde_json::Value,
}

/// D11 shadow-mode record: what the bounded decision would have answered next
/// to the generative verdict. Logged under the `decide_shadow` target; the
/// consolidation behavior never changes while shadow mode is on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecideShadow {
    pub observation_id: String,
    pub rubric: String,
    /// The generative action (`noise` | `create` | `merge`).
    pub decision_llm: String,
    /// The bounded decision's highest-probability option.
    pub best: String,
    pub probs: BTreeMap<String, f64>,
    pub confidence: f64,
    /// `decision_llm == "noise"` matches `best == "noise"`.
    pub agreement: bool,
}

/// One candidate observation row.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationObservation {
    pub id: String,
    /// Stored title (empty for raw `capture_observation` rows); the
    /// deterministic pre-match falls back to the first content line (C9).
    pub title: String,
    pub content: String,
    pub embedding: Option<Embedding>,
    pub confidence: f64,
    pub owner: String,
    pub visibility: String,
}

/// One same-topic memory candidate offered to the distillation prompt
///
#[derive(Debug, Clone, Default, Serialize)]
pub struct DuplicateMemory {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub score: f64,
}

/// `SELECT id, title, content, embedding, confidence, owner, visibility FROM memory …`
/// row projection.
#[derive(Debug, Clone, SurrealValue)]
struct ObservationRow {
    id: Option<RecordId>,
    title: Option<String>,
    content: Option<String>,
    embedding: Option<Embedding>,
    confidence: Option<F64>,
    owner: Option<String>,
    visibility: Option<String>,
}

/// Duplicate-candidate row projection (includes the cosine `score` alias).
#[derive(Debug, Clone, SurrealValue)]
struct DuplicateRow {
    id: Option<RecordId>,
    kind: Option<String>,
    title: Option<String>,
    content: Option<String>,
    visibility: Option<String>,
    owner: Option<String>,
    score: Option<F64>,
}

/// `SELECT project FROM memory … GROUP BY project` row projection.
#[derive(Debug, Clone, SurrealValue)]
struct ProjectRow {
    project: Option<RecordId>,
}

impl Optimizer {
    /// Drains raw observations oldest-first. Routes the distillation to the
    /// tier-M local model (`local:M`, cloud fallback preserved; personal
    /// observations stay `local_only:M`), distills durable typed memories,
    /// merges semantic duplicates, and gives routine noise a short stale
    /// lifetime.
    pub async fn consolidate_loop(
        &self,
        project_name: &str,
        max_items: usize,
    ) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("database not connected")?;
        if self.ai.is_none() {
            return Err("AI provider not configured".to_string());
        }
        let max_items = if max_items == 0 {
            DEFAULT_CONSOLIDATION_ITEMS
        } else {
            max_items
        };

        let observations = self
            .select_observations(db, project_name, max_items)
            .await?;
        tracing::info!(
            "ConsolidateLoop: {} candidate observations for {}",
            observations.len(),
            project_name
        );

        let memory_service: Option<Arc<SearchService>> = match &self.search {
            Some(s) => Some(s.clone()),
            None => self
                .ai
                .as_ref()
                .map(|ai| Arc::new(SearchService::new(self.db.clone(), Some(ai.clone())))),
        };
        // D11: a failing decide shadow disables the measurement for the rest
        // of the pass (missing tier-S model, malformed output) and logs once.
        let mut shadow_skipped = false;
        for (index, observation) in observations.iter().enumerate() {
            let duplicates = match self
                .find_duplicate_memories(db, project_name, observation)
                .await
            {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        "ConsolidateLoop: duplicate lookup failed for {}: {e}",
                        observation.id
                    );
                    Vec::new()
                }
            };
            // C9 deterministic pre-match: when the normalized observation
            // title equals a candidate's, merge directly without an AI call
            // (same persist flow as an LLM merge).
            let (decision, used_llm) = match deterministic_title_match(observation, &duplicates) {
                Some(decision) => {
                    tracing::info!(
                        "ConsolidateLoop: deterministic duplicate match for {} -> {}",
                        observation.id,
                        decision.merge_with
                    );
                    (decision, false)
                }
                None => match self.distill_observation(observation, &duplicates).await {
                    Ok(d) => (d, true),
                    Err(e) => {
                        tracing::warn!(
                            "ConsolidateLoop: distillation failed for {}: {e}",
                            observation.id
                        );
                        continue;
                    }
                },
            };
            // D11 shadow mode: measure what the bounded decision would have
            // said next to the generative verdict. Behavior never changes
            // while shadow is on; the deterministic pre-match stays
            // zero-AI by construction (C9), so it is not shadowed.
            if used_llm && self.decide_shadow && !shadow_skipped {
                match self
                    .shadow_decide(observation, &duplicates, &decision)
                    .await
                {
                    Ok(shadow) => {
                        tracing::info!(
                            target: "decide_shadow",
                            observation = %shadow.observation_id,
                            rubric = %shadow.rubric,
                            decision_llm = %shadow.decision_llm,
                            best = %shadow.best,
                            probs = ?shadow.probs,
                            confidence = shadow.confidence,
                            agreement = shadow.agreement,
                            "decide_shadow"
                        );
                    }
                    Err(e) => {
                        shadow_skipped = true;
                        tracing::warn!(
                            "ConsolidateLoop: decide shadow disabled for this pass: {e}"
                        );
                    }
                }
            }
            if decision.action == "noise" {
                if let Err(e) = self.decay_noise(db, observation).await {
                    tracing::warn!(
                        "ConsolidateLoop: noise decay failed for {}: {e}",
                        observation.id
                    );
                }
                continue;
            }
            let Some(memory_service) = &memory_service else {
                tracing::warn!(
                    "ConsolidateLoop: persistence failed for {}: no memory service",
                    observation.id
                );
                continue;
            };
            if let Err(e) = self
                .persist_consolidation(db, memory_service, project_name, observation, &decision)
                .await
            {
                tracing::warn!(
                    "ConsolidateLoop: persistence failed for {}: {e}",
                    observation.id
                );
                continue;
            }
            tracing::info!(
                "ConsolidateLoop: processed {}/{} observation {} as {}",
                index + 1,
                observations.len(),
                observation.id,
                decision.kind
            );
        }
        Ok(())
    }

    /// `MemoryConsolidate: the MCP-design name for the reusable loop.
    pub async fn memory_consolidate(
        &self,
        project_name: &str,
        max_items: usize,
    ) -> Result<(), String> {
        self.consolidate_loop(project_name, max_items).await
    }

    /// Projects that still hold `current` observations (F4 §B.5: the
    /// daemon-owned loop discovers its work in the store, so team clients
    /// can capture for repos unknown to the server config). One pass per
    /// project keeps `consolidate_loop`'s per-project filter intact.
    ///
    /// C8 §4.9: two discovery branches, deduplicated in Rust — the raw
    /// backlog (non-derived `current` observations) plus the derived backlog
    /// (stale observations, fallback summaries pending an upgrade, rows
    /// without an embedding). Without the second branch a project whose only
    /// work is derived observations would never run the pass.
    ///
    /// The derived branch is bounded to `kind = 'observation'` and
    /// `status IN ['current', 'stale']`: archived/superseded derived rows
    /// (which can still carry `generated_by = 'fallback'` or no embedding)
    /// must not keep their project selected forever.
    pub async fn pending_observation_projects(&self) -> Result<Vec<String>, String> {
        let db = self.db.as_ref().ok_or("database not connected")?;
        let raw_query = "SELECT project FROM memory WHERE kind = 'observation' AND status = 'current' AND (store = NONE OR store != 'derived') GROUP BY project;";
        let derived_query = "SELECT project FROM memory WHERE kind = 'observation' AND store = 'derived' AND status IN ['current', 'stale'] AND (status = 'stale' OR details.generated_by = 'fallback' OR embedding = NONE) GROUP BY project;";
        let mut projects: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for query in [raw_query, derived_query] {
            let outcome = db
                .execute(query)
                .await
                .map_err(|e| format!("select observation projects: {e}"))?;
            let rows = ibis_db::value::decode_row_list::<ProjectRow>(
                outcome.first().unwrap_or(&Value::None),
                query,
            )
            .map_err(|e| format!("decode observation projects: {e}"))?;
            for row in rows {
                let Some(rid) = row.project else { continue };
                let plain = ibis_db::ids::record_id_to_plain(&rid);
                // `repo:<key>` — the key is the sanitized project name
                // (`sanitize_id` output is idempotent, so passing it back
                // into `consolidate_loop` re-derives the same record).
                let Some(name) = plain.strip_prefix("repo:") else {
                    continue;
                };
                let name = name.trim().to_string();
                if !name.is_empty() && seen.insert(name.clone()) {
                    projects.push(name);
                }
            }
        }
        Ok(projects)
    }

    async fn select_observations(
        &self,
        db: &Arc<dyn Executor>,
        project_name: &str,
        max_items: usize,
    ) -> Result<Vec<ConsolidationObservation>, String> {
        // C8 §4.9: derived observations (the observation engine's rows) are
        // NOT raw distillation input — without this exclusion the
        // consolidation would distill its own artifacts and the daemon would
        // treat them as perpetual backlog.
        let mut where_parts = vec![
            "kind = 'observation'",
            "status = 'current'",
            "(store = NONE OR store != 'derived')",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let project = project_name.trim();
        if !project.is_empty() {
            where_parts.push(format!(
                "project = {}",
                ibis_db::ids::format_record_id("repo", &ibis_db::ids::sanitize_id(project))
            ));
        }
        let query = format!(
            "SELECT id, title, content, embedding, confidence, owner, visibility FROM memory WHERE {} ORDER BY created_at ASC LIMIT {max_items};",
            where_parts.join(" AND ")
        );
        let outcome = db
            .execute(&query)
            .await
            .map_err(|e| format!("select observations: {e}"))?;
        let rows = ibis_db::value::decode_row_list::<ObservationRow>(
            outcome.first().unwrap_or(&Value::None),
            &query,
        )
        .map_err(|e| format!("parse observations: {e}"))?;
        let mut observations = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row
                .id
                .as_ref()
                .map(ibis_db::ids::record_id_to_plain)
                .unwrap_or_default();
            let content = row.content.unwrap_or_default();
            if !ibis_db::ids::is_safe_record_id(&id) || content.trim().is_empty() {
                continue;
            }
            observations.push(ConsolidationObservation {
                id,
                title: row.title.unwrap_or_default(),
                content,
                embedding: row.embedding,
                confidence: row.confidence.map(|c| c.get()).unwrap_or(0.5),
                owner: row.owner.unwrap_or_default(),
                visibility: row.visibility.unwrap_or_default(),
            });
        }
        Ok(observations)
    }

    async fn find_duplicate_memories(
        &self,
        db: &Arc<dyn Executor>,
        project_name: &str,
        observation: &ConsolidationObservation,
    ) -> Result<Vec<DuplicateMemory>, String> {
        let Some(embedding) = &observation.embedding else {
            return Ok(Vec::new());
        };
        // Match candidate visibility to the observation so personal content
        // never enters a team distill prompt (the team `local:M` hint can
        // still fall back to cloud). Owner binds (F-002) — user-controlled
        // value.
        let owner = observation.owner.trim();
        let (visibility_clause, bindings): (String, ibis_db::executor::Vars) = if observation
            .visibility
            .trim()
            .eq_ignore_ascii_case("personal")
        {
            if owner.is_empty() {
                return Ok(Vec::new());
            }
            let mut vars = ibis_db::executor::Vars::new();
            vars.insert("dup_owner".into(), Value::from_t(owner));
            (
                "visibility = 'personal' AND owner = $dup_owner".to_string(),
                vars,
            )
        } else {
            ("visibility = 'team'".to_string(), Default::default())
        };
        let mut where_parts = vec![
            "kind != 'observation'".to_string(),
            "status = 'current'".to_string(),
            visibility_clause,
        ];
        let project = project_name.trim();
        if !project.is_empty() {
            where_parts.push(format!(
                "project = {}",
                ibis_db::ids::format_record_id("repo", &ibis_db::ids::sanitize_id(project))
            ));
        }
        let query = format!(
            "SELECT id, kind, title, content, visibility, owner, vector::similarity::cosine(embedding, $obs_embedding) AS score FROM memory WHERE {} ORDER BY vector::distance::cosine(embedding, $obs_embedding) LIMIT 3;",
            where_parts.join(" AND ")
        );
        let mut vars = bindings;
        vars.insert("obs_embedding".into(), embedding.clone().into_value());
        let outcome = db
            .smart_query(&query, vars)
            .await
            .map_err(|e| e.to_string())?;
        let rows = ibis_db::value::decode_row_list::<DuplicateRow>(
            outcome.first().unwrap_or(&Value::None),
            &query,
        )
        .map_err(|e| format!("unexpected duplicate result type: {e}"))?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row
                .id
                .as_ref()
                .map(ibis_db::ids::record_id_to_plain)
                .unwrap_or_default();
            if !ibis_db::ids::is_safe_record_id(&id) {
                continue;
            }
            result.push(DuplicateMemory {
                id,
                kind: row.kind.unwrap_or_default(),
                title: row.title.unwrap_or_default(),
                content: row.content.unwrap_or_default(),
                score: row.score.map(|s| s.get()).unwrap_or(0.0),
            });
        }
        // C9: the top-3 window is similarity-filtered — rows below the floor
        // (including embedding-less rows, which score 0.0) never reach the
        // distillation prompt.
        let min = self.dup_similarity_min.clamp(0.0, 1.0);
        result.retain(|duplicate| duplicate.score >= min);
        Ok(result)
    }

    async fn distill_observation(
        &self,
        observation: &ConsolidationObservation,
        duplicates: &[DuplicateMemory],
    ) -> Result<ConsolidationDecision, String> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| "AI provider not configured".to_string())?;
        let candidates = serde_json::to_string(duplicates).unwrap_or_else(|_| "[]".to_string());
        let prompt = build_distill_prompt(&observation.content, &candidates);
        // Personal observation content must not leave the machine via cloud
        // fallback.
        let route_hint = if observation
            .visibility
            .trim()
            .eq_ignore_ascii_case("personal")
        {
            CONSOLIDATION_PERSONAL_ROUTE_HINT
        } else {
            CONSOLIDATION_ROUTE_HINT
        };
        let contents = [ibis_ai::Content {
            role: "user".to_string(),
            parts: vec![ibis_ai::Part { text: prompt }],
        }];
        // C7 strict mode retries the distillation ONCE when the per-kind
        // required details keys are missing; warn mode keeps the partial
        // details with a log line. Every attempt is billed.
        let mut strict_retried = false;
        loop {
            let response = ai
                .generate_content(
                    &contents,
                    GenerationConfig {
                        route_hint: route_hint.to_string(),
                        temperature: 0.1,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            if let Some(usage) = &response.usage_metadata {
                if let Some(db) = &self.db {
                    let tracker = ibis_ai::CostTracker::new(Some(
                        Arc::new(DbCostStore(db.clone())) as Arc<dyn ibis_ai::CostStore>,
                    ));
                    tracker
                        .record_usage(
                            "system:memory_consolidation",
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
            let decision = parse_consolidation_decision(&text)?;
            if decision.action == "merge"
                && !contains_duplicate_id(duplicates, &decision.merge_with)
            {
                return Err(format!(
                    "merge target {:?} was not a supplied candidate",
                    decision.merge_with
                ));
            }
            // Missing `details` counts as an empty object here: the
            // distillation prompt asks for the rubric keys, so strict mode
            // must not silently accept their absence (the add-path stays
            // backward compatible because its `details` param is optional).
            let effective_details = if decision.details.is_null() {
                serde_json::json!({})
            } else {
                decision.details.clone()
            };
            if let Err(msg) = ibis_search::memory::details::validate_memory_details(
                &decision.kind,
                &effective_details,
            ) {
                match self.schema_enforcement {
                    ibis_search::memory::details::SchemaEnforcement::Strict => {
                        if strict_retried {
                            return Err(format!("distill details: {msg}"));
                        }
                        strict_retried = true;
                        continue;
                    }
                    ibis_search::memory::details::SchemaEnforcement::Warn => {
                        tracing::warn!("distill details: {msg}");
                    }
                }
            }
            return Ok(decision);
        }
    }

    /// D11 shadow mode: computes the bounded `is_noise` decision over the
    /// observation and its duplicate window. The measurement is local-only
    /// (`local_only:S`) for team and personal content alike: it calibrates
    /// the tier-S model, so a cloud fallback would measure the wrong model. A
    /// missing tier-S model fails here and the caller logs it once.
    async fn shadow_decide(
        &self,
        observation: &ConsolidationObservation,
        duplicates: &[DuplicateMemory],
        decision: &ConsolidationDecision,
    ) -> Result<DecideShadow, String> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| "AI provider not configured".to_string())?;
        let state = build_is_noise_state(&observation.content, duplicates);
        let result = decide_with_hint(
            &state,
            &[is_noise_question()],
            ai.as_ref(),
            DECIDE_SHADOW_ROUTE_HINT,
        )
        .await
        .map_err(|e| format!("decide({IS_NOISE_QUESTION_ID}): {e}"))?;
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
        let answer = result.answers.get(IS_NOISE_QUESTION_ID).ok_or_else(|| {
            format!("decide({IS_NOISE_QUESTION_ID}): answer missing from the result")
        })?;
        Ok(decide_shadow_record(&observation.id, decision, answer))
    }

    async fn persist_consolidation(
        &self,
        db: &Arc<dyn Executor>,
        service: &SearchService,
        project_name: &str,
        observation: &ConsolidationObservation,
        decision: &ConsolidationDecision,
    ) -> Result<(), String> {
        let supersedes = if decision.action == "merge" {
            decision.merge_with.trim().to_string()
        } else {
            observation.id.clone()
        };
        let confidence = decision.confidence;
        let mut visibility = observation.visibility.clone();
        if visibility != "personal" {
            visibility = "team".to_string();
        }
        let (workspace_root, anchor_mode) = match &self.workspace {
            Some(resolve) => resolve(project_name),
            None => (String::new(), String::new()),
        };
        let mut params = ibis_search::memory::add::AddMemoryParams {
            project_name: project_name.to_string(),
            kind: decision.kind.clone(),
            title: decision.title.clone(),
            content: decision.content.clone(),
            status: "current".to_string(),
            confidence: Some(confidence),
            visibility,
            owner: observation.owner.clone(),
            valid_until: decision.valid_until.clone(),
            provenance_tool: "memory_consolidate".to_string(),
            supersedes,
            workspace_root,
            anchor_mode,
            details: if decision.details.is_null() {
                None
            } else {
                Some(decision.details.clone())
            },
            ..Default::default()
        };
        let actor = ibis_search::ActorContext::LOCAL;
        let new_id = service
            .add_memory(&actor, &mut params)
            .await
            .map_err(|e| e.to_string())?;

        let valid_until = if decision.valid_until.trim().is_empty() {
            "time::now()".to_string()
        } else {
            format!(
                "d'{}'",
                ibis_db::ids::escape_sql(decision.valid_until.trim())
            )
        };
        let observation_id = observation.id.clone();
        let mut query = String::from("BEGIN TRANSACTION;\n");
        if decision.action == "merge" && ibis_db::ids::is_safe_record_id(&observation_id) {
            // D17 R4: both endpoints are persisted (new memory + selected
            // observation) before the RELATE.
            query.push_str(&format!("RELATE {new_id}->supersedes->{observation_id};\n"));
        }
        query.push_str(&format!(
            "UPDATE {observation_id} SET status = 'archived', valid_until = {valid_until}, updated_at = time::now();\n"
        ));
        query.push_str("COMMIT TRANSACTION;");
        db.execute(&query)
            .await
            .map_err(|e| format!("archive source observation: {e}"))?;
        Ok(())
    }

    async fn decay_noise(
        &self,
        db: &Arc<dyn Executor>,
        observation: &ConsolidationObservation,
    ) -> Result<(), String> {
        let mut confidence = observation.confidence * 0.5;
        if confidence < 0.1 {
            confidence = 0.1;
        }
        let id = observation.id.clone();
        if !ibis_db::ids::is_safe_record_id(&id) {
            return Err(format!("unsafe observation id {id}"));
        }
        // Scenario: formats with 6 decimal places (`strconv.FormatFloat(…, 'f', 6, 64)`).
        let query = format!(
            "UPDATE {id} SET status = 'stale', confidence = {confidence:.6}, valid_until = time::now() + {NOISE_RETENTION}, updated_at = time::now();"
        );
        db.execute(&query).await.map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Normalizes a title for the deterministic duplicate pre-match (C9):
/// lowercase, trimmed, path separators become spaces, whitespace collapsed,
/// trailing sentence punctuation stripped.
fn normalize_title(value: &str) -> String {
    let lowered = value.trim().to_lowercase();
    let spaced = lowered.replace(['\\', '/'], " ");
    let mut out = String::with_capacity(spaced.len());
    let mut previous_space = true;
    for ch in spaced.chars() {
        if ch.is_whitespace() {
            if !previous_space {
                out.push(' ');
                previous_space = true;
            }
        } else {
            out.push(ch);
            previous_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    // A title never ends in sentence punctuation; the first-line fallback
    // (sentence text) does. Strip it so "wizard discovery quirks." matches
    // the "Wizard discovery quirks" title it restates (C9).
    while out.ends_with(['.', '!', '?', ':', ';', ',']) {
        out.pop();
    }
    out
}

/// The observation title used by the deterministic pre-match (C9): the
/// stored title when present, otherwise the first content line capped at 80
/// characters — the same derivation the git claim store applies to untitled
/// rows.
fn observation_title(observation: &ConsolidationObservation) -> String {
    let stored = observation.title.trim();
    if !stored.is_empty() {
        return stored.to_string();
    }
    let first = observation
        .content
        .lines()
        .next()
        .unwrap_or_default()
        .trim();
    first.chars().take(80).collect()
}

/// C9: when a duplicate candidate shares the observation's normalized title,
/// the re-observation restates the same durable fact — merge into it without
/// an AI call (entity resolution). Ties resolve to the nearest candidate,
/// which is the first in the cosine-ordered window.
fn deterministic_title_match(
    observation: &ConsolidationObservation,
    duplicates: &[DuplicateMemory],
) -> Option<ConsolidationDecision> {
    let normalized = normalize_title(&observation_title(observation));
    if normalized.is_empty() {
        return None;
    }
    let candidate = duplicates
        .iter()
        .find(|duplicate| normalize_title(&duplicate.title) == normalized)?;
    Some(ConsolidationDecision {
        action: "merge".to_string(),
        kind: candidate.kind.clone(),
        title: candidate.title.clone(),
        content: candidate.content.clone(),
        confidence: observation.confidence,
        merge_with: candidate.id.clone(),
        valid_until: String::new(),
        details: serde_json::Value::Null,
    })
}

/// Parses and validates the model's consolidation verdict
///
pub fn parse_consolidation_decision(text: &str) -> Result<ConsolidationDecision, String> {
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
    let mut decision: ConsolidationDecision =
        serde_json::from_str(text.trim()).map_err(|e| format!("parse decision JSON: {e}"))?;
    decision.action = decision.action.trim().to_lowercase();
    decision.kind = decision.kind.trim().to_lowercase();
    decision.merge_with = decision.merge_with.trim().to_string();
    match decision.action.as_str() {
        "noise" => return Ok(decision),
        "create" | "merge" => {}
        other => {
            return Err(format!("unsupported consolidation action {other:?}"));
        }
    }
    if !VALID_CONSOLIDATION_KINDS.contains(&decision.kind.as_str()) {
        return Err(format!("unsupported memory kind {:?}", decision.kind));
    }
    if decision.content.trim().is_empty() {
        return Err("consolidated content is empty".to_string());
    }
    if decision.confidence <= 0.0 || decision.confidence > 1.0 {
        decision.confidence = 0.6;
    }
    if !decision.valid_until.is_empty()
        && DateTime::parse_from_rfc3339(&decision.valid_until).is_err()
    {
        return Err(format!("invalid valid_until: {}", decision.valid_until));
    }
    Ok(decision)
}

fn contains_duplicate_id(duplicates: &[DuplicateMemory], id: &str) -> bool {
    duplicates.iter().any(|d| d.id == id)
}

/// D13 prompt stability: byte-stable instructions block for the consolidation
/// distill call site (the call site was already convention-shaped: stable
/// instructions first, variables after; the const pins it by construction).
/// The trailing "Return only JSON" schema stays the final line of the prompt.
const DISTILL_INSTRUCTIONS: &str = r#"Consolidate one raw software-project observation into durable memory.
Classify durable information as one of: decision, fix, constraint, rule, gotcha, design, fact, working_state, reasoning.
Use action "merge" only when a supplied candidate expresses the same durable fact; merge_with must be that candidate's exact ID.
Use action "noise" for routine command output, transient chatter, or information with no reusable value.
For create or merge, rewrite the content as a concise, self-contained statement. Set valid_until only for time-bounded information.
Fill "details" with the kind-specific structured keys when the observation supports them: decision -> scope (required), alternatives[] (array of strings), tradeoffs; fix -> root_cause (required), symptom, remedy; constraint -> applies_to[] (required, array of strings), until; rule -> enforcement (required). Use only the keys you can ground in the observation; omit the rest.
"#;

/// Assembles the distill prompt: byte-stable instructions → observation →
/// duplicates → the return-schema instruction last (D13).
fn build_distill_prompt(observation_content: &str, candidates_json: &str) -> String {
    format!(
        "{DISTILL_INSTRUCTIONS}\nOBSERVATION:\n{observation_content}\n\nPOSSIBLE DUPLICATES:\n{candidates_json}\n\nReturn only JSON:\n{{\"action\":\"create|merge|noise\",\"kind\":\"...\",\"title\":\"...\",\"content\":\"...\",\"confidence\":0.0,\"merge_with\":\"\",\"valid_until\":\"\",\"details\":{{}}}}"
    )
}

/// D11 rubric: the closed `is_noise` question used by the shadow-mode
/// measurement and by `ibis eval decisions`. The rubric version lives in the
/// question text so a calibration dataset pins the exact wording.
pub fn is_noise_question() -> DecideQuestion {
    DecideQuestion::choice(
        IS_NOISE_QUESTION_ID,
        &format!(
            "Rubric {IS_NOISE_RUBRIC_VERSION}. Is the observation routine noise, or does it state a durable project fact, decision, fix, rule, constraint, gotcha, or design rationale worth keeping?"
        ),
        &[
            (
                "noise",
                "routine progress, transient working state, restated context, or content with no durable project value",
            ),
            (
                "durable",
                "a durable project fact, decision, fix, rule, constraint, gotcha, or design rationale worth keeping",
            ),
        ],
    )
}

/// D11 state for the `is_noise` decision: the observation plus the same
/// duplicate window the distillation prompt sees (ids, kinds, titles,
/// contents). Shared by shadow mode and the calibration runner so the
/// measurement matches production exactly.
pub fn build_is_noise_state(observation: &str, duplicates: &[DuplicateMemory]) -> String {
    let mut state = String::from("Observation:\n");
    state.push_str(observation.trim());
    state.push_str("\n\nExisting memories:\n");
    if duplicates.is_empty() {
        state.push_str("(none)");
    } else {
        for duplicate in duplicates {
            state.push_str(&format!(
                "- id: {}\n  kind: {}\n  title: {}\n  content: {}\n",
                duplicate.id, duplicate.kind, duplicate.title, duplicate.content
            ));
        }
    }
    state
}

/// D11 shadow comparison: `noise` vs any durable action (`create`/`merge`).
pub fn decide_shadow_record(
    observation_id: &str,
    decision: &ConsolidationDecision,
    answer: &DecideAnswer,
) -> DecideShadow {
    DecideShadow {
        observation_id: observation_id.to_string(),
        rubric: IS_NOISE_RUBRIC_VERSION.to_string(),
        decision_llm: decision.action.clone(),
        best: answer.best.clone(),
        probs: answer.probs.clone(),
        confidence: answer.confidence,
        agreement: (decision.action == "noise") == (answer.best == "noise"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ibis_ai::types::{Candidate, Content, Part};
    use ibis_ai::AIProvider;
    use ibis_db::executor::{QueryOutcome, Vars};
    use ibis_db::value::Object;
    use ibis_db::value::RecordId;
    use ibis_db::value::ToSql as _;
    use ibis_db::Executor;

    /// D11 log capture: a global subscriber that records only the
    /// `decide_shadow` target events (structured fields as strings). A global
    /// subscriber avoids tracing's process-wide callsite-interest cache being
    /// poisoned by a no-op subscriber in another test; the buffer is shared,
    /// so each test filters by its own observation id.
    #[derive(Default)]
    struct CapturedEvent {
        fields: Vec<(String, String)>,
    }

    #[derive(Default)]
    struct FieldCapture {
        fields: Vec<(String, String)>,
    }

    impl tracing::field::Visit for FieldCapture {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }

        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    static DECIDE_SHADOW_EVENTS: std::sync::OnceLock<std::sync::Mutex<Vec<CapturedEvent>>> =
        std::sync::OnceLock::new();
    static DECIDE_SHADOW_CAPTURE: std::sync::Once = std::sync::Once::new();

    struct DecideShadowSubscriber {
        events: &'static std::sync::Mutex<Vec<CapturedEvent>>,
    }

    impl tracing::Subscriber for DecideShadowSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() != "decide_shadow" {
                return;
            }
            let mut capture = FieldCapture::default();
            event.record(&mut capture);
            self.events.lock().unwrap().push(CapturedEvent {
                fields: capture.fields,
            });
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn install_decide_shadow_capture() -> &'static std::sync::Mutex<Vec<CapturedEvent>> {
        DECIDE_SHADOW_CAPTURE.call_once(|| {
            let events = DECIDE_SHADOW_EVENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
            // Another global subscriber in the test process would win; the
            // capture test then sees an empty buffer and fails loudly.
            let _ = tracing::subscriber::set_global_default(DecideShadowSubscriber { events });
        });
        DECIDE_SHADOW_EVENTS
            .get()
            .expect("capture buffer initialized")
    }

    /// D10 contract: team distillation declares tier M, personal keeps the
    /// tier-M provider but never falls back to cloud.
    #[test]
    fn consolidation_route_hints_are_tier_m() {
        assert_eq!(CONSOLIDATION_ROUTE_HINT, "local:M");
        assert_eq!(CONSOLIDATION_PERSONAL_ROUTE_HINT, "local_only:M");
    }

    /// Builds one object row from `(key, value)` pairs (lane-H test-support
    /// shape, duplicated locally because the crate is `pub(crate)`).
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

    /// Renders a bound variable for assertions (string values verbatim, other
    /// shapes via the SQL rendering).
    fn var_str(v: &Value) -> String {
        v.as_string().cloned().unwrap_or_else(|| v.to_sql())
    }

    /// Scenario: consolidateDB: routes by SQL substring, records calls.
    struct ConsolidateDb {
        observations: Vec<Value>,
        duplicates: Vec<Value>,
        /// `pending_observation_projects` raw branch rows.
        pending_raw: Vec<Value>,
        /// `pending_observation_projects` derived branch rows.
        pending_derived: Vec<Value>,
        calls: std::sync::Mutex<Vec<(String, Vars)>>,
    }

    impl ConsolidateDb {
        fn new(observations: Vec<Value>, duplicates: Vec<Value>) -> Self {
            ConsolidateDb {
                observations,
                duplicates,
                pending_raw: Vec::new(),
                pending_derived: Vec::new(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// C8 §4.9: the two discovery branches of the daemon backlog.
        fn pending(mut self, raw: Vec<Value>, derived: Vec<Value>) -> Self {
            self.pending_raw = raw;
            self.pending_derived = derived;
            self
        }

        fn sql_calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(sql, _)| sql.clone())
                .collect()
        }

        fn vars_of(&self, fragment: &str) -> Vec<Vars> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(sql, _)| sql.contains(fragment))
                .map(|(_, vars)| vars.clone())
                .collect()
        }
    }

    #[async_trait]
    impl Executor for ConsolidateDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
            self.calls
                .lock()
                .unwrap()
                .push((sql.to_string(), Vars::new()));
            if sql.contains("GROUP BY project") {
                if sql.contains("store = 'derived'") {
                    return Ok(QueryOutcome::Single(arr(self.pending_derived.clone())));
                }
                return Ok(QueryOutcome::Single(arr(self.pending_raw.clone())));
            }
            if sql.contains("kind = 'observation'") {
                return Ok(QueryOutcome::Single(arr(self.observations.clone())));
            }
            if sql.contains("kind != 'observation'") {
                return Ok(QueryOutcome::Single(arr(self.duplicates.clone())));
            }
            // Governance probes (add_memory's supersession check) mirror the
            // Scenario: fake: any `SELECT * FROM memory:<id>` returns a team row.
            if sql.starts_with("SELECT * FROM memory:") {
                let id = sql
                    .trim_start_matches("SELECT * FROM ")
                    .trim_end_matches(';');
                return Ok(QueryOutcome::Single(arr(vec![obj(vec![
                    ("id", Value::from_t(id)),
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
            self.calls.lock().unwrap().push((sql.to_string(), vars));
            if sql.contains("kind != 'observation'") {
                return Ok(QueryOutcome::Single(arr(self.duplicates.clone())));
            }
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    /// Scenario: consolidateAI: records prompts and route hints.
    struct ConsolidateAi {
        responses: std::sync::Mutex<Vec<String>>,
        err: Option<String>,
        route_hints: std::sync::Mutex<Vec<String>>,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    impl ConsolidateAi {
        fn new(responses: Vec<&str>) -> Self {
            ConsolidateAi {
                responses: std::sync::Mutex::new(responses.into_iter().map(String::from).collect()),
                err: None,
                route_hints: std::sync::Mutex::new(Vec::new()),
                prompts: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            let mut ai = Self::new(vec![]);
            ai.err = Some("offline".to_string());
            ai
        }
    }

    #[async_trait]
    impl AIProvider for ConsolidateAi {
        fn name(&self) -> &str {
            "consolidate-ai"
        }

        async fn is_functional(&self) -> bool {
            true
        }

        async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, ibis_ai::AiError> {
            Ok(vec![0.1, 0.2])
        }

        async fn generate_content(
            &self,
            contents: &[Content],
            config: GenerationConfig,
        ) -> Result<Candidate, ibis_ai::AiError> {
            self.route_hints
                .lock()
                .unwrap()
                .push(config.route_hint.clone());
            if let Some(first) = contents.first() {
                if let Some(part) = first.parts.first() {
                    self.prompts.lock().unwrap().push(part.text.clone());
                }
            }
            if let Some(err) = &self.err {
                return Err(ibis_ai::AiError::other(err.clone()));
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
                ..Default::default()
            })
        }

        async fn stop(&self) {}
    }

    fn optimizer(db: Arc<ConsolidateDb>, ai: Arc<ConsolidateAi>) -> Optimizer {
        Optimizer::new(
            Some(db as Arc<dyn Executor>),
            Some(ai as Arc<dyn AIProvider>),
            None,
        )
    }

    fn assert_calls_contain(calls: &[String], fragments: &[&str]) {
        let joined = calls.join("\n");
        for fragment in fragments {
            assert!(
                joined.contains(fragment),
                "database calls missing {fragment:?}:\n{joined}"
            );
        }
    }

    fn assert_calls_missing(calls: &[String], fragment: &str) {
        let joined = calls.join("\n");
        assert!(
            !joined.contains(fragment),
            "database calls unexpectedly contain {fragment:?}:\n{joined}"
        );
    }

    /// Scenario: TestConsolidateLoopCreatesTypedMemoryUsingBulkLocal
    /// (add_memory SQL fragments updated to the lane-H bound-variable shape).
    #[tokio::test]
    async fn consolidate_loop_creates_typed_memory_using_local_m() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                (
                    "content",
                    Value::from_t("The wizard crashes when discovery_root is empty."),
                ),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("agent")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{
                "action":"create",
                "kind":"fix",
                "title":"Guard empty discovery root",
                "content":"The setup wizard must guard an empty discovery_root before scanning.",
                "confidence":0.88,
                "valid_until":"2027-01-01T00:00:00Z"
            }"#,
        ]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("ibis", 5)
            .await
            .expect("ConsolidateLoop failed");
        let hints = ai.route_hints.lock().unwrap().clone();
        assert_eq!(hints, vec!["local:M".to_string()], "route hints: {hints:?}");
        let calls = db.sql_calls();
        assert_calls_contain(
            &calls,
            &[
                // add_memory supersession edge (lane-H bound-variable SQL).
                "RELATE $mem->supersedes->memory:obs1",
                // persist transaction: archive the source observation.
                "UPDATE memory:obs1 SET status = 'archived'",
                "valid_until = d'2027-01-01T00:00:00Z'",
            ],
        );
        // The distilled fields ride bound variables (F-002): verify values.
        let add_vars = db.vars_of("CREATE memory:");
        assert_eq!(add_vars.len(), 1, "one add_memory transaction: {calls:?}");
        assert_eq!(
            add_vars[0].get("kind").map(var_str).unwrap_or_default(),
            "fix"
        );
        assert!(
            add_vars[0]
                .get("content")
                .map(var_str)
                .unwrap_or_default()
                .contains("guard an empty discovery_root"),
            "content binding: {:?}",
            add_vars[0].get("content")
        );
    }

    /// C8 §4.9: `select_observations` (the distillation drain) must exclude
    /// derived observations — otherwise the consolidation would distill the
    /// observation engine's own artifacts and the daemon would see a
    /// perpetual backlog.
    #[tokio::test]
    async fn select_observations_excludes_derived() {
        let db = Arc::new(ConsolidateDb::new(Vec::new(), Vec::new()));
        let opt = optimizer(db.clone(), Arc::new(ConsolidateAi::new(vec![])));
        let exec: Arc<dyn Executor> = db.clone();
        let observations = opt
            .select_observations(&exec, "obs_repo", 5)
            .await
            .expect("select observations");
        assert!(observations.is_empty());
        let calls = db.sql_calls();
        let query = calls
            .iter()
            .find(|q| q.contains("kind = 'observation'"))
            .expect("selection query");
        assert!(
            query.contains("(store = NONE OR store != 'derived')"),
            "the drain must exclude derived observations: {query}"
        );
        assert!(query.contains("status = 'current'"), "{query}");
    }

    /// C8 §4.9: the daemon discovers work from BOTH discovery branches — the
    /// raw backlog and the derived backlog (stale / fallback summary /
    /// missing embedding) — deduplicated in Rust. Without the second branch a
    /// project whose only work is derived observations never runs the pass.
    #[tokio::test]
    async fn pending_projects_includes_stale_derived() {
        let repo_row = |key: &str| {
            obj(vec![(
                "project",
                Value::RecordId(RecordId::new("repo", key)),
            )])
        };
        let db = Arc::new(ConsolidateDb::new(Vec::new(), Vec::new()).pending(
            vec![repo_row("raw_repo")],
            vec![repo_row("stale_repo"), repo_row("raw_repo")],
        ));
        let opt = optimizer(db.clone(), Arc::new(ConsolidateAi::new(vec![])));
        let projects = opt
            .pending_observation_projects()
            .await
            .expect("pending projects");
        assert_eq!(
            projects,
            vec!["raw_repo".to_string(), "stale_repo".to_string()]
        );

        let calls = db.sql_calls();
        let raw = calls
            .iter()
            .find(|q| {
                q.contains("GROUP BY project") && q.contains("(store = NONE OR store != 'derived')")
            })
            .expect("raw branch");
        assert!(
            raw.contains("(store = NONE OR store != 'derived')"),
            "{raw}"
        );
        let derived = calls
            .iter()
            .find(|q| q.contains("store = 'derived'"))
            .expect("derived branch");
        assert!(derived.contains("kind = 'observation'"), "{derived}");
        assert!(
            derived.contains("status IN ['current', 'stale']"),
            "archived/superseded rows must be excluded: {derived}"
        );
        assert!(derived.contains("status = 'stale'"), "{derived}");
        assert!(
            derived.contains("details.generated_by = 'fallback'"),
            "{derived}"
        );
        assert!(derived.contains("embedding = NONE"), "{derived}");
    }

    /// One simulated `memory` row for the discovery predicates.
    struct BacklogRow {
        project: &'static str,
        kind: &'static str,
        status: &'static str,
        store: Option<&'static str>,
        generated_by: Option<&'static str>,
        has_embedding: bool,
    }

    /// Predicate-aware fake for `pending_observation_projects`: evaluates the
    /// same bounds as the SQL against simulated rows, so the
    /// archived/superseded exclusion is exercised as behavior and not only
    /// asserted as query text.
    struct BacklogDb(Vec<BacklogRow>);

    #[async_trait]
    impl Executor for BacklogDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
            if !sql.contains("GROUP BY project") {
                return Ok(QueryOutcome::Single(Value::None));
            }
            let derived = sql.contains("store = 'derived'");
            let mut projects: Vec<Value> = Vec::new();
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for row in &self.0 {
                let pending = if derived {
                    row.kind == "observation"
                        && row.store == Some("derived")
                        && matches!(row.status, "current" | "stale")
                        && (row.status == "stale"
                            || row.generated_by == Some("fallback")
                            || !row.has_embedding)
                } else {
                    row.kind == "observation"
                        && row.status == "current"
                        && row.store != Some("derived")
                };
                if pending && seen.insert(row.project) {
                    projects.push(obj(vec![(
                        "project",
                        Value::RecordId(RecordId::new("repo", row.project)),
                    )]));
                }
            }
            Ok(QueryOutcome::Single(arr(projects)))
        }

        async fn smart_query(
            &self,
            _sql: &str,
            _vars: Vars,
        ) -> Result<QueryOutcome, ibis_db::DbError> {
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    /// Review fix: archived/superseded derived rows with a fallback summary or
    /// no embedding must not keep their project selected forever; current and
    /// stale derived rows still select theirs.
    #[tokio::test]
    async fn pending_projects_excludes_archived_and_superseded_derived() {
        let db = Arc::new(BacklogDb(vec![
            BacklogRow {
                project: "archived_repo",
                kind: "observation",
                status: "archived",
                store: Some("derived"),
                generated_by: Some("fallback"),
                has_embedding: false,
            },
            BacklogRow {
                project: "superseded_repo",
                kind: "observation",
                status: "superseded",
                store: Some("derived"),
                generated_by: None,
                has_embedding: false,
            },
            // Archived raw rows stay excluded by the unchanged raw branch.
            BacklogRow {
                project: "archived_raw_repo",
                kind: "observation",
                status: "archived",
                store: None,
                generated_by: None,
                has_embedding: true,
            },
            BacklogRow {
                project: "stale_repo",
                kind: "observation",
                status: "stale",
                store: Some("derived"),
                generated_by: None,
                has_embedding: true,
            },
            BacklogRow {
                project: "fallback_repo",
                kind: "observation",
                status: "current",
                store: Some("derived"),
                generated_by: Some("fallback"),
                has_embedding: true,
            },
            BacklogRow {
                project: "noembed_repo",
                kind: "observation",
                status: "current",
                store: Some("derived"),
                generated_by: None,
                has_embedding: false,
            },
            BacklogRow {
                project: "raw_repo",
                kind: "observation",
                status: "current",
                store: None,
                generated_by: None,
                has_embedding: true,
            },
        ]));
        let opt = Optimizer::new(Some(db as Arc<dyn Executor>), None, None);

        let projects = opt
            .pending_observation_projects()
            .await
            .expect("pending projects");
        assert_eq!(
            projects,
            vec![
                "raw_repo".to_string(),
                "stale_repo".to_string(),
                "fallback_repo".to_string(),
                "noembed_repo".to_string(),
            ],
            "archived/superseded derived rows must not select their project"
        );
    }

    /// D11 contract: the shadow measurement is tier-S local-only.
    #[test]
    fn decide_shadow_route_hints_are_tier_s() {
        assert_eq!(DECIDE_SHADOW_ROUTE_HINT, "local_only:S");
        assert_eq!(IS_NOISE_QUESTION_ID, "is_noise");
        assert_eq!(IS_NOISE_RUBRIC_VERSION, "is_noise.v1");
    }

    /// D11 rubric: closed two-option question with a versioned instruction.
    #[test]
    fn is_noise_question_is_closed_and_versioned() {
        let question = is_noise_question();
        assert_eq!(question.id, IS_NOISE_QUESTION_ID);
        assert!(question.instructions.contains(IS_NOISE_RUBRIC_VERSION));
        match question.kind {
            ibis_ai::decide::DecideKind::Choice(options) => {
                let ids: Vec<&str> = options.iter().map(|(id, _)| id.as_str()).collect();
                assert_eq!(ids, vec!["noise", "durable"]);
            }
            other => panic!("expected a choice question, got {other:?}"),
        }
    }

    /// D11 state builder: observation + duplicate window, explicit empty case.
    #[test]
    fn is_noise_state_matches_the_duplicate_window() {
        let empty = build_is_noise_state("Ran the tests again.", &[]);
        assert!(empty.starts_with("Observation:\nRan the tests again."));
        assert!(empty.ends_with("Existing memories:\n(none)"));

        let duplicates = [DuplicateMemory {
            id: "memory:fix1".into(),
            kind: "fix".into(),
            title: "Setup crash".into(),
            content: "Setup crashes on empty root.".into(),
            score: 0.96,
        }];
        let state = build_is_noise_state("Empty roots crash setup.", &duplicates);
        assert!(state.contains("- id: memory:fix1"));
        assert!(state.contains("kind: fix"));
        assert!(state.contains("title: Setup crash"));
        assert!(state.contains("content: Setup crashes on empty root."));
    }

    /// D11 agreement: noise vs any durable action.
    #[test]
    fn shadow_agreement_maps_noise_to_noise() {
        let answer = DecideAnswer {
            probs: [("durable".to_string(), 0.9), ("noise".to_string(), 0.1)]
                .into_iter()
                .collect(),
            best: "durable".into(),
            confidence: 0.8,
        };
        let mut decision = ConsolidationDecision {
            action: "create".into(),
            ..Default::default()
        };
        let record = decide_shadow_record("memory:obs1", &decision, &answer);
        assert!(record.agreement);
        assert_eq!(record.rubric, IS_NOISE_RUBRIC_VERSION);

        decision.action = "noise".into();
        let record = decide_shadow_record("memory:obs1", &decision, &answer);
        assert!(!record.agreement);
    }

    /// D11 shadow mode: the generative decision is unchanged, the extra
    /// `decide()` call is logged on tier S, and the same fix is persisted.
    #[tokio::test]
    async fn shadow_mode_measures_decide_without_changing_the_decision() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                (
                    "content",
                    Value::from_t("The wizard crashes when discovery_root is empty."),
                ),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("agent")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{
                "action":"create",
                "kind":"fix",
                "title":"Guard empty discovery root",
                "content":"The setup wizard must guard an empty discovery_root before scanning.",
                "confidence":0.88,
                "details":{"root_cause":"empty discovery_root"}
            }"#,
            r#"{"is_noise":{"probs":{"noise":0.1,"durable":0.9}}}"#,
        ]));
        let mut opt = optimizer(db.clone(), ai.clone());
        opt.decide_shadow = true;

        opt.consolidate_loop("ibis", 5)
            .await
            .expect("ConsolidateLoop failed");
        let hints = ai.route_hints.lock().unwrap().clone();
        assert_eq!(
            hints,
            vec!["local:M".to_string(), "local_only:S".to_string()],
            "distill then local-only shadow decide: {hints:?}"
        );
        let prompts = ai.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2, "one distill + one shadow decide");
        assert!(prompts[1].contains("\n\nSTATE:\n"));
        assert!(prompts[1].contains("Existing memories:\n(none)"));

        // Behavior unchanged: the generative verdict still persists the fix.
        let add_vars = db.vars_of("CREATE memory:");
        assert_eq!(add_vars.len(), 1, "one memory written");
        assert_eq!(
            add_vars[0].get("kind").map(var_str).unwrap_or_default(),
            "fix"
        );
    }

    /// D11 shadow mode: the structured `decide_shadow` log line carries the
    /// agreement data the activation phase needs.
    #[tokio::test]
    async fn shadow_mode_emits_the_decide_shadow_log() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs_log")),
                ("content", Value::from_t("Some durable observation.")),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("agent")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"create","kind":"fact","title":"t","content":"c","confidence":0.8}"#,
            r#"{"is_noise":{"probs":{"noise":0.2,"durable":0.8}}}"#,
        ]));
        let mut opt = optimizer(db, ai);
        opt.decide_shadow = true;

        let events = install_decide_shadow_capture();
        events.lock().unwrap().clear();
        opt.consolidate_loop("ibis", 5)
            .await
            .expect("ConsolidateLoop failed");

        let captured = events.lock().unwrap();
        let matching: Vec<&CapturedEvent> = captured
            .iter()
            .filter(|event| {
                event
                    .fields
                    .iter()
                    .any(|(name, value)| name == "observation" && value == "memory:obs_log")
            })
            .collect();
        assert_eq!(matching.len(), 1, "one decide_shadow event");
        let fields: std::collections::BTreeMap<&str, &str> = matching[0]
            .fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(fields.get("decision_llm"), Some(&"create"));
        assert_eq!(fields.get("best"), Some(&"durable"));
        assert_eq!(fields.get("agreement"), Some(&"true"));
        assert_eq!(fields.get("rubric"), Some(&IS_NOISE_RUBRIC_VERSION));
        assert!(fields.contains_key("probs"));
        assert!(fields.contains_key("confidence"));
    }

    /// D11 shadow mode: a failing decide call never changes the decision and
    /// disables the measurement for the rest of the pass (one warn, no more
    /// tier-S calls).
    #[tokio::test]
    async fn shadow_mode_failure_keeps_the_generative_path() {
        let db = Arc::new(ConsolidateDb::new(
            vec![
                obj(vec![
                    ("id", Value::from_t("memory:obs1")),
                    ("content", Value::from_t("First durable observation.")),
                    ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                    ("owner", Value::from_t("agent")),
                    ("visibility", Value::from_t("team")),
                ]),
                obj(vec![
                    ("id", Value::from_t("memory:obs2")),
                    ("content", Value::from_t("Second durable observation.")),
                    ("embedding", Value::from_t(vec![0.3f64, 0.4])),
                    ("owner", Value::from_t("agent")),
                    ("visibility", Value::from_t("team")),
                ]),
            ],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"create","kind":"fact","title":"one","content":"one","confidence":0.8}"#,
            "not json",
            r#"{"action":"create","kind":"fact","title":"two","content":"two","confidence":0.8}"#,
        ]));
        let mut opt = optimizer(db.clone(), ai.clone());
        opt.decide_shadow = true;

        opt.consolidate_loop("ibis", 5)
            .await
            .expect("shadow failure stays contained");
        let hints = ai.route_hints.lock().unwrap().clone();
        assert_eq!(
            hints,
            vec![
                "local:M".to_string(),
                "local_only:S".to_string(),
                "local:M".to_string()
            ],
            "shadow is skipped after the first failure: {hints:?}"
        );
        assert_eq!(
            db.vars_of("CREATE memory:").len(),
            2,
            "both generative decisions persisted"
        );
    }

    /// D11 shadow mode: personal observations never leave the machine.
    #[tokio::test]
    async fn shadow_mode_routes_personal_content_local_only() {
        let db = Arc::new(ConsolidateDb::new(Vec::new(), Vec::new()));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"is_noise":{"probs":{"noise":0.2,"durable":0.8}}}"#,
        ]));
        let opt = optimizer(db, ai.clone());
        let observation = ConsolidationObservation {
            id: "memory:obs1".into(),
            content: "A personal note.".into(),
            owner: "agent".into(),
            visibility: "personal".into(),
            ..Default::default()
        };
        let decision = ConsolidationDecision {
            action: "create".into(),
            ..Default::default()
        };
        let shadow = opt
            .shadow_decide(&observation, &[], &decision)
            .await
            .expect("shadow decide");
        assert_eq!(
            ai.route_hints.lock().unwrap().clone(),
            vec!["local_only:S".to_string()]
        );
        assert!(shadow.agreement);
    }

    /// C7 strict mode: the distillation retries ONCE when the per-kind
    /// required details keys are missing, then persists the grounded details.
    #[tokio::test]
    async fn strict_details_retry_once_and_persist_grounded_details() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                (
                    "content",
                    Value::from_t("The wizard crashes when discovery_root is empty."),
                ),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("agent")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"create","kind":"fix","title":"t","content":"c","confidence":0.8}"#,
            r#"{"action":"create","kind":"fix","title":"t","content":"c","confidence":0.8,"details":{"root_cause":"empty discovery root"}}"#,
        ]));
        let mut opt = optimizer(db.clone(), ai.clone());
        opt.schema_enforcement = ibis_search::memory::details::SchemaEnforcement::Strict;
        opt.consolidate_loop("ibis", 5)
            .await
            .expect("retry succeeds");
        assert_eq!(
            ai.prompts.lock().unwrap().len(),
            2,
            "strict mode retries the distillation exactly once"
        );
        let add_vars = db.vars_of("CREATE memory:");
        assert_eq!(add_vars.len(), 1, "one add_memory transaction");
        assert!(
            add_vars[0]
                .values()
                .any(|v| matches!(v, Value::String(s) if s == "empty discovery root")),
            "details bind on the write: {:?}",
            add_vars[0]
        );
        let calls = db.sql_calls();
        assert!(
            calls.join("\n").contains("details.root_cause = $det_0"),
            "details ride dotted fields: {calls:?}"
        );
    }

    /// C7 strict mode: a second missing-details verdict gives up (item-level
    /// failure, the loop stays healthy) and no memory is written.
    #[tokio::test]
    async fn strict_details_gives_up_after_one_retry() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("content", Value::from_t("Some observation body.")),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("agent")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"create","kind":"fix","title":"t","content":"c","confidence":0.8}"#,
            r#"{"action":"create","kind":"fix","title":"t","content":"c","confidence":0.8}"#,
        ]));
        let mut opt = optimizer(db.clone(), ai.clone());
        opt.schema_enforcement = ibis_search::memory::details::SchemaEnforcement::Strict;
        opt.consolidate_loop("ibis", 5)
            .await
            .expect("item-level failure stays contained");
        assert_eq!(ai.prompts.lock().unwrap().len(), 2, "no third attempt");
        assert!(
            db.vars_of("CREATE memory:").is_empty(),
            "no memory written after the failed retry"
        );
    }

    /// Scenario: TestConsolidateLoopMergesWithCurrentDuplicate.
    #[tokio::test]
    async fn consolidate_loop_merges_with_current_duplicate() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("content", Value::from_t("Empty roots crash setup.")),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
            ])],
            vec![obj(vec![
                ("id", Value::from_t("memory:fix1")),
                ("kind", Value::from_t("fix")),
                ("title", Value::from_t("Setup crash")),
                ("content", Value::from_t("Setup crashes on empty root.")),
                ("score", Value::from_t(0.96f64)),
            ])],
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{
                "action":"merge",
                "kind":"fix",
                "title":"Guard empty setup roots",
                "content":"Setup must validate that discovery_root is non-empty before scanning.",
                "confidence":0.94,
                "merge_with":"memory:fix1"
            }"#,
        ]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 10)
            .await
            .expect("ConsolidateLoop failed");
        let prompts = ai.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1, "one distillation prompt");
        assert!(
            prompts[0].contains("memory:fix1"),
            "expected duplicate candidate in distillation prompt: {}",
            prompts[0]
        );
        let calls = db.sql_calls();
        assert_calls_contain(
            &calls,
            &[
                // add_memory supersedes the merged duplicate.
                "RELATE $mem->supersedes->memory:fix1",
                // persist transaction also supersedes the source observation.
                "RELATE memory:",
                "->supersedes->memory:obs1",
                "UPDATE memory:obs1 SET status = 'archived'",
            ],
        );
    }

    /// C9: candidates below `dup_similarity_min` never reach the prompt; the
    /// config knob retains them when lowered. Zero-candidate observations
    /// still distill, with `POSSIBLE DUPLICATES: []`.
    #[tokio::test]
    async fn find_duplicate_memories_applies_threshold() {
        let make_rows = || {
            (
                vec![obj(vec![
                    ("id", Value::from_t("memory:obs1")),
                    ("content", Value::from_t("Wizard discovery quirks noted.")),
                    ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ])],
                vec![obj(vec![
                    ("id", Value::from_t("memory:fix1")),
                    ("kind", Value::from_t("fix")),
                    ("title", Value::from_t("Setup crash")),
                    ("content", Value::from_t("Setup crashes on empty root.")),
                    ("score", Value::from_t(0.5f64)),
                ])],
            )
        };

        // Default floor 0.80: the 0.5-scoring candidate is dropped; the
        // distillation still runs, with an empty candidate list.
        let (observations, duplicates) = make_rows();
        let db = Arc::new(ConsolidateDb::new(observations, duplicates));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"create","kind":"fix","title":"Guard empty roots","content":"Validate discovery_root before scanning.","confidence":0.9}"#,
        ]));
        let opt = optimizer(db.clone(), ai.clone());
        opt.consolidate_loop("repo", 10).await.expect("loop failed");
        let prompts = ai.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 1, "distillation still runs");
        assert!(
            prompts[0].contains("POSSIBLE DUPLICATES:\n[]"),
            "below-threshold candidates must be dropped: {}",
            prompts[0]
        );
        let calls = db.sql_calls();
        assert_calls_missing(&calls, "supersedes->memory:fix1");

        // The config knob (`Optimizer.dup_similarity_min`) retains the
        // candidate when lowered to the candidate's score.
        let (observations, duplicates) = make_rows();
        let db = Arc::new(ConsolidateDb::new(observations, duplicates));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"merge","kind":"fix","title":"Guard empty setup roots","content":"Setup must validate that discovery_root is non-empty before scanning.","confidence":0.94,"merge_with":"memory:fix1"}"#,
        ]));
        let mut opt = optimizer(db.clone(), ai.clone());
        opt.dup_similarity_min = 0.5;
        opt.consolidate_loop("repo", 10).await.expect("loop failed");
        let prompts = ai.prompts.lock().unwrap().clone();
        assert!(
            prompts[0].contains("memory:fix1"),
            "candidate at the configured floor must be kept: {}",
            prompts[0]
        );
        assert_calls_contain(&db.sql_calls(), &["RELATE $mem->supersedes->memory:fix1"]);
    }

    /// C9: an observation whose normalized title equals a candidate's merges
    /// deterministically — zero AI calls, same persist flow as an LLM merge.
    #[tokio::test]
    async fn deterministic_title_match_skips_llm() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("title", Value::from_t("Setup  Crash")),
                (
                    "content",
                    Value::from_t("Setup crashes when roots are empty."),
                ),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("confidence", Value::from_t(0.5f64)),
            ])],
            vec![obj(vec![
                ("id", Value::from_t("memory:fix1")),
                ("kind", Value::from_t("fix")),
                ("title", Value::from_t("setup/crash")),
                ("content", Value::from_t("Setup crashes on empty root.")),
                ("score", Value::from_t(0.9f64)),
            ])],
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 10)
            .await
            .expect("ConsolidateLoop failed");
        assert!(
            ai.prompts.lock().unwrap().is_empty() && ai.route_hints.lock().unwrap().is_empty(),
            "a deterministic title match must not call the AI"
        );
        let calls = db.sql_calls();
        assert_calls_contain(
            &calls,
            &[
                "RELATE $mem->supersedes->memory:fix1",
                "UPDATE memory:obs1 SET status = 'archived'",
            ],
        );
        let add_vars = db.vars_of("CREATE memory:");
        assert_eq!(add_vars.len(), 1, "one add_memory transaction: {calls:?}");
        assert_eq!(
            add_vars[0].get("kind").map(var_str).unwrap_or_default(),
            "fix",
            "deterministic merge keeps the candidate's kind"
        );
        assert_eq!(
            add_vars[0].get("content").map(var_str).unwrap_or_default(),
            "Setup crashes on empty root.",
            "deterministic merge keeps the candidate's durable content"
        );
    }

    /// C9: untitled observations fall back to the first content line (the
    /// claim-store first_line convention) for the deterministic pre-match.
    #[tokio::test]
    async fn deterministic_title_match_falls_back_to_first_line() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("content", Value::from_t("Wizard discovery quirks.")),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
            ])],
            vec![obj(vec![
                ("id", Value::from_t("memory:fact1")),
                ("kind", Value::from_t("fact")),
                ("title", Value::from_t("WIZARD\\DISCOVERY  QUIRKS")),
                ("content", Value::from_t("Discovery quirks are known.")),
                ("score", Value::from_t(0.85f64)),
            ])],
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 10)
            .await
            .expect("ConsolidateLoop failed");
        assert!(
            ai.prompts.lock().unwrap().is_empty(),
            "the first-line fallback must still match deterministically"
        );
        let add_vars = db.vars_of("CREATE memory:");
        assert_eq!(add_vars.len(), 1, "one add_memory transaction");
        assert_eq!(
            add_vars[0].get("title").map(var_str).unwrap_or_default(),
            "WIZARD\\DISCOVERY  QUIRKS",
            "the candidate title is preserved verbatim"
        );
    }

    /// C9: title normalization — lowercase, trim, path separators become
    /// spaces, whitespace collapsed, trailing sentence punctuation stripped.
    #[test]
    fn normalize_title_collapses_separators_and_whitespace() {
        assert_eq!(
            normalize_title("  Path\\to/Thing   weird "),
            "path to thing weird"
        );
        assert_eq!(normalize_title("Setup crashes."), "setup crashes");
        assert_eq!(normalize_title(""), "");
        assert_eq!(normalize_title("   "), "");
        assert_eq!(normalize_title("..."), "");
    }

    /// Scenario: TestConsolidateLoopDecaysNoise.
    #[tokio::test]
    async fn consolidate_loop_decays_noise() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("content", Value::from_t("Command completed.")),
                ("confidence", Value::from_t(0.5f64)),
                ("embedding", Value::from_t(vec![0.1f64])),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![
            r#"{"action":"noise","reason":"routine tool output"}"#,
        ]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 10)
            .await
            .expect("ConsolidateLoop failed");
        let calls = db.sql_calls();
        assert_calls_contain(
            &calls,
            &[
                "UPDATE memory:obs1 SET status = 'stale'",
                "confidence = 0.250000",
                "valid_until = time::now() + 7d",
            ],
        );
    }

    /// Scenario:
    /// `TestConsolidateLoopSkipsForeignPersonalDuplicatesAndKeepsPersonalLocalOnly`.
    #[tokio::test]
    async fn consolidate_loop_skips_foreign_personal_duplicates_and_keeps_personal_local_only() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs-personal")),
                (
                    "content",
                    Value::from_t("My private note about credentials."),
                ),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("alice")),
                ("visibility", Value::from_t("personal")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![r#"{"action":"noise"}"#]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 5)
            .await
            .expect("ConsolidateLoop failed");
        let hints = ai.route_hints.lock().unwrap().clone();
        assert_eq!(
            hints,
            vec!["local_only:M".to_string()],
            "personal observations must use local_only:M, got {hints:?}"
        );
        let calls = db.sql_calls();
        let joined = calls.join("\n");
        let vars = db.vars_of("kind != 'observation'");
        assert_eq!(vars.len(), 1, "one duplicate lookup: {calls:?}");
        // The owner binds (F-002): value check replaces the `owner = 'alice'
        // interpolation fragment.
        assert!(
            joined.contains("visibility = 'personal'"),
            "personal observation duplicate lookup must be same-owner personal only:\n{joined}"
        );
        assert_eq!(
            vars[0].get("dup_owner").map(var_str).unwrap_or_default(),
            "alice",
            "owner binding: {:?}",
            vars[0].get("dup_owner")
        );
        assert!(
            !joined.contains("visibility = 'team' OR") && !joined.contains("(visibility = 'team'"),
            "personal observation must not pull team candidates:\n{joined}"
        );
    }

    /// Scenario: TestConsolidateLoopTeamObservationDuplicatesAreTeamOnly.
    #[tokio::test]
    async fn consolidate_loop_team_observation_duplicates_are_team_only() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs-team")),
                ("content", Value::from_t("Shared finding.")),
                ("embedding", Value::from_t(vec![0.1f64, 0.2])),
                ("owner", Value::from_t("alice")),
                ("visibility", Value::from_t("team")),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::new(vec![r#"{"action":"noise"}"#]));
        let opt = optimizer(db.clone(), ai.clone());

        opt.consolidate_loop("repo", 5)
            .await
            .expect("ConsolidateLoop failed");
        let hints = ai.route_hints.lock().unwrap().clone();
        assert_eq!(
            hints,
            vec!["local:M".to_string()],
            "team observations keep local:M, got {hints:?}"
        );
        let calls = db.sql_calls();
        assert_calls_contain(&calls, &["visibility = 'team'"]);
        assert_calls_missing(&calls, "visibility = 'personal'");
    }

    /// Scenario: TestConsolidateLoopLeavesObservationOnAIFailure.
    #[tokio::test]
    async fn consolidate_loop_leaves_observation_on_ai_failure() {
        let db = Arc::new(ConsolidateDb::new(
            vec![obj(vec![
                ("id", Value::from_t("memory:obs1")),
                ("content", Value::from_t("Keep this raw observation.")),
                ("embedding", Value::from_t(vec![0.1f64])),
            ])],
            Vec::new(),
        ));
        let ai = Arc::new(ConsolidateAi::failing());
        let opt = optimizer(db.clone(), ai);

        opt.consolidate_loop("repo", 10)
            .await
            .expect("an item-level AI failure should not fail the loop");
        let calls = db.sql_calls();
        for query in &calls {
            assert!(
                !query.contains("UPDATE memory:obs1") && !query.contains("CREATE memory:"),
                "failed classification must leave the observation untouched: {query}"
            );
        }
    }

    /// F4 §B.5 — project discovery for the daemon-owned loop: distinct
    /// `repo` records still holding current observations; junk/none rows and
    /// non-repo shapes are skipped.
    #[tokio::test]
    async fn pending_observation_projects_lists_distinct_repos() {
        struct ProjectsDb(Vec<Value>);
        #[async_trait]
        impl Executor for ProjectsDb {
            async fn execute(&self, _sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
                Ok(QueryOutcome::Single(arr(self.0.clone())))
            }
            async fn smart_query(
                &self,
                _sql: &str,
                _vars: Vars,
            ) -> Result<QueryOutcome, ibis_db::DbError> {
                Ok(QueryOutcome::Single(Value::None))
            }
        }
        let db = Arc::new(ProjectsDb(vec![
            obj(vec![(
                "project",
                Value::RecordId(RecordId::new("repo", "alpha")),
            )]),
            obj(vec![(
                "project",
                Value::RecordId(RecordId::new("repo", "beta")),
            )]),
            obj(vec![("project", Value::None)]),
            obj(vec![(
                "project",
                Value::RecordId(RecordId::new("issue", "42")),
            )]),
        ]));
        let opt = Optimizer::new(Some(db as Arc<dyn Executor>), None, None);

        let projects = opt
            .pending_observation_projects()
            .await
            .expect("project discovery");
        assert_eq!(projects, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// The discovery surfaces store failures instead of silently returning
    /// an empty list (the daemon loop logs and retries on the next tick).
    #[tokio::test]
    async fn pending_observation_projects_propagates_db_errors() {
        struct FailingDb;
        #[async_trait]
        impl Executor for FailingDb {
            async fn execute(&self, _sql: &str) -> Result<QueryOutcome, ibis_db::DbError> {
                Err(ibis_db::DbError::query(
                    "connection lost",
                    "SELECT",
                    std::time::Duration::from_millis(1),
                ))
            }
            async fn smart_query(
                &self,
                _sql: &str,
                _vars: Vars,
            ) -> Result<QueryOutcome, ibis_db::DbError> {
                Err(ibis_db::DbError::NotInitialized)
            }
        }
        let opt = Optimizer::new(Some(Arc::new(FailingDb) as Arc<dyn Executor>), None, None);
        assert!(opt.pending_observation_projects().await.is_err());
    }

    /// `parseConsolidationDecision behavior: fence trimming, action/kind
    /// validation, confidence clamping and valid_until validation.
    #[test]
    fn parse_decision_matrix() {
        let fenced = parse_consolidation_decision(
            "```json\n{\"action\":\"create\",\"kind\":\"FIX\",\"content\":\"c\",\"confidence\":2.5}\n```",
        )
        .expect("fenced");
        assert_eq!(fenced.action, "create");
        assert_eq!(fenced.kind, "fix");
        assert_eq!(fenced.confidence, 0.6, "out-of-range confidence clamps");

        let noise = parse_consolidation_decision("{\"action\":\"noise\"}").expect("noise");
        assert_eq!(noise.action, "noise");

        assert!(
            parse_consolidation_decision("{\"action\":\"destroy\"}").is_err(),
            "unknown action rejected"
        );
        assert!(
            parse_consolidation_decision(
                "{\"action\":\"create\",\"kind\":\"secret\",\"content\":\"c\"}"
            )
            .is_err(),
            "unknown kind rejected"
        );
        assert!(
            parse_consolidation_decision("{\"action\":\"create\",\"kind\":\"fix\"}").is_err(),
            "empty content rejected"
        );
        assert!(
            parse_consolidation_decision(
                "{\"action\":\"create\",\"kind\":\"fix\",\"content\":\"c\",\"valid_until\":\"yesterday\"}"
            )
            .is_err(),
            "invalid valid_until rejected"
        );

        // C7: details ride through the parser; absence stays null.
        let with_details = parse_consolidation_decision(
            "{\"action\":\"create\",\"kind\":\"decision\",\"content\":\"c\",\"details\":{\"scope\":\"billing\",\"alternatives\":[\"cache\"]}}",
        )
        .expect("details parse");
        assert_eq!(
            with_details.details,
            serde_json::json!({"scope": "billing", "alternatives": ["cache"]})
        );
        let without_details = parse_consolidation_decision(
            "{\"action\":\"create\",\"kind\":\"decision\",\"content\":\"c\"}",
        )
        .expect("no details");
        assert!(without_details.details.is_null());
    }
}

/// D13 prompt stability: two invocations with different contexts must share
/// the byte-identical instruction prefix, and the return-schema instruction
/// stays the final line of the prompt.
#[cfg(test)]
mod prompt_stability_tests {
    use super::*;

    #[test]
    fn distill_prompt_prefix_is_byte_stable_and_schema_last() {
        let a = build_distill_prompt(
            "obs A",
            r#"[{"id":"memory:mem_a","kind":"fact","title":"t","content":"c","score":0.9}]"#,
        );
        let b = build_distill_prompt("obs B — entirely different and much longer", "[]");
        assert_eq!(&a[..DISTILL_INSTRUCTIONS.len()], DISTILL_INSTRUCTIONS);
        assert_eq!(&b[..DISTILL_INSTRUCTIONS.len()], DISTILL_INSTRUCTIONS);
        assert_ne!(a, b);
        // The return-schema instruction is the trailing (query-adjacent) part.
        assert!(a.ends_with(r#""merge_with":"","valid_until":"","details":{}}"#));
        assert!(b.ends_with(r#""merge_with":"","valid_until":"","details":{}}"#));
    }
}
