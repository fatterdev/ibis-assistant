//! `ibis eval observations` — deterministic C8 cluster evaluation over a
//! live engine (ddofds fase 5, riuso del harness A1).
//!
//! One estate per dataset case is seeded in a scratch namespace, reconciled
//! in `Full` mode for every scope, and compared against the expected exact
//! memberships + pinned `signature_hash` values. No LLM is involved: the llm
//! synthesis pass is covered by unit tests; this gate pins the deterministic
//! core (families, hub defense, split/merge, scope isolation).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use ibis_db::executor::{Executor, Vars};
use ibis_db::value::Value;
use ibis_search::memory::observations::{
    observation_scopes_for_project, signature_hash, ObsScope, ObservationSettings, ReconcileMode,
    Signature, OBSERVATION_AGENT, OBSERVATION_FAMILIES, OBSERVATION_TOOL,
};
use ibis_search::rows;
use ibis_search::search::SearchService;

/// Dedicated project key of every case estate.
pub const EVAL_PROJECT: &str = "golden";

// ---------------------------------------------------------------------------
// Dataset
// ---------------------------------------------------------------------------

fn default_version() -> String {
    "1".to_string()
}

fn default_team() -> String {
    "team".to_string()
}

fn default_current() -> String {
    "current".to_string()
}

/// One seeded memory row.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalMemory {
    /// Plain key (`g_…`); the record id is `memory:⟨<key⟩⟩`.
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    /// `details.root_cause` (fix family).
    #[serde(default)]
    pub root_cause: String,
    /// `provenance.source_path` (file family without edges).
    #[serde(default)]
    pub source_path: String,
    /// `derived_from` memory→source_file edges (rel paths of `files`).
    #[serde(default)]
    pub files: Vec<String>,
    /// Commit hash key; seeds the commit row + the memory→commit edge.
    #[serde(default)]
    pub commit: String,
    /// `provider#external_key`; seeds the issue row + commit→issue edge
    /// (requires `commit`).
    #[serde(default)]
    pub issue: String,
    #[serde(default = "default_team")]
    pub visibility: String,
    #[serde(default)]
    pub owner: String,
}

/// One seeded source file (+ optional symbol chunks).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalFile {
    pub rel_path: String,
    #[serde(default)]
    pub symbols: Vec<String>,
}

/// One pre-existing derived row (split/merge scenarios).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalExisting {
    /// Plain key (`old_…`); the record id is `memory:⟨obs_<key⟩⟩`.
    pub key: String,
    pub anchor: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_current")]
    pub status: String,
    #[serde(default = "default_team")]
    pub visibility: String,
    #[serde(default)]
    pub owner: String,
}

/// One expected derived row after the reconcile.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalExpectation {
    /// `family|key` (normalized key).
    pub anchor: String,
    /// Member keys in id order.
    pub members: Vec<String>,
    #[serde(default = "default_current")]
    pub status: String,
    #[serde(default = "default_team")]
    pub visibility: String,
    #[serde(default)]
    pub owner: String,
    /// Pinned `signature_hash`; the runner recomputes it from the anchor and
    /// fails the case on drift (the dataset must be regenerated on a
    /// hash-domain change).
    pub signature_hash: String,
}

