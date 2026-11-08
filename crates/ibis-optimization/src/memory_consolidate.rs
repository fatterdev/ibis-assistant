//! Sleeptime memory consolidation (
//! ).

use std::sync::Arc;

use chrono::DateTime;
use serde::{Deserialize, Serialize};

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
    /// Drains raw observations oldest-first. Uses
    /// `bulk_local` so hybrid routing tries local Ollama before cloud,
    /// distills durable typed memories, merges semantic duplicates, and gives
    /// routine noise a short stale lifetime.
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
            let decision = match deterministic_title_match(observation, &duplicates) {
                Some(decision) => {
                    tracing::info!(
                        "ConsolidateLoop: deterministic duplicate match for {} -> {}",
                        observation.id,
                        decision.merge_with
                    );
                    decision
                }
                None => match self.distill_observation(observation, &duplicates).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            "ConsolidateLoop: distillation failed for {}: {e}",
                            observation.id
                        );
                        continue;
                    }
                },
            };
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
    pub async fn pending_observation_projects(&self) -> Result<Vec<String>, String> {
        let db = self.db.as_ref().ok_or("database not connected")?;
        let query = "SELECT project FROM memory WHERE kind = 'observation' AND status = 'current' GROUP BY project;";
        let outcome = db
            .execute(query)
            .await
            .map_err(|e| format!("select observation projects: {e}"))?;
        let rows = ibis_db::value::decode_row_list::<ProjectRow>(
            outcome.first().unwrap_or(&Value::None),
            query,
        )
        .map_err(|e| format!("decode observation projects: {e}"))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let rid = row.project?;
                let plain = ibis_db::ids::record_id_to_plain(&rid);
                // `repo:<key>` — the key is the sanitized project name
                // (`sanitize_id` output is idempotent, so passing it back
                // into `consolidate_loop` re-derives the same record).
                plain
                    .strip_prefix("repo:")
                    .map(|name| name.trim().to_string())
            })
            .filter(|name| !name.is_empty())
            .collect())
    }

    async fn select_observations(
        &self,
        db: &Arc<dyn Executor>,
        project_name: &str,
        max_items: usize,
    ) -> Result<Vec<ConsolidationObservation>, String> {
        let mut where_parts = vec!["kind = 'observation'", "status = 'current'"]
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
        // never enters a team distill prompt (bulk_local can still fall back
        // to cloud). Owner binds (F-002) — user-controlled value.
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
            "local_only"
        } else {
            "bulk_local"
        };
        let contents = [ibis_ai::Content {
            role: "user".to_string(),
            parts: vec![ibis_ai::Part { text: prompt }],
        }];
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
                    Arc::new(DbCostStore(db.clone())) as Arc<dyn ibis_ai::CostStore>
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
        if decision.action == "merge" && !contains_duplicate_id(duplicates, &decision.merge_with) {
            return Err(format!(
                "merge target {:?} was not a supplied candidate",
                decision.merge_with
            ));
        }
        Ok(decision)
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
"#;

/// Assembles the distill prompt: byte-stable instructions → observation →
/// duplicates → the return-schema instruction last (D13).
fn build_distill_prompt(observation_content: &str, candidates_json: &str) -> String {
    format!(
        "{DISTILL_INSTRUCTIONS}\nOBSERVATION:\n{observation_content}\n\nPOSSIBLE DUPLICATES:\n{candidates_json}\n\nReturn only JSON:\n{{\"action\":\"create|merge|noise\",\"kind\":\"...\",\"title\":\"...\",\"content\":\"...\",\"confidence\":0.0,\"merge_with\":\"\",\"valid_until\":\"\"}}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ibis_ai::types::{Candidate, Content, Part};
    use ibis_ai::AIProvider;
    use ibis_db::executor::{QueryOutcome, Vars};
    use ibis_db::value::Object;
    use ibis_db::value::ToSql as _;
    use ibis_db::Executor;

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
        calls: std::sync::Mutex<Vec<(String, Vars)>>,
    }

    impl ConsolidateDb {
        fn new(observations: Vec<Value>, duplicates: Vec<Value>) -> Self {
            ConsolidateDb {
                observations,
                duplicates,
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
    async fn consolidate_loop_creates_typed_memory_using_bulk_local() {
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
        assert_eq!(
            hints,
            vec!["bulk_local".to_string()],
            "route hints: {hints:?}"
        );
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
            vec!["local_only".to_string()],
            "personal observations must use local_only, got {hints:?}"
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
            vec!["bulk_local".to_string()],
            "team observations keep bulk_local, got {hints:?}"
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
        assert!(a.ends_with(r#""merge_with":"","valid_until":""}"#));
        assert!(b.ends_with(r#""merge_with":"","valid_until":""}"#));
    }
}
