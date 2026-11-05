//! Retrieval evaluation harness (A1, ddofds fase 0).
//!
//! Golden dataset (`evaluations/retrieval/dataset.yaml`) + runner over
//! `ask_project` (all four channels) + self-seeded deterministic estate in a
//! dedicated scratch database. The RAFT kernel
//! [`Optimizer::evaluate_retrieval`](crate::Optimizer::evaluate_retrieval)
//! stays unchanged: this module extends the same idea with rank-tolerant
//! metrics (`mrr`, `recall@5`, `recall@15`), per-channel attribution and a
//! JSON report, as the baseline for A2 (RRF fusion), A3 (reranker) and A4
//! (query expansion).
//!
//! Design pins:
//! - Seed and query embeddings come from the SAME provider instance, so
//!   baselines are comparable only at equal `embedder` (the report carries
//!   it). The offline hash embedder is deterministic and CI-safe.
//! - Expected ids are matched against `SearchResult.id` in the plain
//!   `table:key` form the live SurrealDB returns; dataset ids in the
//!   `table:⟨key⟩` shape are normalized on load (authoring tolerance).
//! - Degraded environments surface as per-case `error` status (excluded from
//!   the means), never as ranking failures.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use ibis_ai::types::{Candidate, Content, GenerationConfig};
use ibis_ai::{AIProvider, AiError};
use ibis_db::executor::{Executor, Vars};
use ibis_db::value::{Embedding, Value};
use ibis_search::{ActorContext, SearchResult, SearchService};

// ---------------------------------------------------------------------------
// dataset
// ---------------------------------------------------------------------------

/// One golden case: a query and the record ids that must rank high.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EvalCase {
    /// Unique case id (`r01_…`).
    pub id: String,
    /// The question fed to `ask_project` verbatim.
    pub query: String,
    /// Record ids expected in the ranked board (`file_chunk:g_*`,
    /// `memory:g_*`, `commit_chunk:g_*`, `reasoning:g_*`). Both the plain
    /// `table:key` and the `table:⟨key⟩` spellings are accepted.
    pub expected_ids: Vec<String>,
    /// Record ids that must NOT appear on the board (personal memories of
    /// other actors, superseded rows, … — remediation plan section 9).
    #[serde(default)]
    pub absent_ids: Vec<String>,
    /// Free-form provenance note (which scenario the case pins).
    #[serde(default)]
    pub note: String,
}

/// The parsed `dataset.yaml`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RetrievalDataset {
    /// Dataset schema version (currently `1`).
    #[serde(default = "default_dataset_version")]
    pub version: String,
    pub cases: Vec<EvalCase>,
}

fn default_dataset_version() -> String {
    "1".to_string()
}

/// `table:⟨key⟩` → `table:key`; plain ids pass through (trimmed).
pub fn normalize_record_id(id: &str) -> String {
    let id = id.trim();
    match id.split_once(':') {
        Some((table, key)) => {
            let key = key.trim().trim_start_matches('⟨').trim_end_matches('⟩');
            format!("{}:{}", table.trim(), key)
        }
        None => id.to_string(),
    }
}

impl RetrievalDataset {
    /// Strict validation: cases present, unique ids, non-empty queries and
    /// expected-id lists. Called by [`load_dataset`]; the runner refuses to
    /// run an invalid dataset.
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
            if case.query.trim().is_empty() {
                return Err(format!("case {id} has an empty query"));
            }
            if case.expected_ids.is_empty() {
                return Err(format!("case {id} has no expected ids"));
            }
            for expected in &case.expected_ids {
                if normalize_record_id(expected).is_empty() {
                    return Err(format!("case {id} has an empty expected id"));
                }
            }
        }
        Ok(())
    }
}

/// Reads and validates the YAML dataset.
pub fn load_dataset(path: impl AsRef<Path>) -> Result<RetrievalDataset, String> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read dataset {}: {e}", path.display()))?;
    let dataset: RetrievalDataset =
        serde_yaml::from_str(&text).map_err(|e| format!("failed to parse dataset: {e}"))?;
    dataset.validate()?;
    Ok(dataset)
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

