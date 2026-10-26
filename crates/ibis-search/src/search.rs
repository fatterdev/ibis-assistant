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
use crate::retrieval_boost::{
    apply_multi_signal_boost, resolve_linked_symbols, tokenize_query, ScoredHit,
};
use crate::rows;
use crate::ActorContext;

/// Per-table hit budget for the vector channels.
pub(crate) const VECTOR_CHANNEL_LIMIT: usize = 15;

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
    commit_hash: String,
    status: String,
    kind: String,
}

impl ChunkResult {
    fn from_row(row: &ibis_db::value::Object, source_kind: &str) -> Self {
        ChunkResult {
            id: rows::record_id(rows::field(row, "id")),
            source_kind: source_kind.to_string(),
            content: rows::scalar_string(rows::field(row, "content")),
            path: rows::scalar_string(rows::field(row, "path")),
            hash: rows::scalar_string(rows::field(row, "hash")),
            current_hash: rows::scalar_string(rows::field(row, "current_hash")),
            commit_hash: rows::scalar_string(rows::field(row, "commit_hash")),
            status: rows::scalar_string(rows::field(row, "status")),
            kind: rows::scalar_string(rows::field(row, "kind")),
            score: rows::numeric_float(rows::field(row, "score")),
        }
    }
}

/// Vector-similarity + created_at time-decay score expression shared by the
/// four channels. The embedding binds as `$vec` (F-002). `math::max` takes a
/// single array on SurrealDB 3.x (D17) — the original 2.x two-arg form fails live.
pub(crate) fn ask_score_expr() -> String {
    "(vector::similarity::cosine(embedding, $vec) * 0.7) + \
     (IF created_at IS NONE THEN 0.3 ELSE math::max([0, 1 - duration::days(time::now() - created_at) / 365]) * 0.3 END)"
        .to_string()
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
        let vec = ai
            .embed_text(query)
            .await
            .map_err(|e| SearchError::wrap("embedding failed", e))?;

        cost_tracker(self)
            .record_embedding_usage("system:search_vector", query)
            .await;

        let vec_json = commit_meaning::vec_value(&vec);
        let score_expr = ask_score_expr();

        let trusted_actor = actor.resolve_trusted_actor("");
        let (memory_visibility, memory_vars) = ask_memory_visibility_sql(&trusted_actor);
        let memory_where = format!(
            "embedding != NONE AND {} AND {}",
            memory_currentness_sql(),
            memory_visibility
        );

        struct TableQuery<'a> {
            name: &'a str,
            sql: String,
        }
        let queries = [
            TableQuery {
                name: "file_chunk",
                sql: format!(
                    "SELECT id, file.path as path, content, hash, file.hash as current_hash, {score_expr} as score FROM file_chunk WHERE embedding != NONE ORDER BY score DESC LIMIT {VECTOR_CHANNEL_LIMIT};"
                ),
            },
            TableQuery {
                name: "commit_chunk",
                sql: commit_meaning::commit_chunk_ask_query(),
            },
            TableQuery {
                name: "memory",
                sql: format!(
                    "SELECT id, NONE as path, content, status, kind, NONE as hash, NONE as current_hash, {score_expr} as score FROM memory WHERE {memory_where} ORDER BY score DESC LIMIT {VECTOR_CHANNEL_LIMIT};"
                ),
            },
            TableQuery {
                name: "reasoning",
                sql: format!(
                    "SELECT id, NONE as path, string::concat(question OR '', '\n', outcome OR '') as content, NONE as hash, NONE as current_hash, {score_expr} as score FROM reasoning WHERE embedding != NONE ORDER BY score DESC LIMIT {VECTOR_CHANNEL_LIMIT};"
                ),
            },
        ];

        let mut chunks: Vec<ChunkResult> = Vec::new();
        for tq in &queries {
            let mut vars: Vars = memory_vars.iter().cloned().collect();
            vars.insert("vec".into(), vec_json.clone());
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
                let c = ChunkResult::from_row(row, tq.name);
                if c.id.is_empty() && c.content.is_empty() {
                    continue;
                }
                chunks.push(c);
            }
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
                        chunk.score += 0.05;
                    }
                }
            }
        }

        let (keywords, entities) = tokenize_query(query);
        let linked_symbols = resolve_linked_symbols(self.db.as_ref(), &entities).await;
        let mut hits: Vec<ScoredHit> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| ScoredHit {
                text: c.content.clone(),
                path: c.path.clone(),
                tags: Vec::new(),
                score: c.score,
                payload: i,
            })
            .collect();
        apply_multi_signal_boost(&mut hits, &keywords, &entities, &linked_symbols);
        let boosted: Vec<ChunkResult> = hits
            .iter()
            .filter_map(|hit| {
                let c = chunks.get(hit.payload)?;
                let mut c = c.clone();
                c.score = hit.score;
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
    use crate::test_support::{fake_ai, obj, rows_of, FakeDb};
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
}
