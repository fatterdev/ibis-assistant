//! F0.5 protocol gate: reasoning via MCP sampling over the rmcp in-memory
//! duplex (the same NDJSON framing stdio uses). A sampling-capable client
//! answers `sampling/createMessage`; the `ask_project` tool's agentic lane
//! reasons through the attached session peer. The second scenario proves the
//! degradation: without the `sampling` capability the chain reports the
//! sampling leg unavailable (the fallback chain moves on).

// The rmcp sampling surface is SEP-2577-deprecated upstream but it is exactly
// what this gate exercises (F0.5 keeps sampling as the primary reasoning via).
#![allow(deprecated)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ibis_ai::{AIProvider, Client, CursorLMProvider, FallbackChain, McpSamplingProvider};
use ibis_core::config::{Config, HarnessProfile};
use ibis_db::value::{Object as DbObject, Value as DbValue};
use ibis_db::{DbError, Executor, QueryOutcome, Vars};
use ibis_search::search::SearchService;
use ibis_server::mcp_server::IbisServer;
use ibis_server::sampling::SamplingSessionRegistry;
use ibis_server::state::AppState;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestParams, CreateMessageResult,
    Implementation, Role, SamplingMessage, SamplingMessageContentBlock,
};
use rmcp::service::{RequestContext, RoleClient, RunningService};
use rmcp::{service::serve_client, ErrorData as McpError, ServiceExt};
use serde_json::json;

/// An MCP client that declares the `sampling` capability and answers
/// `sampling/createMessage` with a canned assistant turn (or the next
/// scripted turn, for multi-step agentic scenarios).
#[derive(Clone)]
struct SamplingTestClient {
    info: ClientConfig,
    calls: Arc<AtomicUsize>,
    last_question: Arc<Mutex<String>>,
    /// Scripted assistant turns consumed in order; empty = the default
    /// single-turn `FINAL ANSWER`.
    script: Arc<Mutex<VecDeque<String>>>,
}

impl ClientHandler for SamplingTestClient {
    #[allow(deprecated)]
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    #[allow(deprecated)]
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let question = params
            .messages
            .first()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        SamplingMessageContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        *self.last_question.lock().unwrap() = question;
        let reply = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "FINAL ANSWER: sampled via mcp".to_string());
        Ok(CreateMessageResult::new(
            SamplingMessage::new(
                Role::Assistant,
                SamplingMessageContentBlock::text(reply.as_str()),
            ),
            "vscode-claude".to_string(),
        ))
    }
}

/// The real server over an in-memory duplex, plus the connected client.
struct TestClient {
    service: RunningService<RoleClient, SamplingTestClient>,
    server: tokio::task::JoinHandle<Result<(), String>>,
    handler: SamplingTestClient,
}

async fn test_state(registry: Arc<SamplingSessionRegistry>) -> Arc<AppState> {
    let cfg = Config {
        port: 3030,
        runtime_mode: "plugin".into(),
        ..Default::default()
    };
    let ai = sampling_chain(registry.clone());
    let db: Option<Arc<dyn ibis_db::executor::Executor>> = None;
    let ticket = ibis_server::assembly::assemble_ticketing(db.clone())
        .await
        .0;
    let repopr = ibis_server::assembly::assemble_repopr(&ticket.config().clone());
    let ai_adapter: Option<Arc<dyn ibis_ai::AIProvider>> =
        Some(Arc::new(ibis_server::state::ClientAIAdapter(ai.clone())));
    Arc::new(AppState {
        cfg: Arc::new(RwLock::new(cfg.clone())),
        db: Arc::new(RwLock::new(db)),
        search: Arc::new(RwLock::new(Arc::new(
            ibis_search::search::SearchService::new(None, ai_adapter),
        ))),
        ai,
        ticket,
        repopr,
        sessions: Mutex::new(std::collections::HashMap::new()),
        ingestion: Arc::new(ibis_ingest_dynamic::queue::ProjectIngestionManager::new(
            Arc::new(cfg),
            None,
        )),
        active_repos: vec![],
        db_process: Mutex::new(None),
        resource_subs: ibis_server::resources::ResourceSubscriptions::default(),
        sampling: registry,
        tasks: std::sync::Mutex::new(None),
        task_shutdown: tokio_util::sync::CancellationToken::new(),
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(false),
    })
}

