//! Hybrid ask: vector search with time decay across the
//! four chunk tables, multi-signal re-ranking, graph-context enrichment —
//! plus the raw query passthrough.
//!
//! Deltas:
//! - embeddings bind as `$vec` and memory visibility actors bind as `$actor`
//!   (F-002: every content-bearing query uses bound variables);
//! - [`DbCostStore`] adapts the search executor to `ibis_ai::CostStore` so
//!   token accounting flows through the lane-C tracker.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use ibis_ai::{AIProvider, CostStore, CostTracker};
use ibis_db::executor::{QueryOutcome, Vars};
use ibis_db::Executor;
use ibis_git::{GitAnchorSource, GitClaimWriter, GitNotesSource};

use crate::commit_meaning;
use crate::error::{Result, SearchError};
use crate::graph::{self, CommitSummary, GraphContext, IssueSummary};
use crate::memory::temporal::{ask_memory_visibility_sql, memory_currentness_sql};
use crate::rerank::{rerank_hits, RerankMode};
use crate::retrieval_boost::{
    apply_multi_signal_boost, apply_rrf_boost, calibrate_lexical_score, expand_query,
    resolve_linked_symbols, rrf_fuse, rrf_max_score, tokenize_query, FusionMode,
    QueryExpansionMode, ScoredHit, CURRENT_EPSILON, RRF_K,
};
use crate::rows;
use crate::ActorContext;

/// Per-table hit budget for the vector channels (the final cut after fusion).
pub const VECTOR_CHANNEL_LIMIT: usize = 15;

/// Default candidate window (WQ-7): channels fetch this many rows index-ordered
/// (`ORDER BY vector::distance::cosine` / `search::score`), the temporal
/// rescore and the future A3 reranker run inside the window, and the final cut
/// stays [`VECTOR_CHANNEL_LIMIT`]. Configurable via `search.candidates`.
pub const DEFAULT_CANDIDATE_WINDOW: usize = 50;

/// A knowledge chunk result — code, commit meaning, memory or reasoning
/// (`Result; wire names preserved exactly).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    #[serde(rename = "code_chunk_id")]
    pub id: String,
    #[serde(rename = "file_path")]
    pub path: String,
    #[serde(rename = "relevance_score")]
    pub score: f64,
    /// True when the chunk belongs to the current version of the file.
    #[serde(rename = "is_current")]
    pub is_current: bool,
    #[serde(
        rename = "source_kind",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub source_kind: String,
    #[serde(
        rename = "commit_hash",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub commit_hash: String,
    #[serde(rename = "status", skip_serializing_if = "String::is_empty", default)]
    pub status: String,
    #[serde(rename = "kind", skip_serializing_if = "String::is_empty", default)]
    pub kind: String,
    /// Internal fusion channel id of the best contribution (F03/D1: eval and
    /// diagnostics need more than `source_kind` — `memory` does not
    /// distinguish the vector, title and content channels). Empty for hits
    /// without channel attribution.
    #[serde(rename = "channel", skip_serializing_if = "String::is_empty", default)]
    pub channel: String,
    /// Resolved ref for ask at_ref hits (the world-state spec).
    #[serde(
        rename = "as_of_ref",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub as_of_ref: String,
    /// `git_show` when bytes were materialized at the ref.
    #[serde(
        rename = "content_source",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub content_source: String,
    #[serde(default)]
    pub context: ContextData,
    #[serde(rename = "content", skip_serializing_if = "String::is_empty", default)]
    pub content: String,
}

/// Graph context attached to results.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ContextData {
    #[serde(rename = "related_issues")]
    pub related_issues: Vec<IssueSummary>,
    #[serde(rename = "commits")]
    pub commits: Vec<CommitSummary>,
    #[serde(rename = "expert_authors")]
    pub expert_authors: Vec<String>,
}

/// Handles hybrid queries.
///
/// Degraded mode (D8): `db = None` is a first-class clone-only state — every
/// read path branches exhaustively on it instead of erroring.
pub struct SearchService {
    /// SurrealDB executor; `None` = degraded (clone-only) mode.
    pub db: Option<Arc<dyn Executor>>,
    /// AI client (embeddings + reasoning).
    pub ai: Option<Arc<dyn AIProvider>>,
    /// Git anchor source; `None` disables git binding.
    pub git_anchors: Option<Arc<dyn GitAnchorSource>>,
    /// Claim-file committer; `None` disables the git-tree claim store.
    pub claim_writer: Option<Arc<dyn GitClaimWriter>>,
    /// Git notes source for `refs/notes/ibis-claims`; `None` disables the
    /// notes rebuild (notes are optional cross-links).
    pub notes: Option<Arc<dyn GitNotesSource>>,
    /// Effective personal-claims policy (`db_only` | `stub` | `in_repo`;
    /// "" behaves as db_only).
    pub personal_claims_policy: String,
    /// Hot-ref code byte cache kill-switch
    /// (`world_state.code_cache_disabled`, default false = cache on).
    pub hot_ref_cache_disabled: bool,
    /// Channel fusion mode (`search.fusion`: additive|rrf — A2; default
    /// additive until the WQ-7 retune re-decides on the 4.0 baseline).
    pub fusion_mode: FusionMode,
    /// RRF smoothing constant k (A2 default 60; the eval sweeps k to pick
    /// the fusion winner — remediation plan section 9).
    pub rrf_k: f64,
    /// Candidate window (`search.candidates`, WQ-7): fetch size of every
    /// channel above the final [`VECTOR_CHANNEL_LIMIT`] cut, shared by the
    /// temporal rescore and the future A3 reranker.
    pub candidate_window: usize,
    /// Query expansion mode (`search.query_expansion`: off|symbol_variant —
    /// A4; default symbol_variant, zero-cost when no linked symbols resolve).
    pub query_expansion: QueryExpansionMode,
    /// Post-fusion reranker mode (`search.rerank`: off|local — A3; default
    /// off, activation guided by the A1 eval numbers).
    pub rerank_mode: RerankMode,
    /// Process-local TTL cache of ancestor sets (lazily created).
    pub(crate) ancestor_cache: std::sync::OnceLock<ibis_git::AncestorCache>,
}

impl Default for SearchService {
    fn default() -> Self {
        SearchService {
            db: None,
            ai: None,
            git_anchors: None,
            claim_writer: None,
            notes: None,
            personal_claims_policy: String::new(),
            hot_ref_cache_disabled: false,
            fusion_mode: FusionMode::default(),
            rrf_k: RRF_K,
            candidate_window: DEFAULT_CANDIDATE_WINDOW,
            query_expansion: QueryExpansionMode::default(),
            rerank_mode: RerankMode::default(),
            ancestor_cache: std::sync::OnceLock::new(),
        }
    }
}

impl SearchService {
    /// Builds a service with a database and AI client.
    pub fn new(db: Option<Arc<dyn Executor>>, ai: Option<Arc<dyn AIProvider>>) -> Self {
        SearchService {
            db,
            ai,
            ..Default::default()
        }
    }

    /// D8: the degraded-mode gate. `None` db → "database not connected".
    pub(crate) fn require_db(&self) -> Result<&Arc<dyn Executor>> {
        self.db.as_ref().ok_or(SearchError::DatabaseNotConnected)
    }

    /// `ancestorCache lazy init.
    pub(crate) fn ancestor_cache(&self) -> &ibis_git::AncestorCache {
        self.ancestor_cache
            .get_or_init(ibis_git::AncestorCache::new)
    }

