//! The RAFT self-optimization loop.

use std::sync::Arc;

use threski_ai::{types::GenerationConfig, AIProvider};
use threski_db::executor::Executor;
use threski_db::value::{SurrealValue, Value};
use threski_search::search::SearchService;

use crate::store::DbCostStore;

/// The maximum snippet bytes fed to the QA-generation prompt
/// (`GenerateSyntheticQA 3000-char truncation).
const MAX_QA_SNIPPET: usize = 3000;

/// Resolves (working repo root, anchor_mode) for a project so consolidated
/// git-eligible claims can be committed to `.threski/claims/` (claim store 4b).
/// `None` keeps consolidation Surreal-only.
pub type WorkspaceResolver = Arc<dyn Fn(&str) -> (String, String) + Send + Sync>;

/// Manages the Retrieval-Augmented Fine-Tuning (RAFT) self-optimization loop
/// and the sleeptime memory-consolidation pass.
pub struct Optimizer {
    pub db: Option<Arc<dyn Executor>>,
    pub ai: Option<Arc<dyn AIProvider>>,
    pub search: Option<Arc<SearchService>>,

    /// Optional project → (repo root, anchor mode) resolver (claim store 4b).
    pub workspace: Option<WorkspaceResolver>,
}

impl Optimizer {
    /// `NewOptimizer. `search` is the memory service; when `None` the
    /// consolidation pass builds one over the same db/ai ( constructed
    /// `&search.Service{DB, AI}` inline).
    pub fn new(
        db: Option<Arc<dyn Executor>>,
        ai: Option<Arc<dyn AIProvider>>,
        search: Option<Arc<SearchService>>,
    ) -> Self {
        Optimizer {
            db,
            ai,
            search,
            workspace: None,
        }
    }

    /// `GenerateSyntheticQA uses the LLM to create a question that the
    /// given code chunk answers.
    pub async fn generate_synthetic_qa(&self, chunk_content: &str) -> Result<String, String> {
        let ai = self
            .ai
            .as_ref()
            .ok_or_else(|| "AI provider not configured".to_string())?;
        // Truncate the chunk if too large to avoid token waste.
        let mut snippet = chunk_content.to_string();
        if snippet.chars().count() > MAX_QA_SNIPPET {
            let truncated: String = snippet.chars().take(MAX_QA_SNIPPET).collect();
            snippet = format!("{truncated}...(truncated)");
        }

        let prompt = format!(
            "You are a technical examiner.\nI will provide a code snippet. Your task is to generate a specific, technical question that a developer would ask, which is uniquely answered by this snippet.\nThe question should not mention the file name or variable names directly unless they are critical concepts.\n\nSNIPPET:\n{snippet}\n\nOUTPUT FORMAT:\nQUESTION: <your question>\n"
        );

        let contents = [threski_ai::Content {
            role: "user".to_string(),
            parts: vec![threski_ai::Part { text: prompt }],
        }];
        let resp = ai
            .generate_content(
                &contents,
                GenerationConfig {
                    temperature: 0.7,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| e.to_string())?;

        if let Some(usage) = &resp.usage_metadata {
            if let Some(db) = &self.db {
                let tracker = threski_ai::CostTracker::new(Some(
                    Arc::new(DbCostStore(db.clone())) as Arc<dyn threski_ai::CostStore>
                ));
                tracker
                    .record_usage(
                        "system:optimizer_raft",
                        usage.prompt_token_count as u64,
                        usage.candidates_token_count as u64,
                    )
                    .await;
            }
        }

        let text: String = resp.content.parts.iter().map(|p| p.text.as_str()).collect();

        // Parse.
        if let Some(idx) = text.find("QUESTION:") {
            let after = &text[idx + "QUESTION:".len()..];
            return Ok(after.trim().to_string());
        }
        Ok(text.trim().to_string())
    }

    /// `EvaluateRetrieval: runs a search and scores the expected chunk by
    /// rank (1.0 for top 1, 0.5 for top 2, …); 0 when absent.
    pub async fn evaluate_retrieval(
        &self,
        question: &str,
        expected_chunk_id: &str,
    ) -> Result<f64, String> {
        let service = self
            .search
            .as_ref()
            .ok_or_else(|| "search service not configured".to_string())?;
        let actor = threski_search::ActorContext::LOCAL;
        let results = service
            .ask_project(&actor, question)
            .await
            .map_err(|e| e.to_string())?;

        for (i, r) in results.iter().enumerate() {
            if r.id == expected_chunk_id {
                return Ok(1.0 / (i as f64 + 1.0));
            }
        }
        Ok(0.0)
    }

    /// `OptimizeLoop: for a defined number of random chunks, generate a
    /// question, evaluate retrieval, and reinforce valuable connections.
    pub async fn optimize_loop(&self, iterations: usize) -> Result<(), String> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| "database not connected".to_string())?;
        tracing::info!("Starting Optimization Loop for {iterations} iterations...");

        // 1. Select candidates (random).
        let ql = format!("SELECT id, content FROM file_chunk ORDER BY rand() LIMIT {iterations};");
        let outcome = db
            .execute(&ql)
            .await
            .map_err(|e| format!("failed to select chunks: {e}"))?;

        struct Chunk {
            id: String,
            content: String,
        }
        let mut chunks: Vec<Chunk> = Vec::new();
        for row in threski_db::value::decode_row_list::<FileChunkRow>(
            outcome.first().unwrap_or(&Value::None),
            &ql,
        )
        .map_err(|e| format!("failed to parse chunks: {e}"))?
        {
            if let (Some(id), Some(content)) = (row.id, row.content) {
                let id = threski_db::ids::coerce_record_id(&threski_db::value::Value::from_t(id));
                if id.is_empty() || content.trim().is_empty() {
                    continue;
                }
                chunks.push(Chunk { id, content });
            }
        }

        for (i, c) in chunks.iter().enumerate() {
            tracing::debug!(
                "Optimization [{}/{}]: Processing chunk {}",
                i + 1,
                chunks.len(),
                c.id
            );

            // 2. Generate question.
            let question = match self.generate_synthetic_qa(&c.content).await {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!("Optimization: QA gen failed for {}: {e}", c.id);
                    continue;
                }
            };
            if question.trim().is_empty() {
                continue;
            }
            tracing::debug!("Optimization: Generated Question: {question}");

            // 3. Evaluate.
            let score = match self.evaluate_retrieval(&question, &c.id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Optimization: Eval failed: {e}");
                    continue;
                }
            };
            tracing::info!("Optimization: Chunk {} scored {score:.2}", c.id);

            // 4. Reinforce.
            if score > 0.0 {
                let id = c.id.clone();
                if !threski_db::ids::is_safe_record_id(&id) {
                    continue;
                }
                // 3.x counter idiom (D17 R6).
                let ql = format!("UPDATE {id} SET access_count += 1, last_accessed = time::now();");
                if let Err(e) = db.execute(&ql).await {
                    tracing::warn!("Optimization: Reinforce failed: {e}");
                }
            } else {
                tracing::warn!(
                    "Optimization: MISS. Question '{question}' did not retrieve {}.",
                    c.id
                );
            }
        }

        tracing::info!("Optimization Loop Complete.");
        Ok(())
    }
}

/// `SELECT id, content FROM file_chunk …` row (record ids decode from record
/// values and string shapes alike, Q3).
#[derive(Debug, Clone, SurrealValue)]
struct FileChunkRow {
    id: Option<String>,
    content: Option<String>,
}
