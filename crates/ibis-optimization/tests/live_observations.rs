//! Live C8 synthesis pass over the real SurrealDB sidecar: seed two fixes
//! with the same root cause, run [`Optimizer::observations_pass`] with a
//! scripted AI, and pin the row upgrade (`generated_by = llm`, embedding
//! present, synthetic title) plus second-pass idempotence (zero AI calls).
//!
//! Sidecar: `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`,
//! root/root). Each test creates its own scratch database and drops it at the
//! end; when the server is unreachable the test skips gracefully unless
//! `IBIS_LIVE_REQUIRED=1`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use ibis_ai::types::{Candidate, Content, GenerationConfig, Part};
use ibis_ai::{AIProvider, AiError};
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::value::Value;
use ibis_optimization::Optimizer;
use ibis_search::memory::observations::ObservationSettings;
use ibis_search::rows;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";
const NS: &str = "ibis_opt_live_tests";
const PROJECT: &str = "c8_live_synth";

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!(
        "opt_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let exec = match SurrealDbExecutor::connect(&url, NS, &db_name, "root", "root").await {
        Ok(exec) => exec,
        Err(err) => {
            if std::env::var("IBIS_LIVE_REQUIRED").as_deref() == Ok("1") {
                panic!("IBIS_LIVE_REQUIRED=1 but SurrealDB is unreachable: {err}");
            }
            eprintln!("live: SurrealDB not reachable ({err}), skipping");
            return None;
        }
    };
    Some((Arc::new(exec) as Arc<dyn Executor>, db_name))
}

async fn drop_scratch(exec: &dyn Executor, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

/// Scripted synthesis provider: one canned JSON summary, counting calls.
struct ScriptedAi {
    calls: AtomicUsize,
    prompts: Mutex<Vec<String>>,
    route_hints: Mutex<Vec<String>>,
}

#[async_trait]
impl AIProvider for ScriptedAi {
    fn name(&self) -> &str {
        "scripted-observations"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, AiError> {
        // The live schema pins the DiskANN dimension (768).
        Ok(vec![0.1; 768])
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        config: GenerationConfig,
    ) -> Result<Candidate, AiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.route_hints
            .lock()
            .unwrap()
            .push(config.route_hint.clone());
        if let Some(first) = contents.first() {
            if let Some(part) = first.parts.first() {
                self.prompts.lock().unwrap().push(part.text.clone());
            }
        }
        Ok(Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![Part {
                    text: "{\"name\":\"Empty discovery root crashes\",\"summary\":\"Two fixes share the empty discovery root cause.\"}"
                        .to_string(),
                }],
            },
            finish_reason: "STOP".into(),
            usage_metadata: Some(ibis_ai::types::UsageMetadata {
                prompt_token_count: 220,
                candidates_token_count: 40,
                total_token_count: 260,
                cached_content_token_count: None,
            }),
        })
    }
}

async fn seed_fix(exec: &dyn Executor, key: &str, root_cause: &str, title: &str, content: &str) {
    let repo_id = ibis_db::ids::format_record_id("repo", PROJECT);
    let mut vars = Vars::new();
    vars.insert("kind".into(), Value::from_t("fix"));
    vars.insert("title".into(), Value::from_t(title));
    vars.insert("content".into(), Value::from_t(content));
    vars.insert("root_cause".into(), Value::from_t(root_cause));
    let sql = format!(
        "CREATE memory:⟨{key}⟩ SET kind = $kind, title = $title, content = $content, \
         status = 'current', confidence = 0.7, visibility = 'team', owner = 'agent', \
         details.root_cause = $root_cause, project = {repo_id};"
    );
    exec.smart_query(&sql, vars).await.expect("seed fix");
}

async fn derived_row(exec: &dyn Executor) -> (String, String, String, bool, Vec<String>) {
    let sql = "SELECT id, details.generated_by AS generated_by, title, \
               embedding != NONE AS has_embedding, details.members AS members \
               FROM memory WHERE kind = 'observation' AND store = 'derived';";
    let outcome = exec.execute(sql).await.expect("select derived");
    let rows_values = rows::normalize_rows(&outcome).expect("normalize");
    let rows = rows::object_rows(&rows_values);
    assert_eq!(rows.len(), 1, "exactly one derived row: {rows:?}");
    let row = rows[0];
    (
        rows::record_id(rows::field(row, "id")),
        rows::scalar_string(rows::field(row, "generated_by")),
        rows::scalar_string(rows::field(row, "title")),
        matches!(rows::field(row, "has_embedding"), Value::Bool(true)),
        rows::string_slice(rows::field(row, "members")),
    )
}

#[tokio::test]
async fn observations_pass_synthesizes_and_is_idempotent() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    ibis_db::schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    seed_fix(
        exec.as_ref(),
        "c8s_fix_a",
        "empty discovery root",
        "Wizard crashes on empty discovery root",
        "The discovery wizard panicked when the root was empty.",
    )
    .await;
    seed_fix(
        exec.as_ref(),
        "c8s_fix_b",
        "empty discovery root",
        "Empty root breaks the discovery wizard",
        "Guard the discovery root before the wizard step runs.",
    )
    .await;

    let ai = Arc::new(ScriptedAi {
        calls: AtomicUsize::new(0),
        prompts: Mutex::new(Vec::new()),
        route_hints: Mutex::new(Vec::new()),
    });
    let mut optimizer = Optimizer::new(
        Some(exec.clone()),
        Some(ai.clone() as Arc<dyn AIProvider>),
        None,
    );
    optimizer.observations = ObservationSettings {
        enabled: true,
        ..Default::default()
    };

    let report = optimizer
        .observations_pass(PROJECT, None)
        .await
        .expect("pass 1");
    assert_eq!(report.summarized, 1, "{report:?}");
    assert_eq!(ai.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        ai.route_hints.lock().unwrap().as_slice(),
        ["local:M".to_string()]
    );
    let prompt = ai.prompts.lock().unwrap()[0].clone();
    assert!(
        prompt.contains("c8s_fix_a"),
        "member ids in prompt: {prompt}"
    );

    let (id, generated_by, title, has_embedding, members) = derived_row(exec.as_ref()).await;
    assert!(!id.is_empty());
    assert_eq!(generated_by, "llm");
    assert_eq!(title, "Empty discovery root crashes");
    assert!(has_embedding, "content embedded during synthesis");
    assert_eq!(members.len(), 2);

    // The cost bucket received the usage.
    let cost = exec
        .execute("SELECT tokens_used FROM system:memory_observations;")
        .await
        .expect("select cost");
    let cost_rows = rows::normalize_rows(&cost).expect("normalize cost");
    let tokens = rows::object_rows(&cost_rows)
        .first()
        .map(|row| rows::numeric_float(rows::field(row, "tokens_used")) as u64)
        .unwrap_or(0);
    assert!(tokens > 0, "cost bucket populated: {tokens}");

    // Second pass: the evidence hash is unchanged and the row is `llm` +
    // embedded, so the backlog is empty — zero AI calls.
    let report = optimizer
        .observations_pass(PROJECT, None)
        .await
        .expect("pass 2");
    assert_eq!(report.summarized, 0, "{report:?}");
    assert_eq!(report.embedding_retries, 0);
    assert_eq!(ai.calls.load(Ordering::SeqCst), 1, "no second LLM call");

    drop_scratch(exec.as_ref(), &db_name).await;
}