    /// Executes a raw SurrealQL query for power users.
    pub async fn raw_query(&self, ql: &str) -> Result<QueryOutcome> {
        let db = self.require_db()?;
        Ok(db.execute(ql).await?)
    }
}

/// Adapts the search [`Executor`] to the lane-C [`CostStore`] seam
/// (`ai.NewCostTracker(s.DB) + `UPSERT <id> SET tokens_used = …`; the
/// legacy 2.x `UPDATE` auto-created missing records, 3.x needs `UPSERT`).
struct DbCostStore(Arc<dyn Executor>);

#[async_trait]
impl CostStore for DbCostStore {
    async fn increment_tokens(
        &self,
        record_id: &str,
        total: u64,
    ) -> std::result::Result<(), ibis_ai::AiError> {
        // Entity ids like "system:search_vector" interpolate only when they
        // are safe record ids (F-002: identifiers only after vetting).
        if !ibis_db::ids::is_safe_record_id(record_id) {
            return Ok(());
        }
        // `+=` is the 3.x idiomatic counter increment (type inference: int,
        // default 0 on first write — probed; docs/statements/upsert.md).
        let sql = format!("UPSERT {record_id} SET tokens_used += {total};");
        self.0
            .execute(&sql)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(())
    }
}

/// Runs the cost tracker against the service DB (no-op when degraded).
pub(crate) fn cost_tracker(service: &SearchService) -> CostTracker {
    CostTracker::new(
        service
            .db
            .clone()
            .map(|db| Arc::new(DbCostStore(db)) as Arc<dyn CostStore>),
    )
}

impl SearchService {
    /// A4: embeds the literal query plus the optional symbol-variant query.
    /// Shared by `ask_project` and `ask_project_at_ref` so both retrieval
    /// paths inherit the same expansion/cost/fail-open semantics. The
    /// primary embedding error propagates; the variant is best-effort
    /// (warn + primary pass only on failure).
    pub(crate) async fn query_embeddings(
        &self,
        ai: &Arc<dyn AIProvider>,
        query: &str,
        linked_symbols: &[String],
    ) -> Result<Vec<Vec<f32>>> {
        let primary = ai
            .embed_text(query)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;
        cost_tracker(self)
            .record_embedding_usage("system:search_vector", query)
            .await;
        let mut embeddings = vec![primary];
        if self.query_expansion == QueryExpansionMode::SymbolVariant {
            if let Some(variant) = expand_query(query, linked_symbols).get(1) {
                match ai.embed_text(variant).await {
                    Ok(vec) => {
                        cost_tracker(self)
                            .record_embedding_usage("system:search_vector", variant)
                            .await;
                        embeddings.push(vec);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "query expansion: variant embedding failed; primary only: {e}"
                        );
                    }
                }
            }
        }
        Ok(embeddings)
    }
}

/// Internal decoded vector-search chunk.
#[derive(Debug, Clone)]
struct ChunkResult {
    id: String,
    path: String,
    content: String,
    score: f64,
    hash: String,
    current_hash: String,
    source_kind: String,
    /// Fusion channel that produced this hit (F03/D1 — internal id: vector
    /// channels are their table name, lexical channels carry a `_text`
    /// suffix).
    channel: String,
    commit_hash: String,
    status: String,
    kind: String,
    /// Days since the temporal anchor (memory `created_at`, commit_chunk
    /// `commit_date`); `None` = no anchor (constant decay term).
    age_days: Option<f64>,
    /// Set when the canonical HEAD hash lookup matched this chunk's hash
    /// (drives the "is current" bump/epsilon and the result flag).
    at_head: bool,
}

impl ChunkResult {
    fn from_row(row: &ibis_db::value::Object, source_kind: &str, channel: &str) -> Self {
        ChunkResult {
            id: rows::record_id(rows::field(row, "id")),
            source_kind: source_kind.to_string(),
            channel: channel.to_string(),
            content: rows::scalar_string(rows::field(row, "content")),
            path: rows::scalar_string(rows::field(row, "path")),
            hash: rows::scalar_string(rows::field(row, "hash")),
            current_hash: rows::scalar_string(rows::field(row, "current_hash")),
            commit_hash: rows::scalar_string(rows::field(row, "commit_hash")),
            status: rows::scalar_string(rows::field(row, "status")),
            kind: rows::scalar_string(rows::field(row, "kind")),
            score: rows::numeric_float(rows::field(row, "score")),
            age_days: rows::numeric_float_opt(rows::field(row, "age_days")),
            at_head: false,
        }
    }
}

/// Rust-side temporal decay (WQ-7): the SQL channel ranks by the index
/// (`ORDER BY vector::distance::cosine`), so decay is a **rescore on the
/// returned window** and can never pull a row back in from outside it.
/// Mirrors the retired `ask_score_expr`: temporal term `0.3` without an
/// anchor, else `max(0, 1 - days/365) * 0.3`; similarity weight `0.7`.
pub(crate) fn apply_temporal_decay(score: f64, age_days: Option<f64>) -> f64 {
    let temporal = match age_days {
        None => 0.3,
        Some(days) => (1.0 - days / 365.0).max(0.0) * 0.3,
    };
    score * 0.7 + temporal
}

/// A4 cross-pass merge for one channel: a row already present keeps
/// `score = max(existing, incoming)` (the variant pass can only strengthen);
/// a row seen only in the variant pass joins the channel. Exactly one entry
/// per id, always — in-pass duplicates collapse into the existing slot.
fn merge_channel_pass(
    channel_rows: &mut Vec<ChunkResult>,
    index: &mut HashMap<String, usize>,
    incoming: ChunkResult,
) {
    match index.get(&incoming.id) {
        Some(&idx) => {
            if incoming.score > channel_rows[idx].score {
                channel_rows[idx].score = incoming.score;
            }
        }
        None => {
            index.insert(incoming.id.clone(), channel_rows.len());
            channel_rows.push(incoming);
        }
    }
}

impl SearchService {
    /// Performs the hybrid search:
    /// 1. vector search with time decay on file_chunk, commit_chunk, memory
    ///    and reasoning (SurrealDB multi-table FROM returns empty for vector
    ///    queries, so each table is queried separately and merged);
    /// 2. graph traversal enrichment (weighted).
    pub async fn ask_project(
        &self,
        actor: &ActorContext,
        query: &str,
    ) -> Result<Vec<SearchResult>> {
        // Degraded-mode gate first (audit H-A05: embedded before checking
        // DB, burning an AI round-trip on requests that must fail).
        let db = self.require_db()?;
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| SearchError::Embedding("AI provider not configured".into()))?;

        // A4: tokenization and the linked-symbol lookup moved ahead of the
        // embedding — the expansion variant needs the resolved symbols, and
        // the boost stage below reuses the same tokens.
        let (keywords, entities) = tokenize_query(query);
        let linked_symbols = resolve_linked_symbols(self.db.as_ref(), &entities).await;

        // A4 (query expansion): the shared helper embeds the literal query
        // plus the symbol variant when the mode is on and the variant
        // differs (no linked symbols → fast path, zero extra cost). The
        // variant can only strengthen retrieval: the vector channels keep
        // `score = max(primary_pass, variant_pass)` per row. A variant
        // embedding failure degrades to the primary pass (fail-open).
        let vecs = self.query_embeddings(ai, query, &linked_symbols).await?;
        let vec_json = commit_meaning::vec_value(&vecs[0]);
        let vec2_json = vecs.get(1).map(|v| commit_meaning::vec_value(v));
        let window = self.candidate_window.max(VECTOR_CHANNEL_LIMIT);
        let dist_expr = "vector::distance::cosine(embedding, $vec)";
        let sim_expr = "vector::similarity::cosine(embedding, $vec)";