/// Per-case outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    /// Search ran and the metrics are meaningful.
    Ok,
    /// Search failed (degraded DB/AI): excluded from the aggregate means.
    Error,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseResult {
    pub id: String,
    pub status: CaseStatus,
    /// `1/rank` of the first expected hit (0 when absent from the board).
    pub mrr: f64,
    /// Fraction of expected ids found within the top 5.
    pub recall_at_5: f64,
    /// Fraction of expected ids found within the top 15 (the full board —
    /// `ask_project` truncates at `VECTOR_CHANNEL_LIMIT` = 15).
    pub recall_at_15: f64,
    /// 1-based rank of the first expected hit, when present.
    pub first_rank: Option<usize>,
    /// `source_kind` of the best-ranked expected hit (channel attribution).
    pub top_channel: Option<String>,
    /// Internal fusion channels of the expected hits, best-rank order
    /// (remediation plan section 9: `top_channel='memory'` does not
    /// distinguish the vector, title and content channels).
    pub channels: Vec<String>,
    /// Absent-id violations: ids listed in `absent_ids` that surfaced on the
    /// board anyway (empty = the case's exclusion assertions hold).
    pub absent_leaks: Vec<String>,
    /// Ranks (1-based) of every expected hit found.
    pub hits: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Cases whose best-ranked expected hit came from each channel.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChannelStat {
    pub cases: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EvalAggregate {
    pub cases: usize,
    pub cases_ok: usize,
    pub cases_error: usize,
    /// Mean MRR over the ok cases.
    pub mrr: f64,
    /// Mean recall@5 over the ok cases.
    pub recall_at_5: f64,
    /// Mean recall@15 over the ok cases.
    pub recall_at_15: f64,
    pub by_channel: BTreeMap<String, ChannelStat>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EvalReport {
    /// Dataset path as passed on the CLI.
    pub dataset: String,
    pub version: String,
    /// Embedding provider name (seed and query embedder are the same
    /// instance — baselines are comparable only at equal embedder).
    pub embedder: String,
    /// Channel fusion under test (`additive` | `rrf` — A2 validation).
    pub fusion: String,
    /// RRF k under test (`Some` only for rrf runs — sweep provenance,
    /// remediation plan section 9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrf_k: Option<f64>,
    /// Whether the golden estate was seeded in this run.
    pub seeded: bool,
    pub aggregate: EvalAggregate,
    pub cases: Vec<CaseResult>,
}

impl EvalReport {
    /// `true` when the mean recall@5 misses the gate (`--min-recall-5`).
    pub fn below_threshold(&self, min_recall_5: f64) -> bool {
        self.aggregate.cases_ok > 0 && self.aggregate.recall_at_5 < min_recall_5
    }
}

/// Aggregates per-case results into a report (means over ok cases only).
pub fn build_report(
    dataset: &str,
    version: &str,
    embedder: &str,
    fusion: &str,
    seeded: bool,
    cases: Vec<CaseResult>,
) -> EvalReport {
    let ok: Vec<&CaseResult> = cases
        .iter()
        .filter(|c| c.status == CaseStatus::Ok)
        .collect();
    let n = ok.len() as f64;
    let mut by_channel: BTreeMap<String, ChannelStat> = BTreeMap::new();
    for case in &ok {
        if let Some(channel) = &case.top_channel {
            by_channel.entry(channel.clone()).or_default().cases += 1;
        }
    }
    let mean = |f: &dyn Fn(&CaseResult) -> f64| -> f64 {
        if n == 0.0 {
            0.0
        } else {
            ok.iter().map(|c| f(c)).sum::<f64>() / n
        }
    };
    EvalReport {
        dataset: dataset.to_string(),
        version: version.to_string(),
        embedder: embedder.to_string(),
        fusion: fusion.to_string(),
        rrf_k: None,
        seeded,
        aggregate: EvalAggregate {
            cases: cases.len(),
            cases_ok: ok.len(),
            cases_error: cases.len() - ok.len(),
            mrr: mean(&|c| c.mrr),
            recall_at_5: mean(&|c| c.recall_at_5),
            recall_at_15: mean(&|c| c.recall_at_15),
            by_channel,
        },
        cases,
    }
}

// ---------------------------------------------------------------------------
// runner
// ---------------------------------------------------------------------------

/// Scores the golden dataset cases against the live retrieval stack.
pub struct RetrievalEvalRunner {
    service: Arc<SearchService>,
}

impl RetrievalEvalRunner {
    pub fn new(service: Arc<SearchService>) -> Self {
        RetrievalEvalRunner { service }
    }

    /// Runs one case through `ask_project` (anonymous actor — team
    /// visibility, all four channels).
    pub async fn run_case(&self, case: &EvalCase) -> CaseResult {
        match self
            .service
            .ask_project(&ActorContext::LOCAL, &case.query)
            .await
        {
            Ok(results) => score_case(case, &results),
            Err(e) => CaseResult {
                id: case.id.clone(),
                status: CaseStatus::Error,
                mrr: 0.0,
                recall_at_5: 0.0,
                recall_at_15: 0.0,
                first_rank: None,
                top_channel: None,
                channels: Vec::new(),
                absent_leaks: Vec::new(),
                hits: Vec::new(),
                error: Some(e.to_string()),
            },
        }
    }

    /// Runs every case sequentially and aggregates.
    pub async fn run(&self, dataset: &RetrievalDataset) -> Vec<CaseResult> {
        let mut results = Vec::with_capacity(dataset.cases.len());
        for case in &dataset.cases {
            tracing::debug!("eval: running case {}", case.id);
            results.push(self.run_case(case).await);
        }
        results
    }
}

/// Pure scorer: ranks of the expected ids inside the returned board.
fn score_case(case: &EvalCase, results: &[SearchResult]) -> CaseResult {
    let mut ranks: HashMap<String, (usize, &str, &str)> = HashMap::new();
    for (i, r) in results.iter().enumerate() {
        ranks.insert(
            normalize_record_id(&r.id),
            (i + 1, r.source_kind.as_str(), r.channel.as_str()),
        );
    }
    let expected: Vec<String> = case
        .expected_ids
        .iter()
        .map(|e| normalize_record_id(e))
        .collect();
    let mut first_rank: Option<usize> = None;
    let mut best_rank = usize::MAX;
    let mut top_channel: Option<String> = None;
    let mut hits = Vec::new();
    let mut channels: Vec<String> = Vec::new();
    for exp in &expected {
        if let Some((rank, channel_kind, channel)) = ranks.get(exp.as_str()) {
            hits.push(*rank);
            channels.push(channel.to_string());
            if *rank < best_rank {
                best_rank = *rank;
                first_rank = Some(*rank);
                top_channel = Some((*channel_kind).to_string());
            }
        }
    }
    let in_top = |k: usize| -> f64 {
        let found = expected
            .iter()
            .filter(|e| ranks.get(e.as_str()).is_some_and(|(r, _, _)| *r <= k))
            .count();
        found as f64 / expected.len() as f64
    };
    let absent_leaks: Vec<String> = case
        .absent_ids
        .iter()
        .map(|a| normalize_record_id(a))
        .filter(|a| ranks.contains_key(a))
        .collect();
    CaseResult {
        id: case.id.clone(),
        status: CaseStatus::Ok,
        mrr: first_rank.map_or(0.0, |r| 1.0 / r as f64),
        recall_at_5: in_top(5),
        recall_at_15: in_top(15),
        first_rank,
        top_channel,
        channels,
        absent_leaks,
        hits,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// offline embedder
// ---------------------------------------------------------------------------

/// Deterministic offline embedder (CI-safe, no AI dependency): maps the text
/// bytes onto a fixed 768-dim vector (pattern `H03Ai` in
/// `ibis-search/tests/live_memory_estate.rs`). Quality is vocabulary-overlap
/// level — enough for rank-tolerant metrics, never for semantic judgment.
pub struct OfflineEmbedder;

/// The 768-dim byte-distribution vector behind [`OfflineEmbedder`].
pub fn offline_vec(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    let mut v = vec![0f32; 768];
    if bytes.is_empty() {
        return v;
    }
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = f32::from(bytes[i % bytes.len()]) / 255.0 - 0.5;
    }
    v
}

#[async_trait]
impl AIProvider for OfflineEmbedder {
    fn name(&self) -> &str {
        "offline-hash-768"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
        Ok(offline_vec(text))
    }

    async fn generate_content(
        &self,
        _contents: &[Content],
        _config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        Err(AiError::other("offline embedder does not generate"))
    }
}

// ---------------------------------------------------------------------------
// golden estate
// ---------------------------------------------------------------------------

struct GoldenFile {
    id: &'static str,
    path: &'static str,
    content: &'static str,
    /// `false` seeds the chunk WITHOUT an embedding (lexical-only channel —
    /// remediation plan section 9 case 3/8).
    embed: bool,
}

struct GoldenMemory {
    id: &'static str,
    kind: &'static str,
    title: &'static str,
    content: &'static str,
    /// Visibility (`team` | `personal`); personal rows of another owner must
    /// never surface (section 9 case 6).
    visibility: &'static str,
    /// `current` | `superseded`; superseded rows are excluded by the
    /// currentness filter (section 9 case 7).
    status: &'static str,
    owner: &'static str,
    embed: bool,
}

struct GoldenCommit {
    id: &'static str,
    hash: &'static str,
    content: &'static str,
}

struct GoldenReasoning {
    id: &'static str,
    question: &'static str,
    outcome: &'static str,
}

/// The golden estate: one row per dataset target, plus distractors, mirroring
/// the four retrieval channels. Ids are explicit and stable (`g_*` keys); the
/// dataset expected ids reference exactly these. Contents pin real repo
/// scenarios (R3.3 MCP timeout, PDEATHSIG thread rule, SurrealDB 3.x gotcha)
/// with vocabulary kept distinctive per target.
const GOLDEN_FILES: &[GoldenFile] = &[
    GoldenFile {
        id: "g_fc_price",
        path: "src/billing/price_calculator.rs",
        content: "The PriceCalculatorService applies volume discounts per annual spend tier and rounds tax totals to whole cents before the invoice is emitted.",
        embed: true,
    },
    GoldenFile {
        id: "g_fc_sidecar",
        path: "crates/ibis-core/src/process_tree.rs",
        content: "The kernel job object guarantees that sidecar children die with the parent process: every spawned child joins the job and the kernel kills the whole tree when the last job handle closes.",
        embed: true,
    },
    GoldenFile {
        id: "g_fc_eval",
        path: "crates/ibis-optimization/src/eval.rs",
        content: "The retrieval eval runner scores golden dataset cases with mean reciprocal rank and recall at five and fifteen, then aggregates per-channel ranking statistics into the baseline report.",
        embed: true,
    },
    GoldenFile {
        id: "g_fc_palette",
        path: "web/ui/palette.ts",
        content: "The palette module parses hex color strings into RGB channels and clamps invalid components to the nearest sRGB boundary.",
        embed: true,
    },
    // Lexical-only file chunk (section 9 case 3): no embedding — only the
    // file_chunk_text channel can retrieve it.
    GoldenFile {
        id: "g_fc_lex",
        path: "src/lex_notes.rs",
        content: "The Mangrove export pipeline flushes every dusk before the tide table rotates.",
        embed: false,
    },
    // Special-character path (section 9 case 8): the expected id carries the
    // raw path (comma + space) — id round-trip through retrieval.
    GoldenFile {
        id: "g_fc_special",
        path: "src/special path,file.rs",
        content: "The Baobab cache reindex warms at midnight to dodge the migration storm.",
        embed: false,
    },
];

const GOLDEN_MEMORIES: &[GoldenMemory] = &[
    GoldenMemory {
        id: "g_mem_mcp_timeout",
        kind: "fix",
        title: "MCP response timeout during plugin store init",
        content: "Timeout waiting for MCP response id=1 during plugin store init: the ide-host response timeout was raised from 15 to 60 seconds.",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_spawn_thread",
        kind: "rule",
        title: "Spawn sidecars only from long-lived threads",
        content: "Spawn sidecar children only from long-lived runtime worker threads, never from spawn_blocking: the PDEATHSIG parent is the creating thread.",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_mathmax",
        kind: "gotcha",
        title: "math::max arity on SurrealDB 3.x",
        content: "math::max takes a single array on SurrealDB 3.x; the two-argument form from the 2.x era fails live.",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_rrf",
        kind: "decision",
        title: "Reciprocal rank fusion",
        content: "Reciprocal rank fusion replaces the additive channel merge once the retrieval baseline confirms the gain over the additive scores.",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_surreal_url",
        kind: "fact",
        title: "Test sidecar url",
        content: "Test spikes connect to the SurrealDB sidecar url ws://127.0.0.1:38000 with root credentials.",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    // Title-only lexical memory (section 9 case 1): no embedding.
    GoldenMemory {
        id: "g_mem_lextitle",
        kind: "decision",
        title: "Zephyr bootstrap catalog regression",
        content: "unrelated body text about pipelines",
        visibility: "team",
        status: "current",
        owner: "",
        embed: false,
    },
    // Content-only lexical memory (section 9 case 2): no embedding.
    GoldenMemory {
        id: "g_mem_lexcontent",
        kind: "fact",
        title: "pipeline notes",
        content: "the Larkspur kiln calibration schedule changed after the audit",
        visibility: "team",
        status: "current",
        owner: "",
        embed: false,
    },
    // Dual-presence memory (section 9 case 4): the query hits its vector
    // channel AND both lexical channels.
    GoldenMemory {
        id: "g_mem_dual",
        kind: "design",
        title: "Wisteria scheduler fairness window",
        content: "the Wisteria scheduler fairness window avoids starvation across repos",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    // Conflict pair (section 9 case 5): high vector similarity vs high BM25.
    GoldenMemory {
        id: "g_mem_vecstrong",
        kind: "fact",
        title: "alpha vector doc",
        content: "alpha beta gamma delta epsilon",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_lexstrong",
        kind: "fact",
        title: "alpha lexical doc",
        content: "alpha beta gamma delta epsilon alpha beta gamma delta epsilon alpha beta gamma delta epsilon filler filler filler",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    // Personal memory of ANOTHER actor (section 9 case 6): must never surface.
    GoldenMemory {
        id: "g_mem_personal",
        kind: "fact",
        title: "Juniper quota ledger",
        content: "the Juniper quota ledger tracks nightly spend per key",
        visibility: "personal",
        status: "current",
        owner: "someoneelse",
        embed: true,
    },
    // Team counterpart of the personal memory (the expected hit of case 6).
    GoldenMemory {
        id: "g_mem_teamquota",
        kind: "fact",
        title: "Juniper quota ledger team",
        content: "the Juniper quota ledger tracks nightly spend per key",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
    // Superseded memory (section 9 case 7): excluded by currentness.
    GoldenMemory {
        id: "g_mem_superseded",
        kind: "rule",
        title: "Cypress retry ladder old",
        content: "the Cypress retry ladder caps at three attempts",
        visibility: "team",
        status: "superseded",
        owner: "",
        embed: true,
    },
    GoldenMemory {
        id: "g_mem_currentladder",
        kind: "rule",
        title: "Cypress retry ladder new",
        content: "the Cypress retry ladder caps at five attempts now",
        visibility: "team",
        status: "current",
        owner: "",
        embed: true,
    },
];

const GOLDEN_COMMITS: &[GoldenCommit] = &[GoldenCommit {
    id: "g_cc_rrf",
    hash: "9f1c2b7a00000000000000000000000000000000",
    content: "feat(search): introduce reciprocal rank fusion for the channel merge, re-scoring the ranked board before truncation.",
}];

const GOLDEN_REASONING: &[GoldenReasoning] = &[GoldenReasoning {
    id: "g_r_decay",
    question: "why do freshly created chunks rank higher?",
    outcome: "the score adds a time decay term so recent knowledge outranks stale duplicates.",
}];

fn vars_of(pairs: Vec<(&str, Value)>) -> Vars {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// (Re)creates the golden estate in the connected database: idempotent DDL
/// (IF NOT EXISTS), wipe of the estate tables, then one `CREATE` per row with
/// bound values (F-002) and embeddings computed by the ACTIVE provider (the
/// same instance the runner will use for query embeddings). Returns the
/// number of rows created.
pub async fn seed_golden_estate(
    db: &Arc<dyn Executor>,
    ai: &Arc<dyn AIProvider>,
) -> Result<usize, String> {
    ibis_db::schema::init_schema(db.as_ref())
        .await
        .map_err(|e| format!("init schema: {e}"))?;

    for table in [
        "source_file",
        "file_chunk",
        "commit_chunk",
        "reasoning",
        "memory",
    ] {
        db.execute(&format!("DELETE {table};"))
            .await
            .map_err(|e| format!("wipe {table}: {e}"))?;
    }

    let mut rows = 0usize;

    // WQ-7 + F02: source_file/file_chunk ids are arrays (`[repo, file_path]`,
    // `[file_id, line]`); the golden repo component is `repo:⟨golden⟩` (the
    // repo-record key convention) and the path component is the **raw**
    // repo-relative path (lossless — F02). The dataset expected ids
    // reference exactly these generated strings.
    let golden_repo_ref = ibis_db::ids::format_record_id(
        ibis_db::schema::names::REPO,
        &ibis_db::ids::sanitize_id("golden"),
    );

    for f in GOLDEN_FILES {
        let sf_id = ibis_db::ids::source_file_id(&golden_repo_ref, f.path);
        let chunk_id = ibis_db::ids::file_chunk_id(&sf_id, 0);
        db.smart_query(
            &format!(
                "CREATE {sf_id} SET path = $path, rel_path = $path, \
                 hash = $hash, is_head = true, repo_name = 'golden';"
            ),
            vars_of(vec![
                ("path", Value::from_t(f.path)),
                ("hash", Value::from_t(f.id)),
            ]),
        )
        .await
        .map_err(|e| format!("seed {}: {e}", sf_id))?;
        // `embed = false` seeds a lexical-only chunk: no embedding field at
        // all (the vector channel skips it, file_chunk_text carries it).
        let mut chunk_vars = vars_of(vec![
            ("content", Value::from_t(f.content)),
            ("hash", Value::from_t(f.id)),
        ]);
        let embedding_set = if f.embed {
            chunk_vars.insert(
                "embedding".into(),
                Value::from_t(Embedding(embed_of(ai, f.content).await?)),
            );
            ", embedding = $embedding"
        } else {
            ""
        };
        db.smart_query(
            &format!(
                "CREATE {chunk_id} SET file = {sf_id}, \
                 content = $content, hash = $hash, start_line = 0{embedding_set};"
            ),
            chunk_vars,
        )
        .await
        .map_err(|e| format!("seed {chunk_id}: {e}"))?;
        rows += 2;
    }

    for m in GOLDEN_MEMORIES {
        let embedding_set = if m.embed {
            ", embedding = $embedding"
        } else {
            ""
        };
        let mut mem_vars = vars_of(vec![
            ("content", Value::from_t(m.content)),
            ("kind", Value::from_t(m.kind)),
            ("title", Value::from_t(m.title)),
            ("status", Value::from_t(m.status)),
            ("visibility", Value::from_t(m.visibility)),
            ("owner", Value::from_t(m.owner)),
        ]);
        if m.embed {
            mem_vars.insert(
                "embedding".into(),
                Value::from_t(Embedding(embed_of(ai, m.content).await?)),
            );
        }
        db.smart_query(
            &format!(
                "CREATE memory:⟨{}⟩ SET content = $content, kind = $kind, title = $title, status = $status, visibility = $visibility, owner = $owner{embedding_set};",
                m.id
            ),
            mem_vars,
        )
        .await
        .map_err(|e| format!("seed {}: {e}", m.id))?;
        rows += 1;
    }

    for c in GOLDEN_COMMITS {
        let emb = embed_of(ai, c.content).await?;
        db.smart_query(
            &format!(
                "CREATE commit_chunk:⟨{}⟩ SET content = $content, commit_hash = $hash, embedding = $embedding;",
                c.id
            ),
            vars_of(vec![
                ("content", Value::from_t(c.content)),
                ("hash", Value::from_t(c.hash)),
                ("embedding", Value::from_t(Embedding(emb))),
            ]),
        )
        .await
        .map_err(|e| format!("seed {}: {e}", c.id))?;
        rows += 1;
    }

    for r in GOLDEN_REASONING {
        let emb = embed_of(ai, &format!("{}\n{}", r.question, r.outcome)).await?;
        db.smart_query(
            &format!(
                "CREATE reasoning:⟨{}⟩ SET question = $question, outcome = $outcome, embedding = $embedding;",
                r.id
            ),
            vars_of(vec![
                ("question", Value::from_t(r.question)),
                ("outcome", Value::from_t(r.outcome)),
                ("embedding", Value::from_t(Embedding(emb))),
            ]),
        )
        .await
        .map_err(|e| format!("seed {}: {e}", r.id))?;
        rows += 1;
    }

    Ok(rows)
}

async fn embed_of(ai: &Arc<dyn AIProvider>, text: &str) -> Result<Vec<f32>, String> {
    ai.embed_text(text)
        .await
        .map_err(|e| format!("embed failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ibis_db::executor::QueryOutcome;
    use ibis_db::value::Object;
    use ibis_db::DbError;
    use std::sync::Mutex;

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

    /// Longest-substring-match fake (pattern `FakeDb` in ibis-search
    /// test_support): records calls, returns the registered value whose key
    /// is the longest substring of the SQL.
    struct EvalDb {
        return_data: Vec<(String, Value)>,
        calls: Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    impl EvalDb {
        fn new(return_data: Vec<(&str, Value)>) -> Self {
            EvalDb {
                return_data: return_data
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                calls: Mutex::new(Vec::new()),
                fail_on: None,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Executor for EvalDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
            self.calls.lock().unwrap().push(sql.to_string());
            if let Some(fail) = &self.fail_on {
                if sql.contains(fail.as_str()) {
                    return Err(DbError::Query {
                        message: format!("fake failure on {fail}"),
                        duration_ms: 0,
                    });
                }
            }
            let mut best_key = "";
            let mut best: Option<&Value> = None;
            for (k, v) in &self.return_data {
                if sql.contains(k.as_str()) && k.len() > best_key.len() {
                    best_key = k;
                    best = Some(v);
                }
            }
            Ok(QueryOutcome::Single(best.cloned().unwrap_or(Value::None)))
        }

        async fn smart_query(&self, sql: &str, _vars: Vars) -> Result<QueryOutcome, DbError> {
            self.execute(sql).await
        }
    }

    /// Embeds with the deterministic offline vector; generates never.
    struct EvalAi;

    #[async_trait]
    impl AIProvider for EvalAi {
        fn name(&self) -> &str {
            "eval-ai"
        }

        async fn is_functional(&self) -> bool {
            true
        }

        async fn embed_text(&self, text: &str) -> Result<Vec<f32>, AiError> {
            Ok(offline_vec(text))
        }

        async fn generate_content(
            &self,
            _contents: &[Content],
            _config: GenerationConfig,
        ) -> Result<Candidate, AiError> {
            Err(AiError::other("eval ai does not generate"))
        }
    }

    fn result(id: &str, score: f64, source_kind: &str) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            score,
            source_kind: source_kind.to_string(),
            ..Default::default()
        }
    }

    fn case(id: &str, query: &str, expected: &[&str]) -> EvalCase {
        EvalCase {
            id: id.to_string(),
            query: query.to_string(),
            expected_ids: expected.iter().map(|s| s.to_string()).collect(),
            absent_ids: Vec::new(),
            note: String::new(),
        }
    }

    // --- pure scorer -------------------------------------------------------

    #[test]
    fn scores_mrr_recall_and_channel_attribution() {
        let board = [
            result("file_chunk:g_fc_eval", 0.95, "file_chunk"),
            result("memory:g_mem_rrf", 0.85, "memory"),
            result("reasoning:g_r_decay", 0.75, "reasoning"),
            result("file_chunk:g_fc_price", 0.65, "file_chunk"),
            result("commit_chunk:g_cc_rrf", 0.55, "commit_chunk"),
        ];
        // Angle-bracket authoring form must normalize to the plain hit id.
        let c = case("c1", "q", &["commit_chunk:⟨g_cc_rrf⟩"]);
        let r = score_case(&c, &board);
        assert_eq!(r.status, CaseStatus::Ok);
        assert_eq!(r.mrr, 0.2);
        assert_eq!(r.first_rank, Some(5));
        assert_eq!(r.top_channel, Some("commit_chunk".to_string()));
        assert_eq!(r.hits, vec![5]);
        assert_eq!(r.recall_at_5, 1.0);
        assert_eq!(r.recall_at_15, 1.0);

        let multi = case("c2", "q", &["file_chunk:g_fc_eval", "memory:g_mem_rrf"]);
        let r = score_case(&multi, &board);
        assert_eq!(r.mrr, 1.0);
        assert_eq!(r.first_rank, Some(1));
        assert_eq!(r.top_channel, Some("file_chunk".to_string()));
        assert_eq!(r.hits, vec![1, 2]);
    }

    #[test]
    fn recall_windows_and_misses() {
        let board = [
            result("memory:⟨g_mem_x⟩", 0.9, "memory"),
            result("file_chunk:g_a", 0.8, "file_chunk"),
            result("file_chunk:g_b", 0.7, "file_chunk"),
            result("file_chunk:g_c", 0.6, "file_chunk"),
            result("file_chunk:g_d", 0.5, "file_chunk"),
            result("file_chunk:g_e", 0.4, "file_chunk"),
            result("file_chunk:g_f", 0.3, "file_chunk"),
        ];
        // Hits at ranks 5 and 7: inside @5 once, inside @15 twice.
        let c = case("c1", "q", &["file_chunk:g_d", "file_chunk:g_f"]);
        let r = score_case(&c, &board);
        assert_eq!(r.recall_at_5, 0.5);
        assert_eq!(r.recall_at_15, 1.0);
        assert_eq!(r.mrr, 0.2);
        assert_eq!(r.top_channel, Some("file_chunk".to_string()));

        let miss = case("c2", "q", &["memory:g_absent"]);
        let r = score_case(&miss, &board);
        assert_eq!(r.mrr, 0.0);
        assert_eq!(r.recall_at_5, 0.0);
        assert_eq!(r.recall_at_15, 0.0);
        assert_eq!(r.first_rank, None);
        assert_eq!(r.top_channel, None);
        assert!(r.hits.is_empty());
    }

    // --- runner over the live service path ---------------------------------

    fn board_rows() -> Vec<Value> {
        vec![
            obj(vec![
                ("id", Value::from_t("file_chunk:g_fc_eval")),
                (
                    "path",
                    Value::from_t("crates/ibis-optimization/src/eval.rs"),
                ),
                ("content", Value::from_t("eval runner scores golden cases")),
                ("score", Value::from_t(0.95f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("memory:g_mem_rrf")),
                ("content", Value::from_t("reciprocal rank fusion decision")),
                ("status", Value::from_t("current")),
                ("kind", Value::from_t("decision")),
                ("score", Value::from_t(0.85f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("commit_chunk:g_cc_rrf")),
                ("content", Value::from_t("introduce reciprocal rank fusion")),
                ("commit_hash", Value::from_t("abc1234")),
                ("score", Value::from_t(0.55f64)),
            ]),
        ]
    }

    #[tokio::test]
    async fn run_case_scores_over_ask_project_channels() {
        let db = Arc::new(EvalDb::new(vec![
            ("FROM file_chunk", arr(board_rows())),
            ("FROM commit_chunk", arr(vec![])),
            ("FROM memory", arr(vec![])),
            ("FROM reasoning", arr(vec![])),
        ]));
        let svc = Arc::new(SearchService::new(
            Some(db.clone()),
            Some(Arc::new(EvalAi) as Arc<dyn AIProvider>),
        ));
        let runner = RetrievalEvalRunner::new(svc);
        // The fake board merges channels in table order: file_chunk rows
        // first, then memory. Both expected hits are found, ranked 1 and 2.
        let c = case(
            "c1",
            "retrieval probe",
            &["memory:g_mem_rrf", "file_chunk:g_fc_eval"],
        );
        let r = runner.run_case(&c).await;
        assert_eq!(r.status, CaseStatus::Ok, "{:?}", r.error);
        assert_eq!(r.first_rank, Some(1), "file_chunk rows precede memory rows");
        assert_eq!(r.mrr, 1.0);
        assert_eq!(r.hits, vec![2, 1], "hits follow the expected-id order");
        assert_eq!(r.top_channel, Some("file_chunk".to_string()));
        assert_eq!(r.recall_at_5, 1.0);
    }

    #[tokio::test]
    async fn run_case_marks_degraded_search_as_error() {
        let mut db = EvalDb::new(vec![]);
        db.fail_on = Some("FROM file_chunk".to_string());
        let svc = Arc::new(SearchService::new(
            Some(Arc::new(db)),
            Some(Arc::new(EvalAi) as Arc<dyn AIProvider>),
        ));
        let runner = RetrievalEvalRunner::new(svc);
        let c = case("c1", "retrieval probe", &["memory:g_mem_rrf"]);
        let r = runner.run_case(&c).await;
        assert_eq!(r.status, CaseStatus::Error);
        assert_eq!(r.mrr, 0.0);
        assert!(r.error.is_some());
    }

    // --- dataset -----------------------------------------------------------

    #[test]
    fn dataset_validation_matrix() {
        let ok = RetrievalDataset {
            version: "1".to_string(),
            cases: vec![case("a", "query", &["memory:g_x"])],
        };
        assert!(ok.validate().is_ok());

        let empty = RetrievalDataset {
            version: "1".to_string(),
            cases: vec![],
        };
        assert_eq!(
            empty.validate().err(),
            Some("dataset has no cases".to_string())
        );

        let dup = RetrievalDataset {
            version: "1".to_string(),
            cases: vec![
                case("a", "q1", &["memory:g_x"]),
                case("a", "q2", &["memory:g_x"]),
            ],
        };
        assert!(dup.validate().err().unwrap().contains("duplicate case id"));

        let no_query = RetrievalDataset {
            version: "1".to_string(),
            cases: vec![case("a", "  ", &["memory:g_x"])],
        };
        assert!(no_query.validate().err().unwrap().contains("empty query"));

        let no_expected = RetrievalDataset {
            version: "1".to_string(),
            cases: vec![case("a", "q", &[])],
        };
        assert!(no_expected
            .validate()
            .err()
            .unwrap()
            .contains("no expected ids"));
    }

    #[test]
    fn normalize_strips_angle_brackets_only() {
        assert_eq!(normalize_record_id("memory:⟨g_mem_x⟩"), "memory:g_mem_x");
        assert_eq!(normalize_record_id(" memory:g_mem_x "), "memory:g_mem_x");
        assert_eq!(
            normalize_record_id("reasoning:g_r_decay"),
            "reasoning:g_r_decay"
        );
        assert_eq!(normalize_record_id("bare"), "bare");
    }

    #[tokio::test]
    async fn yaml_dataset_roundtrip_loads_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dataset.yaml");
        std::fs::write(
            &path,
            "version: \"1\"\ncases:\n  - id: r01_x\n    query: q\n    expected_ids: [\"memory:g_x\"]\n",
        )
        .unwrap();
        assert!(load_dataset(&path).is_ok());
        let bad = dir.path().join("bad.yaml");
        std::fs::write(
            &bad,
            "version: \"1\"\ncases:\n  - id: r01_x\n    query: q\n  - id: r01_x\n    query: q2\n",
        )
        .unwrap();
        assert!(load_dataset(&bad).is_err());
        let missing = dir.path().join("absent.yaml");
        assert!(load_dataset(&missing).is_err());
    }

    // --- report ------------------------------------------------------------

    fn ok_case(id: &str, mrr: f64, r5: f64, r15: f64, channel: Option<&str>) -> CaseResult {
        CaseResult {
            id: id.to_string(),
            status: CaseStatus::Ok,
            mrr,
            recall_at_5: r5,
            recall_at_15: r15,
            first_rank: if mrr > 0.0 { Some(1) } else { None },
            top_channel: channel.map(|s| s.to_string()),
            channels: channel.map(|s| vec![s.to_string()]).unwrap_or_default(),
            absent_leaks: Vec::new(),
            hits: vec![],
            error: None,
        }
    }

    #[test]
    fn aggregate_excludes_errors_and_counts_channels() {
        let cases = vec![
            ok_case("a", 1.0, 1.0, 1.0, Some("memory")),
            ok_case("b", 0.5, 0.5, 1.0, Some("file_chunk")),
            ok_case("c", 0.0, 0.0, 0.0, None),
            CaseResult {
                id: "d".to_string(),
                status: CaseStatus::Error,
                mrr: 0.0,
                recall_at_5: 0.0,
                recall_at_15: 0.0,
                first_rank: None,
                top_channel: None,
                channels: Vec::new(),
                absent_leaks: Vec::new(),
                hits: vec![],
                error: Some("degraded".to_string()),
            },
        ];
        let report = build_report("ds.yaml", "1", "offline-hash-768", "additive", true, cases);
        let agg = &report.aggregate;
        assert_eq!(agg.cases, 4);
        assert_eq!(agg.cases_ok, 3);
        assert_eq!(agg.cases_error, 1);
        assert!((agg.mrr - (1.0 + 0.5 + 0.0) / 3.0).abs() < 1e-9);
        assert!((agg.recall_at_5 - (1.0 + 0.5 + 0.0) / 3.0).abs() < 1e-9);
        assert!((agg.recall_at_15 - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(agg.by_channel.get("memory").unwrap().cases, 1);
        assert_eq!(agg.by_channel.get("file_chunk").unwrap().cases, 1);
        assert!(report.below_threshold(0.6));
        assert!(!report.below_threshold(0.4));
        // No ok cases: never "below threshold" — the all-error state is
        // reported separately by the caller.
        let all_error = vec![CaseResult {
            id: "e".to_string(),
            status: CaseStatus::Error,
            mrr: 0.0,
            recall_at_5: 0.0,
            recall_at_15: 0.0,
            first_rank: None,
            top_channel: None,
            channels: Vec::new(),
            absent_leaks: Vec::new(),
            hits: vec![],
            error: Some("x".to_string()),
        }];
        let report = build_report(
            "ds.yaml",
            "1",
            "offline-hash-768",
            "additive",
            false,
            all_error,
        );
        assert!(!report.below_threshold(0.5));
        assert_eq!(report.aggregate.cases_ok, 0);
    }

    // --- golden estate seed ------------------------------------------------

    #[tokio::test]
    async fn seed_creates_full_estate_with_bound_embeddings() {
        let db = Arc::new(EvalDb::new(vec![]));
        let ai = Arc::new(EvalAi) as Arc<dyn AIProvider>;
        let rows = seed_golden_estate(&(db.clone() as Arc<dyn Executor>), &ai)
            .await
            .expect("seed");
        // 6 files × (source_file + file_chunk) + 14 memories + 1 commit + 1 reasoning
        assert_eq!(rows, 28);
        let calls = db.calls();
        for table in [
            "DELETE source_file;",
            "DELETE memory;",
            "DELETE commit_chunk;",
            "DELETE reasoning;",
        ] {
            assert!(
                calls.iter().any(|c| c.contains(table)),
                "missing wipe {table}"
            );
        }
        for id in [
            // WQ-7 + F02: file_chunk seeds use the array id
            // `[file_id, line]` with the raw path component (seed literal
            // form — the golden repo key stays bare-safe).
            "CREATE file_chunk:[source_file:[repo:golden, 'src/billing/price_calculator.rs'], 0]",
            "CREATE memory:⟨g_mem_mathmax⟩",
            "CREATE commit_chunk:⟨g_cc_rrf⟩",
            "CREATE reasoning:⟨g_r_decay⟩",
        ] {
            assert!(calls.iter().any(|c| c.contains(id)), "missing {id}");
        }
    }
}