impl TestClient {
    /// Connects with the sampling-capable handler.
    async fn connect_sampling(state: Arc<AppState>) -> TestClient {
        Self::connect(state, true, Vec::new()).await
    }

    /// Connects with the sampling-capable handler and a scripted sequence of
    /// assistant turns (multi-step agentic scenarios).
    async fn connect_sampling_scripted(state: Arc<AppState>, script: Vec<String>) -> TestClient {
        Self::connect(state, true, script).await
    }

    /// Connects with a default (sampling-less) client handler.
    async fn connect_plain(state: Arc<AppState>) -> TestClient {
        Self::connect(state, false, Vec::new()).await
    }

    async fn connect(state: Arc<AppState>, sampling: bool, script: Vec<String>) -> TestClient {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let service = IbisServer::new(state, false);
            match service.serve(server_stream).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        });
        #[allow(deprecated)]
        let info = if sampling {
            ClientConfig::new(
                ClientCapabilities::builder().enable_sampling().build(),
                Implementation::new("sampling-e2e-client", "0.0.0"),
            )
        } else {
            ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("sampling-e2e-client", "0.0.0"),
            )
        };
        let handler = SamplingTestClient {
            info,
            calls: Arc::new(AtomicUsize::new(0)),
            last_question: Arc::new(Mutex::new(String::new())),
            script: Arc::new(Mutex::new(script.into_iter().collect())),
        };
        let service = match serve_client(handler.clone(), client_stream).await {
            Ok(service) => service,
            Err(e) => {
                let server_result = server.await;
                panic!("client init failed ({e}); server task: {server_result:?}")
            }
        };
        TestClient {
            service,
            server,
            handler,
        }
    }

    fn peer(&self) -> &rmcp::service::Peer<RoleClient> {
        self.service.peer()
    }

    async fn shutdown(self) {
        let _ = self.service.cancel().await;
        match self.server.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("server serve error: {e}"),
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }
}

/// The F0.5 reasoning stack wired the way `assemble_ai` does for
/// `ide_sampling`: the chain `sampling → lm_bridge` (the bridge points at a
/// dead loopback port, so the sampling leg serves the request).
fn sampling_steps(registry: Arc<SamplingSessionRegistry>) -> Vec<Arc<dyn AIProvider>> {
    vec![
        Arc::new(McpSamplingProvider::new(registry)),
        Arc::new(CursorLMProvider::new("http://127.0.0.1:1")),
    ]
}

fn sampling_chain(registry: Arc<SamplingSessionRegistry>) -> Arc<Client> {
    Arc::new(Client::new(
        None,
        Some(Arc::new(FallbackChain::new(
            sampling_steps(registry),
            false,
        ))),
        None,
    ))
}

/// Embedding-only AI for the citation scenario: retrieval needs vectors while
/// the reasoning leg stays the MCP sampling chain.
struct EmbeddingStub;

#[async_trait::async_trait]
impl AIProvider for EmbeddingStub {
    fn name(&self) -> &str {
        "test-embedding"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, ibis_ai::AiError> {
        Ok(vec![0.1, 0.2, 0.3])
    }
}

fn db_obj(pairs: Vec<(&str, DbValue)>) -> DbValue {
    let mut obj = DbObject::new();
    for (key, value) in pairs {
        obj.insert(key, value);
    }
    DbValue::Object(obj)
}

/// Minimal HEAD `file_chunk` fixture: the two channels return the same two
/// current sources, every other statement decodes as an empty result (the
/// same shape the in-crate `FakeDb` serves to the search tests).
struct CitationFixtureDb;

#[async_trait::async_trait]
impl Executor for CitationFixtureDb {
    async fn execute(&self, _sql: &str) -> Result<QueryOutcome, DbError> {
        Ok(QueryOutcome::Single(DbValue::None))
    }