        let trusted_actor = actor.resolve_trusted_actor("");
        let (memory_visibility, memory_vars) = ask_memory_visibility_sql(&trusted_actor);
        let memory_where = format!(
            "embedding != NONE AND {} AND {}",
            memory_currentness_sql(),
            memory_visibility
        );
        // Lexical channel filter: currentness + visibility, no embedding clause.
        let lexical_where = format!("{} AND {}", memory_currentness_sql(), memory_visibility);

        struct TableQuery<'a> {
            name: &'a str,
            /// Public `source_kind` of the hits (F03: the lexical channels of
            /// a table keep the table's public kind).
            kind: &'a str,
            sql: String,
            /// Vector channels get the Rust temporal rescore; lexical (BM25)
            /// channels keep their calibrated score.
            temporal: bool,
        }
        let queries = [
            TableQuery {
                name: "file_chunk",
                kind: "file_chunk",
                sql: format!(
                    "SELECT id, file.path as path, content, hash, file.hash as current_hash, {sim_expr} as score FROM file_chunk WHERE embedding != NONE ORDER BY {dist_expr} LIMIT {window};"
                ),
                temporal: true,
            },
            TableQuery {
                name: "commit_chunk",
                kind: "commit_chunk",
                sql: commit_meaning::commit_chunk_ask_query(window),
                temporal: true,
            },
            TableQuery {
                name: "memory",
                kind: "memory",
                sql: format!(
                    "SELECT id, NONE as path, content, status, kind, duration::days(time::now() - created_at) as age_days, NONE as hash, NONE as current_hash, {sim_expr} as score FROM memory WHERE {memory_where} ORDER BY {dist_expr} LIMIT {window};"
                ),
                temporal: true,
            },
            TableQuery {
                name: "reasoning",
                kind: "reasoning",
                sql: format!(
                    "SELECT id, NONE as path, string::concat(question OR '', '\n', outcome OR '') as content, NONE as hash, NONE as current_hash, NONE as age_days, {sim_expr} as score FROM reasoning WHERE embedding != NONE ORDER BY {dist_expr} LIMIT {window};"
                ),
                temporal: true,
            },
            // Lexical channels (WQ-7 + D3/F06): BM25 on memory.title,
            // memory.content and file_chunk.content. Two separate queries per
            // table — one FULLTEXT index per field — fused as independent
            // rank lists; the file_chunk content channel feeds `ask_project`
            // with id/path/hash/current_hash like the vector channel.
            TableQuery {
                name: "memory_text_title",
                kind: "memory",
                sql: format!(
                    "SELECT id, NONE as path, content, status, kind, NONE as hash, NONE as current_hash, search::score(1) as score FROM memory WHERE title @1@ $q AND {lexical_where} ORDER BY search::score(1) DESC LIMIT {window};"
                ),
                temporal: false,
            },
            TableQuery {
                name: "memory_text_content",
                kind: "memory",
                sql: format!(
                    "SELECT id, NONE as path, content, status, kind, NONE as hash, NONE as current_hash, search::score(1) as score FROM memory WHERE content @1@ $q AND {lexical_where} ORDER BY search::score(1) DESC LIMIT {window};"
                ),
                temporal: false,
            },
            TableQuery {
                name: "file_chunk_text",
                kind: "file_chunk",
                sql: format!(
                    "SELECT id, file.path as path, content, hash, file.hash as current_hash, search::score(1) as score FROM file_chunk WHERE content @1@ $q ORDER BY search::score(1) DESC LIMIT {window};"
                ),
                temporal: false,
            },
        ];

        // F03/D1: channels are collected independently — the same document
        // MAY appear in several channels (each contributes to the fusion);
        // only duplicates INSIDE one channel collapse (one entry per id).
        // A4: vector channels run one pass per embedding (primary + optional
        // symbol variant) and merge per row with `score = max(passes)`;
        // rows seen only in the variant pass join the channel. Lexical
        // channels keep the literal query (identical tokens → single pass).
        let mut channels: Vec<Vec<ChunkResult>> = Vec::with_capacity(queries.len());
        for tq in &queries {
            let passes: Vec<&ibis_db::value::Value> = if tq.temporal {
                match &vec2_json {
                    Some(v2) => vec![&vec_json, v2],
                    None => vec![&vec_json],
                }
            } else {
                vec![&vec_json]
            };
            let mut seen_in_channel: HashMap<String, usize> = HashMap::new();
            let mut channel_rows: Vec<ChunkResult> = Vec::new();
            for pass_vec in passes {
                let mut vars: Vars = memory_vars.iter().cloned().collect();
                vars.insert("vec".into(), pass_vec.clone());
                vars.insert("q".into(), ibis_db::value::Value::from_t(query));
                let outcome = db.smart_query(&tq.sql, vars).await.map_err(|e| {
                    SearchError::other(format!("vector search failed on {}: {e}", tq.name))
                })?;
                let Some(rows_values) = rows::normalize_rows(&outcome) else {
                    return Err(SearchError::other(format!(
                        "failed to parse chunks from {}: unexpected result type",
                        tq.name
                    )));
                };
                for row in rows::object_rows(&rows_values) {
                    let mut c = ChunkResult::from_row(row, tq.kind, tq.name);
                    if c.id.is_empty() && c.content.is_empty() {
                        continue;
                    }
                    if tq.temporal {
                        c.score = apply_temporal_decay(c.score, c.age_days);
                    } else {
                        // F04/D2: raw BM25 units are calibrated onto the blend
                        // scale before they meet the cosine/decay scores.
                        c.score = calibrate_lexical_score(c.score);
                    }
                    merge_channel_pass(&mut channel_rows, &mut seen_in_channel, c);
                }
            }
            if tq.temporal {
                // Rescore may reorder within the window: ranks follow the
                // decayed score, not the distance order.
                channel_rows.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            channels.push(channel_rows);
        }

        // Board of representative rows: one entry per document (first
        // occurrence across channels); channel ranks are kept separately.
        let mut chunks: Vec<ChunkResult> = Vec::new();
        let mut rep_index: HashMap<String, usize> = HashMap::new();
        let mut rank_lists: Vec<Vec<(usize, usize)>> = Vec::with_capacity(channels.len());
        for channel in &channels {
            let mut ranks: Vec<(usize, usize)> = Vec::with_capacity(channel.len());
            for (rank, hit) in channel.iter().enumerate() {
                let payload = match rep_index.get(&hit.id) {
                    Some(idx) => *idx,
                    None => {
                        let idx = chunks.len();
                        chunks.push(hit.clone());
                        rep_index.insert(hit.id.clone(), idx);
                        idx
                    }
                };
                ranks.push((payload, rank + 1));
            }
            rank_lists.push(ranks);
        }

        // Resolve canonical HEAD hashes by path (hash-suffixed source_file
        // IDs make file.hash == chunk.hash for historical versions too).
        let mut head_by_path: HashMap<String, String> = HashMap::new();
        for c in &chunks {
            if c.source_kind != "file_chunk" || c.path.is_empty() {
                continue;
            }
            if head_by_path.contains_key(&c.path) {
                continue;
            }
            if let Ok((hash, true)) = self.lookup_current_file_hash_by_path(&c.path).await {
                head_by_path.insert(c.path.clone(), hash);
            }
        }
        for chunk in chunks.iter_mut() {
            if chunk.source_kind != "file_chunk" {
                continue;
            }
            if let Some(head) = head_by_path.get(&chunk.path) {
                if !head.is_empty() {
                    chunk.current_hash = head.clone();
                    if !chunk.hash.is_empty() && chunk.hash == *head {
                        chunk.at_head = true;
                    }
                }
            }
        }

        // Fusion (F03/D1): the same document appearing in several channels
        // sums its per-channel contributions; the board dedupes AFTER the
        // fusion (one entry per document — `chunks` is already the
        // representative board). `keywords`/`entities`/`linked_symbols` were
        // resolved before the embedding (A4) and are reused here.
        let mut hits: Vec<ScoredHit> = match self.fusion_mode {
            FusionMode::Additive => chunks
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    // Sum the contributions of every channel that returned
                    // this document (F03: cross-channel summing; the board
                    // entry is only the representative metadata carrier).
                    let mut total = 0.0;
                    for channel in &channels {
                        if let Some(hit) = channel.iter().find(|h| h.id == c.id) {
                            total += hit.score;
                        }
                    }
                    ScoredHit {
                        text: c.content.clone(),
                        path: c.path.clone(),
                        tags: Vec::new(),
                        score: total,
                        id: c.id.clone(),
                        payload: i,
                        channel: c.channel.clone(),
                    }
                })
                .collect(),
            FusionMode::Rrf => {
                let channels_with_hits = rank_lists.iter().filter(|c| !c.is_empty()).count();
                let fused = rrf_fuse(&rank_lists, self.rrf_k);
                let rrf_max = rrf_max_score(channels_with_hits, self.rrf_k);
                chunks
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let base = (fused.get(&i).copied().unwrap_or(0.0) / rrf_max).min(1.0);
                        let epsilon = if c.at_head { CURRENT_EPSILON } else { 0.0 };
                        ScoredHit {
                            text: c.content.clone(),
                            path: c.path.clone(),
                            tags: Vec::new(),
                            score: base + epsilon,
                            id: c.id.clone(),
                            payload: i,
                            channel: c.channel.clone(),
                        }
                    })
                    .collect()
            }
        };
        // "is current" bump: once per document, after the fusion (additive
        // keeps the legacy fraction-of-score bump; RRF applies a constant
        // epsilon inside its branch above).
        if self.fusion_mode == FusionMode::Additive {
            for hit in hits.iter_mut() {
                if chunks
                    .get(hit.payload)
                    .is_some_and(|c| c.at_head && c.source_kind == "file_chunk")
                {
                    hit.score += 0.05;
                }
            }
        }
        // Best-contributing channel per document (F03/D1 diagnostics): pick the
        // channel whose contribution to this document is strongest. In
        // additive mode that is the largest per-channel score; in RRF mode
        // scores carry no comparable units, so the strength is the rank
        // position (rank 1 = strongest) from the same rank lists that feed
        // the fusion — never the per-channel score, whose `.max(1.0)`
        // clamp would flatten every strength to 1.0 and let the first
        // channel in query order win regardless of its rank.
        for hit in hits.iter_mut() {
            let Some(c) = chunks.get(hit.payload) else {
                continue;
            };
            let mut best: Option<(&str, f64)> = None;
            for (channel_idx, channel) in channels.iter().enumerate() {
                let Some(h) = channel.iter().find(|h| h.id == c.id) else {
                    continue;
                };
                let strength = match self.fusion_mode {
                    FusionMode::Additive => h.score,
                    FusionMode::Rrf => rank_lists[channel_idx]
                        .iter()
                        .find(|(payload, _)| *payload == hit.payload)
                        .map_or(0.0, |(_, rank)| 1.0 / *rank as f64),
                };
                if best.is_none_or(|(_, s)| strength > s) {
                    best = Some((h.channel.as_str(), strength));
                }
            }
            if let Some((name, _)) = best {
                hit.channel = name.to_string();
            }
        }
        match self.fusion_mode {
            FusionMode::Additive => {
                apply_multi_signal_boost(&mut hits, &keywords, &entities, &linked_symbols)
            }
            FusionMode::Rrf => apply_rrf_boost(&mut hits, &keywords, &entities, &linked_symbols),
        }
        // A3 (post-fusion reranker): one optional local-first semantic
        // scoring pass over the top candidate-window hits, before the final
        // cut. Fail-open: any AI failure keeps the fused order (warn log);
        // cost lands on `system:search_rerank`. Off by default.
        if self.rerank_mode == RerankMode::Local {
            let tracker = cost_tracker(self);
            rerank_hits(&mut hits, query, ai, &tracker, self.candidate_window).await;
        }
        let boosted: Vec<ChunkResult> = hits
            .iter()
            .filter_map(|hit| {
                let c = chunks.get(hit.payload)?;
                let mut c = c.clone();
                c.score = hit.score;
                c.channel = hit.channel.clone();
                Some(c)
            })
            .collect();
        let mut chunks = boosted;
        chunks.truncate(VECTOR_CHANNEL_LIMIT);

        // 3. Identify top 3 distinct files for enrichment.
        let mut unique_paths: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in &chunks {
            if !c.path.is_empty() && seen.insert(c.path.clone()) {
                unique_paths.push(c.path.clone());
            }
        }
        let top3: std::collections::HashSet<String> =
            unique_paths.iter().take(3).cloned().collect();

        // 4. Enrich commit_chunk hits with a representative changed path.
        let commit_hashes: Vec<String> = chunks
            .iter()
            .filter(|c| {
                c.source_kind == "commit_chunk" && !c.commit_hash.is_empty() && c.path.is_empty()
            })
            .map(|c| c.commit_hash.clone())
            .collect();
        let changed_paths =
            commit_meaning::load_commit_changed_paths(self.db.as_ref(), &commit_hashes)
                .await
                .unwrap_or_default(); // non-fatal: ranking works without path labels
        for chunk in chunks.iter_mut() {
            if chunk.source_kind == "commit_chunk" && chunk.path.is_empty() {
                if let Some(p) = changed_paths.get(&chunk.commit_hash) {
                    chunk.path = p.clone();
                }
            }
        }

        // 5. Enrich and build results.
        let mut context_cache: HashMap<String, GraphContext> = HashMap::new();
        let mut final_results: Vec<SearchResult> = Vec::new();
        for c in chunks {
            let mut is_current = c.hash == c.current_hash;
            // Legacy data: empty current_hash → current for non-commit kinds.
            if c.current_hash.is_empty() && c.source_kind != "commit_chunk" {
                is_current = true;
            }
            if c.source_kind == "commit_chunk" {
                is_current = false; // historical meaning, never HEAD source
            }
            if c.source_kind == "memory" {
                is_current = true; // filtered to current already
            }
            let content = if c.source_kind == "commit_chunk" {
                commit_meaning::truncate_commit_meaning(&c.content)
            } else {
                c.content.clone()
            };

            let mut result = SearchResult {
                id: c.id,
                path: c.path.clone(),
                score: c.score,
                content,
                is_current,
                source_kind: c.source_kind,
                channel: c.channel,
                commit_hash: c.commit_hash,
                status: c.status,
                kind: c.kind,
                context: ContextData {
                    related_issues: Vec::new(),
                    commits: Vec::new(),
                    expert_authors: Vec::new(),
                },
                as_of_ref: String::new(),
                content_source: String::new(),
            };

            if top3.contains(&c.path) {
                let gctx = match context_cache.get(&c.path) {
                    Some(cached) => Some(cached.clone()),
                    None => match graph::get_file_context(db, &c.path).await {
                        Ok(ctx) => {
                            context_cache.insert(c.path.clone(), ctx.clone());
                            Some(ctx)
                        }
                        Err(_) => None,
                    },
                };
                if let Some(ctx) = gctx {
                    result.context.related_issues = ctx.issues;
                    // Aggregate authors (experts).
                    let mut seen_authors = std::collections::HashSet::new();
                    for commit in &ctx.commits {
                        if !commit.author.is_empty() && commit.author != "Unknown" {
                            seen_authors.insert(commit.author.clone());
                        }
                    }
                    result.context.commits = ctx.commits;
                    result.context.expert_authors = seen_authors.into_iter().collect();
                }
            }
            final_results.push(result);
        }

        Ok(final_results)
    }

    /// `lookupCurrentFileHashByPath — see `memory::lifecycle`.
    pub(crate) async fn lookup_current_file_hash_by_path(
        &self,
        path: &str,
    ) -> Result<(String, bool)> {
        crate::memory::lifecycle::lookup_current_file_hash_by_path(self, path).await
    }
}

