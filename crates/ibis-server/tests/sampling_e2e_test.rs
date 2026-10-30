//! F0.5 protocol gate: reasoning via MCP sampling over the rmcp in-memory
//! duplex (the same NDJSON framing stdio uses). A sampling-capable client
//! answers `sampling/createMessage`; the `ask_project` tool's agentic lane
//! reasons through the attached session peer. The second scenario proves the
//! degradation: without the `sampling` capability the chain reports the
//! sampling leg unavailable (the fallback chain moves on).

// The rmcp sampling surface is SEP-2577-deprecated upstream but it is exactly
// what this gate exercises (F0.5 keeps sampling as the primary reasoning via).
#![allow(deprecated)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestParams, CreateMessageResult,
    Implementation, Role, SamplingMessage, SamplingMessageContentBlock,
};
use rmcp::service::{RequestContext, RoleClient, RunningService};
use rmcp::{ErrorData as McpError, ServiceExt, service::serve_client};
use serde_json::json;
use ibis_ai::{
    Client, CursorLMProvider, FallbackChain, McpSamplingProvider, AIProvider,
};
use ibis_core::config::Config;
use ibis_server::mcp_server::IbisServer;
use ibis_server::sampling::SamplingSessionRegistry;
use ibis_server::state::AppState;

/// An MCP client that declares the `sampling` capability and answers
/// `sampling/createMessage` with a canned assistant turn.
#[derive(Clone)]
struct SamplingTestClient {
    info: ClientConfig,
    calls: Arc<AtomicUsize>,
    last_question: Arc<Mutex<String>>,
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
        Ok(CreateMessageResult::new(
            SamplingMessage::new(
                Role::Assistant,
                SamplingMessageContentBlock::text("FINAL ANSWER: sampled via mcp"),
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
    let ticket = ibis_server::assembly::assemble_ticketing(db.clone()).await.0;
    let repopr = ibis_server::assembly::assemble_repopr(&ticket.config().clone());
    let ai_adapter: Option<Arc<dyn ibis_ai::AIProvider>> = Some(Arc::new(
        ibis_server::state::ClientAIAdapter(ai.clone()),
    ));
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
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(false),
    })
}

impl TestClient {
    /// Connects with the sampling-capable handler.
    async fn connect_sampling(state: Arc<AppState>) -> TestClient {
        Self::connect(state, true).await
    }

    /// Connects with a default (sampling-less) client handler.
    async fn connect_plain(state: Arc<AppState>) -> TestClient {
        Self::connect(state, false).await
    }

    async fn connect(state: Arc<AppState>, sampling: bool) -> TestClient {
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
fn sampling_chain(registry: Arc<SamplingSessionRegistry>) -> Arc<Client> {
    let steps: Vec<Arc<dyn AIProvider>> = vec![
        Arc::new(McpSamplingProvider::new(registry.clone())),
        Arc::new(CursorLMProvider::new("http://127.0.0.1:1")),
    ];
    Arc::new(Client::new(
        None,
        Some(Arc::new(FallbackChain::new(steps, false))),
        None,
    ))
}

#[tokio::test]
async fn ask_project_reasons_through_mcp_sampling() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry).await;
    let client = TestClient::connect_sampling(state).await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project")
                .with_arguments(json!({ "query": "why did init change?" })
                    .as_object()
                    .unwrap()
                    .clone()),
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

#[tokio::test]
async fn sampling_without_client_capability_degrades_to_chain_error() {
    let registry = Arc::new(SamplingSessionRegistry::new());
    let state = test_state(registry).await;
    let client = TestClient::connect_plain(state).await;
    let peer = client.peer();

    let result = peer
        .call_tool(
            rmcp::model::CallToolRequestParams::new("ask_project")
                .with_arguments(json!({ "query": "why?" })
                    .as_object()
                    .unwrap()
                    .clone()),
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