    async fn smart_query(&self, sql: &str, _vars: Vars) -> Result<QueryOutcome, DbError> {
        if !sql.contains("FROM file_chunk") {
            return Ok(QueryOutcome::Single(DbValue::None));
        }
        Ok(QueryOutcome::Single(DbValue::from_t(vec![
            db_obj(vec![
                ("id", DbValue::from_t("file_chunk:init")),
                ("path", DbValue::from_t("/src/init.rs")),
                ("content", DbValue::from_t("fn init() { run(); }")),
                ("hash", DbValue::from_t("h-init")),
                ("current_hash", DbValue::from_t("h-init")),
                ("score", DbValue::from_t(0.9f64)),
            ]),
            db_obj(vec![
                ("id", DbValue::from_t("file_chunk:bootstrap")),
                ("path", DbValue::from_t("/src/bootstrap.rs")),
                ("content", DbValue::from_t("fn bootstrap() { init(); }")),
                ("hash", DbValue::from_t("h-boot")),
                ("current_hash", DbValue::from_t("h-boot")),
                ("score", DbValue::from_t(0.8f64)),
            ]),
        ])))
    }
}

#[tokio::test]
async fn ask_project_reasons_through_mcp_sampling() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry).await;
    let client = TestClient::connect_sampling(state).await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project").with_arguments(
                json!({ "query": "why did init change?" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("ask_project over sampling");
    assert!(
        !result.is_error.unwrap_or(false),
        "ask_project failed: {result:?}"
    );
    let text = match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("sampled via mcp"),
        "agentic answer must carry the sampled text: {text}"
    );
    assert!(text.contains("\"answer\""), "agentic JSON shape: {text}");

    // E15: the payload always carries `citations` + `trust`; a sampled answer
    // with no citation markers is the honest low/weak no-evidence state.
    let payload: serde_json::Value = serde_json::from_str(&text).expect("agentic payload is JSON");
    assert!(payload.get("trust").is_some(), "trust present: {text}");
    assert!(
        payload.get("citations").is_some(),
        "citations present: {text}"
    );
    assert_eq!(payload["trust"]["level"], "low");
    assert_eq!(payload["trust"]["coverage"], 0.0);
    assert_eq!(payload["trust"]["weak_context"], true);

    // Exactly one sampling request answered by the IDE-side handler, with
    // the agentic system prompt + question as the user turn.
    assert_eq!(client.handler.calls.load(Ordering::SeqCst), 1);
    assert!(
        client
            .handler
            .last_question
            .lock()
            .unwrap()
            .contains("Question: why did init change?"),
        "sampling request carries the agentic user turn: {:?}",
        client.handler.last_question.lock().unwrap()
    );
    client.shutdown().await;
}

