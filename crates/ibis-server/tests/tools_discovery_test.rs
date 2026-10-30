//! F0.6 protocol gate: dynamic tool discovery — the config-gated ticketing
//! group is announced on `tools/list` only when a ticketing provider is
//! configured (unconfigured installs surface the core set only), hidden tool
//! calls get a configuration error instead of a bare "tool not found", the
//! `tools` capability advertises `listChanged`, and a visibility flip
//! (ticketing hot reload) broadcasts `notifications/tools/list_changed` to
//! initialized sessions — whose re-list then reflects the new surface.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ErrorCode, Implementation,
};
use rmcp::service::{
    ClientLifecycleMode, ClientServiceExt, NotificationContext, Peer, RoleClient, RunningService,
    ServiceError, ServiceExt,
};
use rmcp::ClientHandler;
use ibis_ai::Client;
use ibis_core::config::{Config, ProjectConfig};
use ibis_server::registry::TICKETING_TOOLS;
use ibis_server::state::AppState;
use ibis_server::{assembly, mcp_server};

/// A minimal-but-real [`AppState`]: no DB (degraded clone-only), no AI
/// providers, no background workers — the ticketing service starts
/// unconfigured, so the F0.6 gate starts closed.
async fn test_state(projects: &[String]) -> Arc<AppState> {
    let mut cfg = Config {
        port: 3030,
        runtime_mode: "server".into(),
        ..Default::default()
    };
    for name in projects {
        cfg.projects.push(ProjectConfig {
            name: (*name).to_string(),
            working_repo_path: format!("C:/repos/{name}"),
            ..Default::default()
        });
    }
    let cfg = Arc::new(cfg);
    let db: Option<Arc<dyn ibis_db::executor::Executor>> = None;
    let ticket = assembly::assemble_ticketing(db.clone()).await.0;
    let repopr = assembly::assemble_repopr(&ticket.config().clone());
    Arc::new(AppState {
        cfg: Arc::new(RwLock::new((*cfg).clone())),
        db: Arc::new(RwLock::new(db)),
        search: Arc::new(RwLock::new(Arc::new(
            ibis_search::search::SearchService::new(None, None),
        ))),
        ai: Arc::new(Client::new(None, None, None)),
        ticket,
        repopr,
        sessions: Mutex::new(std::collections::HashMap::new()),
        ingestion: Arc::new(ibis_ingest_dynamic::queue::ProjectIngestionManager::new(
            cfg.clone(),
            None,
        )),
        active_repos: vec![],
        db_process: Mutex::new(None),
        resource_subs: ibis_server::resources::ResourceSubscriptions::default(),
        sampling: Arc::new(ibis_server::sampling::SamplingSessionRegistry::new()),
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(false),
    })
}

/// A ticketing config with one real (unreachable) provider: the F0.6 gate
/// opens without any network traffic.
fn configured_ticketing() -> ibis_ticketing::Config {
    let mut cfg = ibis_ticketing::default_config();
    cfg.providers.redmine = ibis_ticketing::RedmineConfig {
        base_url: "http://127.0.0.1:9".to_string(),
        api_key: "test-key".to_string(),
    };
    cfg
}

/// The real server over an in-memory duplex, plus the connected client.
struct TestClient {
    service: RunningService<RoleClient, ClientConfig>,
    server: tokio::task::JoinHandle<Result<(), String>>,
    state: Arc<AppState>,
}