/// One expected `supersedes` edge (`new -> old`, both anchors).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalSupersede {
    pub new: String,
    pub old: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalCase {
    pub id: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub memories: Vec<ObservationEvalMemory>,
    #[serde(default)]
    pub files: Vec<ObservationEvalFile>,
    #[serde(default)]
    pub existing: Vec<ObservationEvalExisting>,
    #[serde(default)]
    pub expected: Vec<ObservationEvalExpectation>,
    #[serde(default)]
    pub supersedes: Vec<ObservationEvalSupersede>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservationEvalDataset {
    #[serde(default = "default_version")]
    pub version: String,
    pub cases: Vec<ObservationEvalCase>,
}

impl ObservationEvalDataset {
    /// Strict validation: unique non-empty case ids, valid anchors, pinned
    /// 32-hex hashes, member keys present in the case estate.
    pub fn validate(&self) -> Result<(), String> {
        if self.cases.is_empty() {
            return Err("dataset has no cases".to_string());
        }
        let mut seen: HashSet<String> = HashSet::new();
        for case in &self.cases {
            let id = case.id.trim();
            if id.is_empty() {
                return Err("case id is empty".to_string());
            }
            if !seen.insert(id.to_string()) {
                return Err(format!("duplicate case id: {id}"));
            }
            let mut known: HashSet<&str> = case
                .memories
                .iter()
                .map(|memory| memory.id.as_str())
                .collect();
            for memory in &case.memories {
                if memory.id.trim().is_empty() || memory.kind.trim().is_empty() {
                    return Err(format!("case {id}: memory id/kind is empty"));
                }
            }
            for existing in &case.existing {
                if existing.key.trim().is_empty() {
                    return Err(format!("case {id}: existing key is empty"));
                }
                parse_anchor(&existing.anchor).ok_or_else(|| {
                    format!("case {id}: invalid existing anchor {}", existing.anchor)
                })?;
                for member in &existing.members {
                    if !known.contains(member.as_str()) {
                        return Err(format!(
                            "case {id}: existing member {member} is not a seeded memory"
                        ));
                    }
                }
                known.insert(existing.key.as_str());
            }
            for expected in &case.expected {
                parse_anchor(&expected.anchor).ok_or_else(|| {
                    format!("case {id}: invalid expected anchor {}", expected.anchor)
                })?;
                if expected.signature_hash.len() != 32
                    || !expected
                        .signature_hash
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                {
                    return Err(format!(
                        "case {id}: signature_hash for {} is not 32 lowercase hex",
                        expected.anchor
                    ));
                }
                if expected.members.is_empty() {
                    return Err(format!(
                        "case {id}: expected {} has no members",
                        expected.anchor
                    ));
                }
                for member in &expected.members {
                    if !known.contains(member.as_str()) {
                        return Err(format!(
                            "case {id}: expected member {member} is not a seeded memory"
                        ));
                    }
                }
            }
            for supersede in &case.supersedes {
                for anchor in [&supersede.new, &supersede.old] {
                    if !case.expected.iter().any(|e| &e.anchor == anchor) {
                        return Err(format!(
                            "case {id}: supersedes anchor {anchor} is not among the expectations"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Reads and validates the YAML dataset.
pub fn load_observations_dataset(path: impl AsRef<Path>) -> Result<ObservationEvalDataset, String> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read dataset {}: {e}", path.display()))?;
    let dataset: ObservationEvalDataset =
        serde_yaml::from_str(&text).map_err(|e| format!("failed to parse dataset: {e}"))?;
    dataset.validate()?;
    Ok(dataset)
}

fn parse_anchor(anchor: &str) -> Option<Signature> {
    let (family, key) = anchor.split_once('|')?;
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

fn scope_for(visibility: &str, owner: &str) -> ObsScope {
    if visibility.trim().eq_ignore_ascii_case("personal") {
        ObsScope::Personal(owner.to_string())
    } else {
        ObsScope::Team
    }
}

/// Wrapped record id for SQL interpolation (`memory:⟨key⟩`).
fn memory_record_id(key: &str) -> String {
    let trimmed = key.trim();
    let bare = trimmed.strip_prefix("memory:").unwrap_or(trimmed);
    let bare = bare
        .trim_start_matches(['?', '⟨', '`'])
        .trim_end_matches(['?', '⟩', '`']);
    ibis_db::ids::format_record_id("memory", bare)
}

/// Plain `memory:key` — the exact form the reconciler persists in
/// `details.members` (Q1 ids decode plain; only the row id is canonicalized
/// at Q7 decode).
fn memory_member_ref(key: &str) -> String {
    let trimmed = key.trim();
    let bare = trimmed.strip_prefix("memory:").unwrap_or(trimmed);
    let bare = bare
        .trim_start_matches(['?', '⟨', '`'])
        .trim_end_matches(['?', '⟩', '`']);
    format!("memory:{bare}")
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservationCaseResult {
    pub id: String,
    pub ok: bool,
    pub expected: usize,
    pub mismatches: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservationEvalReport {
    pub dataset: String,
    pub version: String,
    pub cases: Vec<ObservationCaseResult>,
    pub aggregate: ObservationEvalAggregate,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ObservationEvalAggregate {
    pub cases: usize,
    pub cases_ok: usize,
    pub failures: usize,
    pub skipped: usize,
}

pub fn build_observations_report(
    dataset_path: &str,
    dataset: &ObservationEvalDataset,
    cases: Vec<ObservationCaseResult>,
) -> ObservationEvalReport {
    let cases_ok = cases.iter().filter(|case| case.ok).count();
    ObservationEvalReport {
        dataset: dataset_path.to_string(),
        version: dataset.version.clone(),
        aggregate: ObservationEvalAggregate {
            cases: cases.len(),
            cases_ok,
            failures: cases.len() - cases_ok,
            skipped: 0,
        },
        cases,
    }
}

/// Runs one estate per case against the live engine.
pub struct ObservationEvalRunner {
    service: Arc<SearchService>,
    settings: ObservationSettings,
    db: Arc<dyn Executor>,
}

impl ObservationEvalRunner {
    pub fn new(db: Arc<dyn Executor>, settings: ObservationSettings) -> Self {
        let service = Arc::new(SearchService::new(Some(db.clone()), None));
        ObservationEvalRunner {
            service,
            settings,
            db,
        }
    }

    pub async fn run(&self, dataset: &ObservationEvalDataset) -> Vec<ObservationCaseResult> {
        let mut results = Vec::with_capacity(dataset.cases.len());
        for case in &dataset.cases {
            results.push(self.run_case(case).await);
        }
        results
    }

    async fn run_case(&self, case: &ObservationEvalCase) -> ObservationCaseResult {
        match self.run_case_inner(case).await {
            Ok(mismatches) => ObservationCaseResult {
                id: case.id.clone(),
                ok: mismatches.is_empty(),
                expected: case.expected.len(),
                mismatches,
            },
            Err(err) => ObservationCaseResult {
                id: case.id.clone(),
                ok: false,
                expected: case.expected.len(),
                mismatches: vec![format!("error: {err}")],
            },
        }
    }

    async fn run_case_inner(&self, case: &ObservationEvalCase) -> Result<Vec<String>, String> {
        self.wipe().await?;
        self.seed_case(case).await?;
        let scopes = observation_scopes_for_project(
            self.db.as_ref(),
            EVAL_PROJECT,
            self.settings.max_scopes_per_project,
        )
        .await
        .map_err(|e| e.to_string())?;
        for scope in &scopes {
            self.service
                .reconcile_observations_with_settings(
                    EVAL_PROJECT,
                    scope,
                    ReconcileMode::Full,
                    &self.settings,
                )
                .await
                .map_err(|e| e.to_string())?;
        }

        let actual = self.derived_rows().await?;
        let mut mismatches: Vec<String> = Vec::new();

        if actual.len() != case.expected.len() {
            mismatches.push(format!(
                "expected {} derived rows, found {}: {:?}",
                case.expected.len(),
                actual.len(),
                actual.values().map(|row| &row.anchor).collect::<Vec<_>>()
            ));
        }

        // anchor → resolved actual id, for the supersedes edges.
        let mut ids: BTreeMap<String, String> = BTreeMap::new();

        for expected in &case.expected {
            let anchor = parse_anchor(&expected.anchor)
                .ok_or_else(|| format!("invalid expected anchor {}", expected.anchor))?;
            let scope = scope_for(&expected.visibility, &expected.owner);
            let computed = signature_hash(
                EVAL_PROJECT,
                &scope.hash_scope(),
                &anchor.family,
                &anchor.key,
            );
            if computed != expected.signature_hash {
                mismatches.push(format!(
                    "dataset drift: {} expects hash {} but signature_hash() yields {computed}",
                    expected.anchor, expected.signature_hash
                ));
                continue;
            }
            // The hash uniquely identifies the row (the same anchor lives in
            // different scopes with different hashes).
            let Some(row) = actual
                .values()
                .find(|row| row.signature_hash == expected.signature_hash)
            else {
                mismatches.push(format!("missing observation for {}", expected.anchor));
                continue;
            };
            ids.insert(expected.anchor.clone(), row.id.clone());
            if row.anchor != expected.anchor {
                mismatches.push(format!(
                    "{}: persisted anchor {} != expected {}",
                    expected.anchor, row.anchor, expected.anchor
                ));
            }
            if row.status != expected.status {
                mismatches.push(format!(
                    "{}: status {} != expected {}",
                    expected.anchor, row.status, expected.status
                ));
            }
            if row.visibility != expected.visibility {
                mismatches.push(format!(
                    "{}: visibility {} != expected {}",
                    expected.anchor, row.visibility, expected.visibility
                ));
            }
            if expected.visibility == "personal" && row.owner != expected.owner {
                mismatches.push(format!(
                    "{}: owner {} != expected {}",
                    expected.anchor, row.owner, expected.owner
                ));
            }
            let expected_members: Vec<String> = expected
                .members
                .iter()
                .map(|member| memory_member_ref(member))
                .collect();
            if row.members != expected_members {
                mismatches.push(format!(
                    "{}: members {:?} != expected {:?}",
                    expected.anchor, row.members, expected_members
                ));
            }
        }

        if !case.supersedes.is_empty() {
            let edges = self.supersedes_edges().await?;
            for supersede in &case.supersedes {
                let (Some(new_id), Some(old_id)) =
                    (ids.get(&supersede.new), ids.get(&supersede.old))
                else {
                    continue; // the missing row already produced a mismatch
                };
                if !edges.contains(&(new_id.clone(), old_id.clone())) {
                    mismatches.push(format!(
                        "missing supersedes edge {} -> {}",
                        supersede.new, supersede.old
                    ));
                }
            }
        }
        Ok(mismatches)
    }

    async fn wipe(&self) -> Result<(), String> {
        for table in [
            "memory",
            "derived_from",
            "has_memory",
            "supersedes",
            "implements",
            "source_file",
            "file_chunk",
            "commit",
            "issue",
            "repo",
        ] {
            self.db
                .execute(&format!("DELETE {table};"))
                .await
                .map_err(|e| format!("wipe {table}: {e}"))?;
        }
        let repo_id = ibis_db::ids::format_record_id("repo", EVAL_PROJECT);
        self.db
            .smart_query(
                &format!("UPSERT {repo_id} SET name = $name;"),
                [("name".to_string(), Value::from_t(EVAL_PROJECT))]
                    .into_iter()
                    .collect(),
            )
            .await
            .map_err(|e| format!("seed repo: {e}"))?;
        Ok(())
    }

    async fn seed_case(&self, case: &ObservationEvalCase) -> Result<(), String> {
        let repo_id = ibis_db::ids::format_record_id("repo", EVAL_PROJECT);

        // Source files + symbol chunks.
        let mut files_by_path: BTreeMap<String, String> = BTreeMap::new();
        for file in &case.files {
            let sf_id = ibis_db::ids::source_file_id(&repo_id, &file.rel_path);
            let mut vars = Vars::new();
            vars.insert("rel".into(), Value::from_t(file.rel_path.as_str()));
            vars.insert("hash".into(), Value::from_t(format!("h_{}", file.rel_path)));
            self.db
                .smart_query(
                    &format!(
                        "CREATE {sf_id} SET path = $rel, rel_path = $rel, hash = $hash, \
                         is_head = true, repo_name = 'golden';"
                    ),
                    vars,
                )
                .await
                .map_err(|e| format!("seed source_file {}: {e}", file.rel_path))?;
            for (index, symbol) in file.symbols.iter().enumerate() {
                let chunk_id = ibis_db::ids::file_chunk_id(&sf_id, index as i64);
                let mut vars = Vars::new();
                vars.insert("symbol".into(), Value::from_t(symbol.as_str()));
                vars.insert(
                    "content".into(),
                    Value::from_t(format!("fn {symbol}() {{ /* body */ }}")),
                );
                self.db
                    .smart_query(
                        &format!(
                            "CREATE {chunk_id} SET file = {sf_id}, content = $content, \
                             hash = $symbol, start_line = 0, symbol_name = $symbol;"
                        ),
                        vars,
                    )
                    .await
                    .map_err(|e| format!("seed file_chunk {symbol}: {e}"))?;
            }
            files_by_path.insert(file.rel_path.clone(), sf_id);
        }

        // Memories + evidence edges.
        let mut commits_seeded: HashSet<String> = HashSet::new();
        let mut issues_seeded: HashSet<String> = HashSet::new();
        let mut implements_seen: HashSet<(String, String)> = HashSet::new();
        for memory in &case.memories {
            let memory_id = memory_record_id(&memory.id);
            let mut sets: Vec<String> = Vec::new();
            let mut vars = Vars::new();
            let push = |sets: &mut Vec<String>, vars: &mut Vars, name: &str, value: Value| {
                let var = format!("seed_{}", name.replace('.', "_"));
                sets.push(format!("{name} = ${var}"));
                vars.insert(var, value);
            };
            push(
                &mut sets,
                &mut vars,
                "kind",
                Value::from_t(memory.kind.as_str()),
            );
            push(
                &mut sets,
                &mut vars,
                "title",
                Value::from_t(memory.title.as_str()),
            );
            push(
                &mut sets,
                &mut vars,
                "content",
                Value::from_t(memory.content.as_str()),
            );
            push(&mut sets, &mut vars, "status", Value::from_t("current"));
            push(&mut sets, &mut vars, "confidence", Value::from_t(0.7f64));
            push(
                &mut sets,
                &mut vars,
                "visibility",
                Value::from_t(memory.visibility.as_str()),
            );
            push(
                &mut sets,
                &mut vars,
                "owner",
                Value::from_t(if memory.owner.trim().is_empty() {
                    "agent"
                } else {
                    memory.owner.as_str()
                }),
            );
            if !memory.root_cause.trim().is_empty() {
                push(
                    &mut sets,
                    &mut vars,
                    "details.root_cause",
                    Value::from_t(memory.root_cause.as_str()),
                );
            }
            if !memory.source_path.trim().is_empty() {
                push(
                    &mut sets,
                    &mut vars,
                    "provenance.source_path",
                    Value::from_t(memory.source_path.as_str()),
                );
            }
            let sql = format!(
                "CREATE {memory_id} SET {}, project = {repo_id};",
                sets.join(", ")
            );
            self.db
                .smart_query(&sql, vars)
                .await
                .map_err(|e| format!("seed memory {}: {e}", memory.id))?;

            for rel_path in &memory.files {
                let Some(sf_id) = files_by_path.get(rel_path) else {
                    return Err(format!(
                        "memory {} links unknown file {rel_path}",
                        memory.id
                    ));
                };
                self.db
                    .execute(&format!("RELATE {memory_id}->derived_from->{sf_id};"))
                    .await
                    .map_err(|e| format!("link memory {rel_path}: {e}"))?;
            }
            if !memory.commit.trim().is_empty() {
                let commit_id = ibis_db::ids::format_record_id("commit", memory.commit.trim());
                if commits_seeded.insert(commit_id.clone()) {
                    let mut vars = Vars::new();
                    vars.insert("hash".into(), Value::from_t(memory.commit.trim()));
                    self.db
                        .smart_query(
                            &format!("CREATE {commit_id} SET hash = $hash, date = time::now();"),
                            vars,
                        )
                        .await
                        .map_err(|e| format!("seed commit {}: {e}", memory.commit))?;
                }
                self.db
                    .execute(&format!("RELATE {memory_id}->derived_from->{commit_id};"))
                    .await
                    .map_err(|e| format!("link memory commit: {e}"))?;
                if !memory.issue.trim().is_empty() {
                    let (provider, external_key) = memory
                        .issue
                        .split_once('#')
                        .ok_or_else(|| format!("issue {} is not provider#key", memory.issue))?;
                    let issue_id = ibis_db::ids::format_record_id(
                        "issue",
                        &ibis_db::ids::sanitize_id(&format!("{provider}_{external_key}")),
                    );
                    if issues_seeded.insert(issue_id.clone()) {
                        let mut vars = Vars::new();
                        vars.insert("provider".into(), Value::from_t(provider));
                        vars.insert("external_key".into(), Value::from_t(external_key));
                        self.db
                            .smart_query(
                                &format!(
                                    "CREATE {issue_id} SET provider = $provider, \
                                     external_key = $external_key;"
                                ),
                                vars,
                            )
                            .await
                            .map_err(|e| format!("seed issue {}: {e}", memory.issue))?;
                    }
                    if implements_seen.insert((commit_id.clone(), issue_id.clone())) {
                        self.db
                            .execute(&format!("RELATE {commit_id}->implements->{issue_id};"))
                            .await
                            .map_err(|e| format!("link commit issue: {e}"))?;
                    }
                }
            }
        }

        // Pre-existing derived rows (split/merge scenarios).
        for existing in &case.existing {
            let anchor = parse_anchor(&existing.anchor)
                .ok_or_else(|| format!("invalid existing anchor {}", existing.anchor))?;
            let scope = scope_for(&existing.visibility, &existing.owner);
            let hash = signature_hash(
                EVAL_PROJECT,
                &scope.hash_scope(),
                &anchor.family,
                &anchor.key,
            );
            let id = ibis_db::ids::format_record_id("memory", &format!("obs_{}", existing.key));
            let members: Vec<Value> = existing
                .members
                .iter()
                .map(|member| Value::from_t(memory_member_ref(member)))
                .collect();
            let mut vars = Vars::new();
            vars.insert("hash".into(), Value::from_t(hash.as_str()));
            vars.insert("anchor".into(), Value::from_t(existing.anchor.as_str()));
            vars.insert("family".into(), Value::from_t(anchor.family.as_str()));
            vars.insert("members".into(), Value::from_t(members));
            vars.insert("status".into(), Value::from_t(existing.status.as_str()));
            vars.insert(
                "title".into(),
                Value::from_t(if existing.title.trim().is_empty() {
                    existing.anchor.as_str()
                } else {
                    existing.title.as_str()
                }),
            );
            vars.insert(
                "content".into(),
                Value::from_t(if existing.content.trim().is_empty() {
                    existing.anchor.as_str()
                } else {
                    existing.content.as_str()
                }),
            );
            vars.insert(
                "visibility".into(),
                Value::from_t(existing.visibility.as_str()),
            );
            vars.insert(
                "owner".into(),
                Value::from_t(if existing.owner.trim().is_empty() {
                    OBSERVATION_AGENT
                } else {
                    existing.owner.as_str()
                }),
            );
            let sql = format!(
                "CREATE {id} SET kind = 'observation', title = $title, content = $content, \
                 status = $status, confidence = 0.75, visibility = $visibility, owner = $owner, \
                 project = {repo_id}, store = 'derived', policy_applied = 'observation.v1', \
                 provenance.agent = '{OBSERVATION_AGENT}', provenance.tool = '{OBSERVATION_TOOL}', \
                 details.signature_hash = $hash, details.anchor_signature = $anchor, \
                 details.signatures = [$anchor], details.members = $members, \
                 details.evidence_hash = 'seed', details.family = $family, \
                 details.generated_by = 'llm';"
            );
            self.db
                .smart_query(&sql, vars)
                .await
                .map_err(|e| format!("seed existing {}: {e}", existing.key))?;
        }
        Ok(())
    }

    async fn derived_rows(&self) -> Result<BTreeMap<String, ActualObservation>, String> {
        let sql = "SELECT id, status, visibility, owner, \
                   details.signature_hash AS signature_hash, \
                   details.anchor_signature AS anchor, details.family AS family, \
                   details.members AS members \
                   FROM memory WHERE kind = 'observation' AND store = 'derived' ORDER BY id;";
        let outcome = self.db.execute(sql).await.map_err(|e| e.to_string())?;
        let rows = rows::normalize_rows(&outcome)
            .ok_or_else(|| "unexpected derived-rows shape".to_string())?;
        let mut actual = BTreeMap::new();
        for row in rows::object_rows(&rows) {
            let id = rows::record_id(rows::field(row, "id"));
            actual.insert(
                id.clone(),
                ActualObservation {
                    id,
                    status: rows::scalar_string(rows::field(row, "status")),
                    visibility: rows::scalar_string(rows::field(row, "visibility")),
                    owner: rows::scalar_string(rows::field(row, "owner")),
                    signature_hash: rows::scalar_string(rows::field(row, "signature_hash")),
                    anchor: rows::scalar_string(rows::field(row, "anchor")),
                    members: rows::string_slice(rows::field(row, "members")),
                },
            );
        }
        Ok(actual)
    }

    async fn supersedes_edges(&self) -> Result<HashSet<(String, String)>, String> {
        let outcome = self
            .db
            .execute("SELECT in, out FROM supersedes;")
            .await
            .map_err(|e| e.to_string())?;
        let rows = rows::normalize_rows(&outcome)
            .ok_or_else(|| "unexpected supersedes shape".to_string())?;
        Ok(rows::object_rows(&rows)
            .iter()
            .map(|row| {
                (
                    rows::record_id(rows::field(row, "in")),
                    rows::record_id(rows::field(row, "out")),
                )
            })
            .collect())
    }
}

struct ActualObservation {
    id: String,
    status: String,
    visibility: String,
    owner: String,
    signature_hash: String,
    anchor: String,
    members: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_validation_rejects_bad_shapes() {
        let memory = ObservationEvalMemory {
            id: "g_a".into(),
            kind: "fix".into(),
            title: "t".into(),
            content: "c".into(),
            root_cause: "rc".into(),
            source_path: String::new(),
            files: vec![],
            commit: String::new(),
            issue: String::new(),
            visibility: "team".into(),
            owner: String::new(),
        };
        let expected = ObservationEvalExpectation {
            anchor: "fix_root_cause|rc".into(),
            members: vec!["g_a".into(), "g_b".into()],
            status: "current".into(),
            visibility: "team".into(),
            owner: String::new(),
            signature_hash: "0".repeat(32),
        };
        let case = ObservationEvalCase {
            id: "c1".into(),
            note: String::new(),
            memories: vec![memory.clone()],
            files: vec![],
            existing: vec![],
            expected: vec![expected.clone()],
            supersedes: vec![],
        };
        let dataset = ObservationEvalDataset {
            version: "1".into(),
            cases: vec![case.clone()],
        };
        assert!(dataset.validate().is_err(), "unknown member g_b");

        let mut fixed = case.clone();
        fixed.expected[0].members = vec!["g_a".into()];
        let dataset = ObservationEvalDataset {
            version: "1".into(),
            cases: vec![fixed.clone()],
        };
        assert!(dataset.validate().is_ok());

        let mut bad_hash = fixed.clone();
        bad_hash.expected[0].signature_hash = "zz".into();
        let dataset = ObservationEvalDataset {
            version: "1".into(),
            cases: vec![bad_hash],
        };
        assert!(dataset.validate().is_err(), "hash must be 32 lowercase hex");

        let mut bad_anchor = fixed;
        bad_anchor.expected[0].anchor = "bogus".into();
        let dataset = ObservationEvalDataset {
            version: "1".into(),
            cases: vec![bad_anchor],
        };
        assert!(dataset.validate().is_err());
    }

    /// The committed dataset parses, validates, and its pinned hashes match
    /// `signature_hash()` (runs without a live engine).
    #[test]
    fn committed_dataset_pins_match_signature_hash() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../evaluations/observations/dataset.yaml");
        let dataset = load_observations_dataset(&path)
            .unwrap_or_else(|e| panic!("observations dataset missing at {}: {e}", path.display()));
        assert!(dataset.cases.len() >= 8, "the L3 gate expects 8 cases");
        for case in &dataset.cases {
            for expected in &case.expected {
                let anchor = parse_anchor(&expected.anchor).expect("anchor");
                let scope = scope_for(&expected.visibility, &expected.owner);
                assert_eq!(
                    signature_hash(
                        EVAL_PROJECT,
                        &scope.hash_scope(),
                        &anchor.family,
                        &anchor.key
                    ),
                    expected.signature_hash,
                    "case {} anchor {}",
                    case.id,
                    expected.anchor
                );
            }
        }
    }
}