/// Positive citation path: the sampled model searches, receives the `[S1]` /
/// `[S2]` observation prefix, then answers citing both ids. The deterministic
/// assessment must cover the claims (coverage > 0, level above `low`) instead
/// of the honest no-evidence state asserted by the test above.
#[tokio::test]
async fn ask_project_cites_observed_evidence() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry.clone()).await;

    // Retrieval needs an embedding provider and a HEAD file_chunk fixture;
    // the reasoning leg stays the MCP sampling chain (same fallback steps).
    let hybrid = Arc::new(Client::new(
        Some(Arc::new(EmbeddingStub)),
        Some(Arc::new(FallbackChain::new(
            sampling_steps(registry.clone()),
            false,
        ))),
        None,
    ));
    *state.search.write().unwrap() = Arc::new(SearchService::new(
        Some(Arc::new(CitationFixtureDb)),
        Some(Arc::new(ibis_server::state::ClientAIAdapter(hybrid))),
    ));

    let client = TestClient::connect_sampling_scripted(
        state,
        vec![
            "THOUGHT: I need the init code.\nSEARCH: \"init flow\"".to_string(),
            "FINAL ANSWER: The init flow is implemented in the init module [S1]. The bootstrap wiring is implemented in the bootstrap module [S2]."
                .to_string(),
        ],
    )
    .await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project").with_arguments(
                json!({ "query": "how does init work?" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("ask_project over sampling");
    assert!(
        !result.is_error.unwrap_or(false),
        "ask_project failed: {result:?}"
    );
    let text = match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    let payload: serde_json::Value = serde_json::from_str(&text).expect("agentic payload is JSON");

    let citations = payload["citations"]
        .as_array()
        .unwrap_or_else(|| panic!("citations array: {text}"));
    assert_eq!(citations.len(), 2, "two observed sources: {text}");
    assert_eq!(citations[0]["citation_id"], "S1");
    assert_eq!(citations[1]["citation_id"], "S2");
    assert!(
        payload["trust"]["coverage"].as_f64().unwrap_or(0.0) > 0.0,
        "cited claims must be covered: {text}"
    );
    assert_ne!(
        payload["trust"]["level"], "low",
        "cited current evidence is above the low bucket: {text}"
    );
    assert_eq!(payload["invalid_citations"], 0, "no invented ids: {text}");
    assert_eq!(payload["stop_reason"], "final_answer");
    assert_eq!(payload["sources"].as_array().map(Vec::len), Some(2));

    // Two sampling turns: the SEARCH turn and the citing FINAL ANSWER.
    assert_eq!(client.handler.calls.load(Ordering::SeqCst), 2);
    let first_question = client.handler.last_question.lock().unwrap().clone();
    assert_eq!(
        first_question.matches("CITATION PROTOCOL:").count(),
        1,
        "the citation block reaches the model exactly once: {first_question}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn sampling_without_client_capability_degrades_to_chain_error() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry).await;
    let client = TestClient::connect_plain(state).await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project")
                .with_arguments(json!({ "query": "why?" }).as_object().unwrap().clone()),
        )
        .await
        .expect("ask_project executes");
    assert!(
        result.is_error.unwrap_or(false),
        "no sampling-capable client and no fallback legs: {result:?}"
    );
    let text = match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("mcp_sampling unavailable"),
        "the chain reports the sampling leg as unavailable: {text}"
    );
    assert!(
        text.contains("cursor unavailable"),
        "the dead lm-bridge leg is reported too: {text}"
    );
    // The sampling request was never sent (capability not declared).
    assert_eq!(client.handler.calls.load(Ordering::SeqCst), 0);
    client.shutdown().await;
}

/// G20 §5: a profile with `sampling: never` disables the leg for a session
/// that DID declare the capability — the chain skips it (no
/// `sampling/createMessage` is issued) and the policy lands on the shared
/// registry that the reasoning chain consults.
#[tokio::test]
async fn harness_sampling_never_skips_a_capable_client() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry.clone()).await;
    state
        .cfg
        .write()
        .unwrap()
        .harness
        .profiles
        .push(HarnessProfile {
            match_names: vec!["sampling-e2e".into()],
            name: "sampling-e2e".into(),
            sampling: "never".into(),
            hidden_tool_groups: Vec::new(),
        });

    let client = TestClient::connect_sampling(state).await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project")
                .with_arguments(json!({ "query": "why?" }).as_object().unwrap().clone()),
        )
        .await
        .expect("ask_project executes");
    assert!(
        result.is_error.unwrap_or(false),
        "sampling-never must degrade to the next legs: {result:?}"
    );
    let text = match result.content.first() {
        Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(
        text.contains("mcp_sampling unavailable"),
        "the profile disabled the leg despite the declared capability: {text}"
    );
    // No sampling request was sent despite the declared capability.
    assert_eq!(client.handler.calls.load(Ordering::SeqCst), 0);
    assert!(
        !registry.sampling_enabled(),
        "the policy is applied on the accepted call"
    );
    client.shutdown().await;
}