impl TestClient {
    async fn connect(projects: &[&str]) -> TestClient {
        Self::connect_with(
            projects,
            ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("tools-discovery-test", "0.0.0"),
            ),
        )
        .await
    }

    async fn connect_with(projects: &[&str], config: ClientConfig) -> TestClient {
        let projects: Vec<String> = projects.iter().map(|s| s.to_string()).collect();
        let state = test_state(&projects).await;
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn({
            let state = state.clone();
            async move {
                let service = mcp_server::IbisServer::new(state, false);
                match service.serve(server_stream).await {
                    Ok(running) => {
                        let _ = running.waiting().await;
                        Ok(())
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
        });
        let service = match config
            .serve_with_lifecycle(client_stream, ClientLifecycleMode::Initialize)
            .await
        {
            Ok(service) => service,
            Err(e) => {
                let server_result = server.await;
                panic!("client init failed ({e}); server task: {server_result:?}")
            }
        };
        TestClient {
            service,
            server,
            state,
        }
    }

    fn peer(&self) -> &Peer<RoleClient> {
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

fn mcp_error(err: &ServiceError) -> (i32, String) {
    match err {
        ServiceError::McpError(e) => (e.code.0, e.message.to_string()),
        other => panic!("expected an MCP protocol error, got {other}"),
    }
}

/// Waits until the server-side discovery registry has `expected` session
/// peers (the `initialized` notification races the test's next call).
async fn wait_for_registered_peers(state: &Arc<AppState>, expected: usize) {
    for _ in 0..200 {
        if state.tool_discovery.peer_count() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "discovery registry never reached {expected} peers (now {})",
        state.tool_discovery.peer_count()
    );
}

fn ticket_names(tools: &[rmcp::model::Tool]) -> Vec<&str> {
    tools
        .iter()
        .map(|t| t.name.as_ref())
        .filter(|n| TICKETING_TOOLS.contains(n))
        .collect()
}

/// The closed gate: unconfigured ticketing announces the 41 core tools only,
/// hidden calls get the configuration error, and the capability advertises
/// `listChanged`.
#[tokio::test]
async fn unconfigured_ticketing_hides_the_ticket_group() {
    let client = TestClient::connect(&[]).await;
    let peer = client.peer();

    // F0.6: tools capability carries listChanged=true.
    let info = peer
        .peer_info()
        .expect("initialize result cached by the client");
    let tools = info.capabilities.tools.as_ref().expect("tools capability");
    assert_eq!(tools.list_changed, Some(true), "listChanged advertised");

    // tools/list: exactly the core set — no ticket_* row.
    let listed = peer
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    assert_eq!(listed.tools.len(), 56 - TICKETING_TOOLS.len());
    assert!(
        ticket_names(&listed.tools).is_empty(),
        "ticket group must be hidden: {:?}",
        ticket_names(&listed.tools)
    );
    for core in ["memory_search", "settings_apply", "ask_project"] {
        assert!(
            listed.tools.iter().any(|t| t.name.as_ref() == core),
            "core tool {core} missing"
        );
    }

    // Calling a hidden tool: a configuration error, not "tool not found".
    let err = client
        .service
        .call_tool(CallToolRequestParams::new("ticket_get_capabilities"))
        .await
        .expect_err("hidden ticket tool must error");
    let (code, message) = mcp_error(&err);
    assert_eq!(code, ErrorCode::INVALID_PARAMS.0);
    assert!(
        message.contains("ticketing is not configured"),
        "hidden call: {message}"
    );
    assert!(
        message.contains("ticketing_config.json"),
        "the error names the fix: {message}"
    );

    client.shutdown().await;
}

/// The open gate: one registered provider announces the full 56-tool surface
/// and the ticket tools actually dispatch.
#[tokio::test]
async fn configured_ticketing_announces_the_full_surface() {
    let client = TestClient::connect(&[]).await;
    let visible =
        assembly::reload_ticketing_with_config(&client.state, configured_ticketing()).await;
    assert!(visible, "the gate opens with a provider");

    let listed = client
        .service
        .peer()
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    assert_eq!(listed.tools.len(), 56);
    let announced = ticket_names(&listed.tools);
    assert_eq!(announced.len(), TICKETING_TOOLS.len(), "{announced:?}");
    for name in TICKETING_TOOLS {
        assert!(
            announced.contains(name),
            "{name} missing from the announced set"
        );
    }

    // A formerly hidden tool dispatches: capabilities report the provider.
    let caps = client
        .service
        .call_tool(CallToolRequestParams::new("ticket_get_capabilities"))
        .await
        .expect("ticket_get_capabilities");
    assert!(
        !caps.is_error.is_some_and(|e| e),
        "capabilities call: {caps:?}"
    );
    let text = match &caps.content[0] {
        rmcp::model::ContentBlock::Text(text) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert!(text.contains("redmine"), "capabilities body: {text}");

    client.shutdown().await;
}

/// A legacy-protocol client that records `notifications/tools/list_changed`.
struct NotifyingClient {
    config: ClientConfig,
    changes: Arc<Mutex<usize>>,
}

impl ClientHandler for NotifyingClient {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        *self.changes.lock().unwrap() += 1;
    }
}

/// The flip: a ticketing hot reload that opens/closes the gate broadcasts
/// `notifications/tools/list_changed` to the initialized session, whose
/// re-list then reflects the new surface. A reload without a flip is silent.
#[tokio::test]
async fn visibility_flip_broadcasts_tools_list_changed() {
    let changes = Arc::new(Mutex::new(0usize));
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("tools-flip-test", "0.0.0"),
    );
    let state = test_state(&[]).await;
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn({
        let state = state.clone();
        async move {
            let service = mcp_server::IbisServer::new(state, false);
            match service.serve(server_stream).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    });
    let handler = NotifyingClient {
        config,
        changes: changes.clone(),
    };
    let service = handler
        .serve(client_stream)
        .await
        .expect("notifying client init");
    let peer = service.peer();

    wait_for_registered_peers(&state, 1).await;
    let listed = peer
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    assert_eq!(listed.tools.len(), 56 - TICKETING_TOOLS.len());

    // A reload that keeps the gate closed is silent.
    let visible =
        assembly::reload_ticketing_with_config(&state, ibis_ticketing::default_config()).await;
    assert!(!visible);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*changes.lock().unwrap(), 0, "no flip, no notification");

    // Opening the gate announces the change; the client's cached list is
    // invalidated automatically and the re-list shows the ticket group.
    let visible = assembly::reload_ticketing_with_config(&state, configured_ticketing()).await;
    assert!(visible);
    for _ in 0..100 {
        if *changes.lock().unwrap() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *changes.lock().unwrap(),
        1,
        "one tools/list_changed after the open flip"
    );
    let relisted = peer.list_tools(Default::default()).await.expect("re-list");
    assert_eq!(relisted.tools.len(), 56);

    // Closing the gate announces again and the re-list hides the group.
    let visible =
        assembly::reload_ticketing_with_config(&state, ibis_ticketing::default_config()).await;
    assert!(!visible);
    for _ in 0..100 {
        if *changes.lock().unwrap() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        *changes.lock().unwrap(),
        2,
        "second tools/list_changed after the close flip"
    );
    let relisted = peer.list_tools(Default::default()).await.expect("re-list");
    assert_eq!(relisted.tools.len(), 56 - TICKETING_TOOLS.len());
    assert!(ticket_names(&relisted.tools).is_empty());

    let _ = service.cancel().await;
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("server serve error: {e}"),
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// Data-shape gate: the F0.6 table holds exactly the 15 ticketing
/// registrations (no core tool is config-gated, no unknown name).
#[test]
fn ticketing_tool_table_matches_the_registry() {
    use ibis_server::registry::{spec_of, TOOL_SPECS};
    assert_eq!(TICKETING_TOOLS.len(), 15);
    for name in TICKETING_TOOLS {
        assert!(spec_of(name).is_some(), "{name} must be a registered tool");
    }
    let gated: Vec<&str> = TOOL_SPECS
        .iter()
        .map(|s| s.name)
        .filter(|n| TICKETING_TOOLS.contains(n))
        .collect();
    assert_eq!(gated.len(), TICKETING_TOOLS.len(), "no unknown gated names");
}