/// `float32To64 — embedding vectors returned by
/// [`AIProvider::embed_text`] become the f64 shape the typed-memory writes
/// carry.
pub(crate) fn float32_to_f64(vec: &[f32]) -> Vec<f64> {
    vec.iter().map(|v| f64::from(*v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ai, obj, rows_of, FakeAI, FakeDb};
    use ibis_ai::types::{Candidate, Content, GenerationConfig, Part};
    use ibis_db::value::Value;

    fn service(db: Arc<FakeDb>, ai: Option<Arc<dyn ibis_ai::AIProvider>>) -> SearchService {
        let mut svc = SearchService::new(Some(db), ai);
        svc.personal_claims_policy = String::new();
        svc
    }

    /// `TestAskProject_FiltersNonCurrentMemoryAndQueriesCommitChunk.
    #[tokio::test]
    async fn ask_filters_non_current_memory_and_queries_commit_chunk() {
        let fake = Arc::new(FakeDb::new(vec![
            (
                "FROM file_chunk",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("file_chunk:1")),
                    ("path", Value::from_t("/src/a.go")),
                    ("content", Value::from_t("code")),
                    ("hash", Value::from_t("h1")),
                    ("current_hash", Value::from_t("h1")),
                    ("score", Value::from_t(0.9f64)),
                ])]),
            ),
            (
                "FROM commit_chunk",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("commit_chunk:1")),
                    (
                        "content",
                        Value::from_t("COMMIT MESSAGE:\nfix\n\nPATCH:\n-diff"),
                    ),
                    ("commit_hash", Value::from_t("abc1234")),
                    ("score", Value::from_t(0.85f64)),
                ])]),
            ),
            (
                "FROM memory",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("memory:1")),
                    ("content", Value::from_t("team rule")),
                    ("status", Value::from_t("current")),
                    ("kind", Value::from_t("rule")),
                    ("score", Value::from_t(0.8f64)),
                ])]),
            ),
        ]));
        let svc = service(fake.clone(), Some(fake_ai()));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "why auth")
            .await
            .expect("ask_project");

        let calls = fake.execute_calls.lock().unwrap().clone();
        let smart = fake.smart_calls.lock().unwrap().clone();
        let all: Vec<String> = calls.iter().chain(smart.iter()).cloned().collect();
        let memory_sql = all
            .iter()
            .find(|q| q.contains("FROM memory"))
            .expect("memory query")
            .clone();
        for want in ["status = 'current'", "valid_until", "visibility = 'team'"] {
            assert!(
                memory_sql.contains(want),
                "memory query missing {want}: {memory_sql}"
            );
        }
        let commit_sql = all
            .iter()
            .find(|q| q.contains("FROM commit_chunk"))
            .expect("commit query")
            .clone();
        assert!(
            commit_sql.contains("commit_date"),
            "commit_chunk query with commit_date: {commit_sql}"
        );

        let mut saw_commit = false;
        let mut saw_file = false;
        for r in &results {
            if r.source_kind == "commit_chunk" {
                saw_commit = true;
                assert!(!r.is_current, "commit_chunk must not be is_current");
                assert_eq!(r.commit_hash, "abc1234");
            }
            if r.source_kind == "file_chunk" {
                saw_file = true;
                assert!(r.is_current, "matching HEAD file_chunk should be current");
            }
        }
        assert!(saw_commit && saw_file, "expected file and commit hits");
    }

    /// `TestAskProject: multi-channel merge, top-3 context enrichment,
    /// issue weights and expert authors.
    #[tokio::test]
    async fn ask_project_merges_channels_and_enriches_top3() {
        let chunks: Vec<Value> = vec![
            obj(vec![
                ("id", Value::from_t("chunk:1")),
                ("path", Value::from_t("/src/main.go")),
                ("content", Value::from_t("func main()")),
                ("score", Value::from_t(0.9f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("chunk:2")),
                ("path", Value::from_t("/src/main.go")),
                ("content", Value::from_t("var x = 1")),
                ("score", Value::from_t(0.8f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("chunk:3")),
                ("path", Value::from_t("/src/utils.go")),
                ("content", Value::from_t("func help()")),
                ("score", Value::from_t(0.7f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("chunk:4")),
                ("path", Value::from_t("/src/auth.go")),
                ("content", Value::from_t("func login()")),
                ("score", Value::from_t(0.6f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("chunk:5")),
                ("path", Value::from_t("/src/extra.go")),
                ("content", Value::from_t("func extra()")),
                ("score", Value::from_t(0.5f64)),
            ]),
        ];

        let graph_history = vec![obj(vec![
            ("id", Value::from_t("commit:abc")),
            ("hash", Value::from_t("abc")),
            ("message", Value::from_t("init")),
            ("date", Value::from_t("2024-01-01")),
            ("author", Value::from_t(vec![Value::from_t("me")])),
            (
                "issues",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("1")),
                    ("subject", Value::from_t("Fix")),
                    ("status", Value::from_t("Closed")),
                    ("weight", Value::from_t(vec![Value::from_t(1.2f64)])),
                ])]),
            ),
        ])];

        let change_edges = vec![obj(vec![
            ("in", Value::from_t("commit:abc")),
            ("out", Value::from_t("file:main")),
            ("impact", Value::from_t(0.5f64)),
        ])];

        let graph_response = vec![obj(vec![
            ("history", Value::from_t(graph_history)),
            ("change_edges", Value::from_t(change_edges)),
        ])];

        let fake = Arc::new(FakeDb::new(vec![
            ("FROM file_chunk", rows_of(chunks)),
            ("<-changed", rows_of(graph_response)),
        ]));
        let svc = service(fake, Some(fake_ai()));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "test query")
            .await
            .expect("ask_project");

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].path, "/src/main.go");
        assert!(!results[0].context.expert_authors.is_empty());
        assert_eq!(results[0].context.expert_authors[0], "me");
        assert!(!results[0].context.related_issues.is_empty());
        assert_eq!(results[0].context.related_issues[0].weight, 1.2);

        let extra = results
            .iter()
            .find(|r| r.path == "/src/extra.go")
            .expect("4th file present");
        assert!(
            extra.context.expert_authors.is_empty(),
            "4th file gets no context"
        );
    }

    /// RRF fusion (A2 + F03/D1): ranking follows per-channel ranks and a
    /// document present in MORE channels sums its contributions. The memory row
    /// appears in both its vector channel and the lexical content channel, so it
    /// outranks the file chunk that leads only its vector channel — while a
    /// rank-2 file hit with a much higher cosine score stays below both.
    #[tokio::test]
    async fn ask_project_rrf_fusion_sums_cross_channel_ranks() {
        let file_rows = vec![
            obj(vec![
                ("id", Value::from_t("file_chunk:a")),
                ("path", Value::from_t("/src/a.go")),
                ("content", Value::from_t("func main()")),
                ("score", Value::from_t(0.9f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("file_chunk:b")),
                ("path", Value::from_t("/src/b.go")),
                ("content", Value::from_t("func help()")),
                ("score", Value::from_t(0.8f64)),
            ]),
        ];
        let memory_rows = vec![obj(vec![
            ("id", Value::from_t("memory:m1")),
            ("content", Value::from_t("team rule")),
            ("status", Value::from_t("current")),
            ("kind", Value::from_t("rule")),
            ("score", Value::from_t(0.4f64)),
        ])];
        let fake = Arc::new(FakeDb::new(vec![
            // Channel-specific keys: only the intended query matches each
            // registered response.
            ("FROM file_chunk WHERE embedding", rows_of(file_rows)),
            ("FROM memory WHERE embedding", rows_of(memory_rows.clone())),
            (
                "FROM memory WHERE content @1@",
                rows_of(memory_rows.clone()),
            ),
        ]));
        let mut svc = service(fake.clone(), Some(fake_ai()));
        svc.fusion_mode = FusionMode::Rrf;
        let results = svc
            .ask_project(&ActorContext::LOCAL, "test query")
            .await
            .expect("ask_project");

        assert_eq!(results.len(), 3, "one board entry per document");
        let ids: Vec<String> = results.iter().map(|r| r.id.clone()).collect();
        // memory:m1 sums two channel contributions (vector + lexical) and
        // lands first; file_chunk:a leads its single channel.
        assert_eq!(ids, vec!["memory:m1", "file_chunk:a", "file_chunk:b"]);
        let score = |id: &str| results.iter().find(|r| r.id == id).unwrap().score;
        assert!(
            score("memory:m1") > score("file_chunk:a"),
            "cross-channel sum outranks the single-channel lead"
        );
        assert!(
            score("file_chunk:b") < score("file_chunk:a"),
            "rank-2 stays below rank-1 despite its higher cosine score"
        );
        // The best-contributing channel attribution (F03/D1 diagnostics).
        let channel = |id: &str| results.iter().find(|r| r.id == id).unwrap().channel.clone();
        assert_eq!(channel("file_chunk:a"), "file_chunk");
        // m1 leads with its vector channel (rank-1 tie with the lexical
        // content channel resolves to the first channel in order).
        assert_eq!(channel("memory:m1"), "memory");

        // The file_chunk lexical channel ran too (F06/D3).
        let all: Vec<String> = fake
            .smart_calls
            .lock()
            .unwrap()
            .iter()
            .chain(fake.execute_calls.lock().unwrap().iter())
            .cloned()
            .collect();
        assert!(
            all.iter()
                .any(|q| q.contains("FROM file_chunk WHERE content @1@")),
            "file_chunk_text channel must be part of ask_project: {all:?}"
        );
    }

    /// RRF attribution (F03/D1 diagnostics): the best-contributing channel is
    /// the one with the strongest RANK (rank 1 beats rank 2), not the first
    /// channel in query order — per-channel scores are not comparable units and
    /// their `.max(1.0)` clamp would flatten every strength to 1.0.
    #[tokio::test]
    async fn ask_project_rrf_best_channel_follows_rank_not_query_order() {
        // Vector channel: `a` at rank 1, `b` at rank 2. Lexical channel: `b`
        // at rank 1 (and `a` absent). If attribution followed query order, `b`
        // would be credited to the vector channel it ranks second in.
        let file_rows = vec![
            obj(vec![
                ("id", Value::from_t("file_chunk:a")),
                ("path", Value::from_t("/src/a.go")),
                ("content", Value::from_t("func main()")),
                ("score", Value::from_t(0.9f64)),
            ]),
            obj(vec![
                ("id", Value::from_t("file_chunk:b")),
                ("path", Value::from_t("/src/b.go")),
                ("content", Value::from_t("func help()")),
                ("score", Value::from_t(0.8f64)),
            ]),
        ];
        let lexical_rows = vec![obj(vec![
            ("id", Value::from_t("file_chunk:b")),
            ("path", Value::from_t("/src/b.go")),
            ("content", Value::from_t("func help()")),
            ("score", Value::from_t(5.0f64)),
        ])];
        let fake = Arc::new(FakeDb::new(vec![
            ("FROM file_chunk WHERE embedding", rows_of(file_rows)),
            ("FROM file_chunk WHERE content @1@", rows_of(lexical_rows)),
        ]));
        let mut svc = service(fake, Some(fake_ai()));
        svc.fusion_mode = FusionMode::Rrf;
        let results = svc
            .ask_project(&ActorContext::LOCAL, "test query")
            .await
            .expect("ask_project");

        let score = |id: &str| results.iter().find(|r| r.id == id).unwrap().score;
        let channel = |id: &str| results.iter().find(|r| r.id == id).unwrap().channel.clone();
        // `b` sums two channel contributions (vector rank-2 + lexical rank-1)
        // and outranks `a` (vector rank-1 only).
        assert!(
            score("file_chunk:b") > score("file_chunk:a"),
            "cross-channel ranks sum: {} vs {}",
            score("file_chunk:b"),
            score("file_chunk:a")
        );
        // Attribution follows the strongest rank: `b`'s lexical rank-1 beats
        // its vector rank-2, even though the vector channel runs first.
        assert_eq!(
            channel("file_chunk:b"),
            "file_chunk_text",
            "rank-1 in the later channel must outrank rank-2 in the first channel"
        );
        assert_eq!(
            channel("file_chunk:a"),
            "file_chunk",
            "single-channel documents keep their own channel"
        );
    }

    /// Additive fusion (F03/D1 + F04/D2): the same document in two channels
    /// sums its calibrated channel scores; raw BM25 units are calibrated onto
    /// the blend scale (bounded, monotone) so a weak lexical match cannot
    /// ride a large unit and a strong one stays comparable to cosine scores.
    #[tokio::test]
    async fn ask_project_additive_sums_cross_channel_scores() {
        let file_rows = vec![obj(vec![
            ("id", Value::from_t("file_chunk:a")),
            ("path", Value::from_t("/src/a.go")),
            ("content", Value::from_t("func main()")),
            ("score", Value::from_t(0.9f64)),
        ])];
        let lexical_rows = vec![
            obj(vec![
                ("id", Value::from_t("file_chunk:weak")),
                ("path", Value::from_t("/src/weak.go")),
                ("content", Value::from_t("weak lexical body")),
                ("score", Value::from_t(0.3f64)), // weak raw BM25
            ]),
            obj(vec![
                ("id", Value::from_t("file_chunk:strong")),
                ("path", Value::from_t("/src/strong.go")),
                ("content", Value::from_t("strong lexical body")),
                ("score", Value::from_t(50.0f64)), // raw BM25 scale
            ]),
        ];
        let fake = Arc::new(FakeDb::new(vec![
            ("FROM file_chunk WHERE embedding", rows_of(file_rows)),
            ("FROM file_chunk WHERE content @1@", rows_of(lexical_rows)),
        ]));
        let svc = service(fake, Some(fake_ai()));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "test query")
            .await
            .expect("ask_project");

        let score = |id: &str| {
            results
                .iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("{id} in board"))
                .score
        };
        // Calibrated lexical scores stay bounded (raw BM25 50 ≈ 0.94).
        assert!(
            score("file_chunk:strong") <= 1.0 + 1e-9,
            "calibrated lexical score stays bounded: {}",
            score("file_chunk:strong")
        );
        // A weak lexical match rides no unit: it lands below the strong
        // vector hit.
        assert!(
            score("file_chunk:weak") < score("file_chunk:a"),
            "weak lexical ({}) must stay below the strong vector ({})",
            score("file_chunk:weak"),
            score("file_chunk:a")
        );
        // A strong lexical match outranks the weak one — order follows the
        // calibrated scale, not the raw units.
        assert!(score("file_chunk:strong") > score("file_chunk:weak"));
        // Both lexical hits are one board entry each, with their own
        // channels and the public kind unchanged.
        let strong = results
            .iter()
            .find(|r| r.id == "file_chunk:strong")
            .expect("strong lexical hit on the board");
        assert_eq!(strong.source_kind, "file_chunk", "public kind unchanged");
        assert_eq!(strong.channel, "file_chunk_text");
    }

    /// Degraded mode (D8): no DB → ask fails with the original sentinel message.
    #[tokio::test]
    async fn ask_project_without_db_errors() {
        let svc = SearchService::new(None, Some(fake_ai()));
        let err = svc
            .ask_project(&ActorContext::LOCAL, "q")
            .await
            .expect_err("no db");
        assert!(err.to_string().contains("database not connected"));
    }

    /// Raw query passthrough.
    #[tokio::test]
    async fn raw_query_executes() {
        let fake = Arc::new(FakeDb::default());
        let svc = service(fake.clone(), None);
        let _ = svc.raw_query("SELECT 1;").await.expect("raw");
        assert_eq!(fake.execute_calls.lock().unwrap()[0], "SELECT 1;");
    }

    // --- A4: query expansion --------------------------------------------------

    #[test]
    fn merge_channel_pass_keeps_max_per_row() {
        fn chunk(id: &str, score: f64) -> ChunkResult {
            ChunkResult {
                id: id.into(),
                path: String::new(),
                content: String::new(),
                score,
                hash: String::new(),
                current_hash: String::new(),
                source_kind: "file_chunk".into(),
                channel: "file_chunk".into(),
                commit_hash: String::new(),
                status: String::new(),
                kind: String::new(),
                age_days: None,
                at_head: false,
            }
        }
        let mut rows = vec![chunk("a", 0.5), chunk("b", 0.2)];
        let mut index: HashMap<String, usize> = [("a".to_string(), 0), ("b".to_string(), 1)]
            .into_iter()
            .collect();
        // Variant pass strengthens `a`, introduces `c`, and cannot demote `b`.
        merge_channel_pass(&mut rows, &mut index, chunk("a", 0.9));
        merge_channel_pass(&mut rows, &mut index, chunk("c", 0.4));
        merge_channel_pass(&mut rows, &mut index, chunk("b", 0.1));
        assert_eq!(rows.len(), 3, "exactly one entry per id");
        assert!((rows[0].score - 0.9).abs() < 1e-12, "max wins for a");
        assert!(
            (rows[1].score - 0.2).abs() < 1e-12,
            "weaker pass never demotes"
        );
        assert!(
            (rows[2].score - 0.4).abs() < 1e-12,
            "variant-only row joins"
        );
    }

    /// A4: with the symbol-variant expansion on and a linked symbol resolved,
    /// the query embeds twice (primary + variant) and the vector channel runs
    /// two passes; the lexical channel keeps the literal query.
    #[tokio::test]
    async fn ask_project_query_expansion_runs_second_vector_pass() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let calls = embed_calls.clone();
        let ai: Arc<dyn AIProvider> = Arc::new(FakeAI {
            embed: Some(Box::new(move |_text: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                vec![0.1, 0.2, 0.3]
            })),
            generate: None,
        });
        let fake = Arc::new(FakeDb::new(vec![
            (
                "FROM file_chunk WHERE embedding",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("file_chunk:1")),
                    ("path", Value::from_t("/src/calc.go")),
                    ("content", Value::from_t("price calculator logic")),
                    ("hash", Value::from_t("h1")),
                    ("current_hash", Value::from_t("h1")),
                    ("score", Value::from_t(0.9f64)),
                ])]),
            ),
            (
                "FROM symbol",
                rows_of(vec![obj(vec![("name", Value::from_t("pricecalculator"))])]),
            ),
        ]));
        let svc = service(fake.clone(), Some(ai));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "price calculator PriceCalculator")
            .await
            .expect("ask_project");

        assert_eq!(
            embed_calls.load(Ordering::SeqCst),
            2,
            "symbol-variant expansion embeds primary + variant"
        );
        let smart = fake.smart_calls.lock().unwrap();
        let vector_passes = smart
            .iter()
            .filter(|q| q.contains("FROM file_chunk WHERE embedding"))
            .count();
        assert_eq!(vector_passes, 2, "vector channel runs primary + variant");
        let lexical_passes = smart
            .iter()
            .filter(|q| q.contains("FROM file_chunk WHERE content @1@"))
            .count();
        drop(smart);
        assert_eq!(lexical_passes, 1, "lexical channel keeps the literal query");
        assert!(
            results.iter().any(|r| r.id == "file_chunk:1"),
            "merged board still contains the row"
        );
    }

    /// A4 fast path: no linked symbols → single embedding, single pass
    /// (zero extra cost, L3 edge case).
    #[tokio::test]
    async fn ask_project_query_expansion_fast_path_without_symbols() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let calls = embed_calls.clone();
        let ai: Arc<dyn AIProvider> = Arc::new(FakeAI {
            embed: Some(Box::new(move |_text: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                vec![0.1, 0.2, 0.3]
            })),
            generate: None,
        });
        let fake = Arc::new(FakeDb::new(vec![(
            "FROM file_chunk WHERE embedding",
            rows_of(vec![obj(vec![
                ("id", Value::from_t("file_chunk:1")),
                ("path", Value::from_t("/src/a.go")),
                ("content", Value::from_t("plain body")),
                ("score", Value::from_t(0.9f64)),
            ])]),
        )]));
        let svc = service(fake.clone(), Some(ai));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "plain words only")
            .await
            .expect("ask_project");
        assert_eq!(
            embed_calls.load(Ordering::SeqCst),
            1,
            "no linked symbols → no second embedding"
        );
        let vector_passes = fake
            .smart_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("FROM file_chunk WHERE embedding"))
            .count();
        assert_eq!(vector_passes, 1, "single pass on the fast path");
        assert!(results.iter().any(|r| r.id == "file_chunk:1"));
    }

    /// A4 off mode: even with linked symbols resolved, expansion stays off.
    #[tokio::test]
    async fn ask_project_query_expansion_off_keeps_single_pass() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let calls = embed_calls.clone();
        let ai: Arc<dyn AIProvider> = Arc::new(FakeAI {
            embed: Some(Box::new(move |_text: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                vec![0.1, 0.2, 0.3]
            })),
            generate: None,
        });
        let fake = Arc::new(FakeDb::new(vec![
            (
                "FROM file_chunk WHERE embedding",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("file_chunk:1")),
                    ("path", Value::from_t("/src/calc.go")),
                    ("content", Value::from_t("price calculator logic")),
                    ("score", Value::from_t(0.9f64)),
                ])]),
            ),
            (
                "FROM symbol",
                rows_of(vec![obj(vec![("name", Value::from_t("pricecalculator"))])]),
            ),
        ]));
        let mut svc = service(fake.clone(), Some(ai));
        svc.query_expansion = QueryExpansionMode::Off;
        let results = svc
            .ask_project(&ActorContext::LOCAL, "price calculator PriceCalculator")
            .await
            .expect("ask_project");
        assert_eq!(
            embed_calls.load(Ordering::SeqCst),
            1,
            "off mode never embeds the variant"
        );
        let vector_passes = fake
            .smart_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|q| q.contains("FROM file_chunk WHERE embedding"))
            .count();
        assert_eq!(vector_passes, 1);
        assert!(results.iter().any(|r| r.id == "file_chunk:1"));
    }

    /// A4 fail-open: a variant-embedding failure degrades to the primary pass
    /// without failing the ask.
    #[tokio::test]
    async fn ask_project_query_expansion_variant_failure_fails_open() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct VariantFailsAI(AtomicUsize);
        #[async_trait]
        impl AIProvider for VariantFailsAI {
            fn name(&self) -> &str {
                "variant-fails"
            }
            async fn is_functional(&self) -> bool {
                true
            }
            async fn embed_text(
                &self,
                _text: &str,
            ) -> std::result::Result<Vec<f32>, ibis_ai::AiError> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                if n > 0 {
                    return Err(ibis_ai::AiError::other("variant embedding down"));
                }
                Ok(vec![0.1, 0.2, 0.3])
            }
        }
        let fake = Arc::new(FakeDb::new(vec![
            (
                "FROM file_chunk WHERE embedding",
                rows_of(vec![obj(vec![
                    ("id", Value::from_t("file_chunk:1")),
                    ("path", Value::from_t("/src/calc.go")),
                    ("content", Value::from_t("price calculator logic")),
                    ("score", Value::from_t(0.9f64)),
                ])]),
            ),
            (
                "FROM symbol",
                rows_of(vec![obj(vec![("name", Value::from_t("pricecalculator"))])]),
            ),
        ]));
        let svc = service(fake, Some(Arc::new(VariantFailsAI(AtomicUsize::new(0)))));
        let results = svc
            .ask_project(&ActorContext::LOCAL, "price calculator PriceCalculator")
            .await
            .expect("primary pass survives the variant failure");
        assert!(
            results.iter().any(|r| r.id == "file_chunk:1"),
            "ask still returns the primary board"
        );
    }

    // --- A3: post-fusion reranker -------------------------------------------

    /// Six one-channel file hits, fused scores descending (0.9..0.4): enough
    /// rows to clear the rerank floor, one channel so base_norm math is
    /// transparent.
    fn rerank_board() -> Vec<Value> {
        let ids = ["a", "b", "c", "d", "e", "f"];
        let scores = [0.9f64, 0.8, 0.7, 0.6, 0.5, 0.4];
        ids.iter()
            .zip(scores)
            .map(|(id, score)| {
                obj(vec![
                    ("id", Value::from_t(format!("file_chunk:{id}"))),
                    ("path", Value::from_t(format!("/src/{id}.go"))),
                    ("content", Value::from_t(format!("body of {id}"))),
                    ("hash", Value::from_t("h1")),
                    ("current_hash", Value::from_t("h1")),
                    ("score", Value::from_t(score)),
                ])
            })
            .collect()
    }

    /// A3 on: the judged relevance reorders the fused board (blend
    /// `base_norm*0.4 + llm*0.6`) — the leader drops with score 0, the tail
    /// rises with score 1.
    #[tokio::test]
    async fn ask_project_rerank_reorders_the_fused_board() {
        let fake = Arc::new(FakeDb::new(vec![(
            "FROM file_chunk WHERE embedding",
            rows_of(rerank_board()),
        )]));
        let ai: Arc<dyn AIProvider> = FakeAI::with_generate(|_contents| {
            Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![Part {
                    text: r#"[{"index": 0, "score": 0.0}, {"index": 2, "score": 0.5}, {"index": 5, "score": 1.0}]"#
                        .into(),
                }],
            },
            finish_reason: "STOP".into(),
            usage_metadata: None,
        }
        });
        let mut svc = service(fake, Some(ai));
        svc.rerank_mode = RerankMode::Local;
        let results = svc
            .ask_project(&ActorContext::LOCAL, "anything")
            .await
            .expect("ask_project");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        // base_norms (max 0.9): a 1.0, b .925, c .849, d .774, e .699, f .624.
        // Scored: a→0.400 (llm 0), c→0.640 (llm .5), f→0.849 (llm 1).
        // Unscored keep their base_norm: b .925, d .774, e .699.
        assert_eq!(
            ids,
            vec![
                "file_chunk:b",
                "file_chunk:f",
                "file_chunk:d",
                "file_chunk:e",
                "file_chunk:c",
                "file_chunk:a",
            ]
        );
    }

    /// A3 fail-open integration: the scoring call fails, the ask keeps the
    /// fused order and still succeeds.
    #[tokio::test]
    async fn ask_project_rerank_failure_keeps_fused_order() {
        struct RerankFailsAI;
        #[async_trait]
        impl AIProvider for RerankFailsAI {
            fn name(&self) -> &str {
                "rerank-fails"
            }
            async fn is_functional(&self) -> bool {
                true
            }
            async fn embed_text(
                &self,
                _text: &str,
            ) -> std::result::Result<Vec<f32>, ibis_ai::AiError> {
                Ok(vec![0.1, 0.2, 0.3])
            }
            async fn generate_content(
                &self,
                _contents: &[Content],
                _config: GenerationConfig,
            ) -> std::result::Result<Candidate, ibis_ai::AiError> {
                Err(ibis_ai::AiError::other("judge down"))
            }
        }
        let fake = Arc::new(FakeDb::new(vec![(
            "FROM file_chunk WHERE embedding",
            rows_of(rerank_board()),
        )]));
        let mut svc = service(fake, Some(Arc::new(RerankFailsAI)));
        svc.rerank_mode = RerankMode::Local;
        let results = svc
            .ask_project(&ActorContext::LOCAL, "anything")
            .await
            .expect("ask survives the rerank failure");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "file_chunk:a",
                "file_chunk:b",
                "file_chunk:c",
                "file_chunk:d",
                "file_chunk:e",
                "file_chunk:f",
            ],
            "fused order preserved"
        );
    }

    /// A3 off (default): no scoring call, fused order intact.
    #[tokio::test]
    async fn ask_project_rerank_off_never_generates() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let generate_calls = Arc::new(AtomicUsize::new(0));
        struct CountingAI {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl AIProvider for CountingAI {
            fn name(&self) -> &str {
                "counting"
            }
            async fn is_functional(&self) -> bool {
                true
            }
            async fn embed_text(
                &self,
                _text: &str,
            ) -> std::result::Result<Vec<f32>, ibis_ai::AiError> {
                Ok(vec![0.1, 0.2, 0.3])
            }
            async fn generate_content(
                &self,
                _contents: &[Content],
                _config: GenerationConfig,
            ) -> std::result::Result<Candidate, ibis_ai::AiError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ibis_ai::AiError::other("should never be called"))
            }
        }
        let fake = Arc::new(FakeDb::new(vec![(
            "FROM file_chunk WHERE embedding",
            rows_of(rerank_board()),
        )]));
        let svc = service(
            fake,
            Some(Arc::new(CountingAI {
                calls: generate_calls.clone(),
            })),
        );
        let results = svc
            .ask_project(&ActorContext::LOCAL, "anything")
            .await
            .expect("ask_project");
        assert_eq!(generate_calls.load(Ordering::SeqCst), 0, "no judge call");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.first(), Some(&"file_chunk:a"));
    }
}
