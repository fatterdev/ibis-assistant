//! F0.7 protocol gate: the real [`IbisServer`] over the rmcp in-memory
//! duplex serves the MCP App "Ibis Assistant Memory Curator" surface —
//! the `ui://ibis/memory-curator` resource is listed with the SEP-1865
//! MIME type and loopback CSP metadata, `resources/read` returns the
//! self-contained app document, a foreign `ui://` URI stays an unknown
//! resource, and the three memory tools declare the `_meta.ui.resourceUri`
//! linkage (host-side tool metadata) while every other tool stays meta-free.
//! The textual degradation of the tool results is gated by the unit tests on
//! `tools::with_ui_meta`.

use std::sync::{Arc, Mutex, RwLock};

use rmcp::model::{
    ClientCapabilities, ClientConfig, ErrorCode, Implementation, ReadResourceRequestParams,
    ResourceContents,
};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, Peer, RoleClient, RunningService,
    ServiceError, ServiceExt};
use serde_json::Value;
use ibis_ai::Client;
use ibis_core::config::{Config, ProjectConfig};
use ibis_httpserver::curator;
use ibis_server::resources;
use ibis_server::sampling;
use ibis_server::state::AppState;
use ibis_server::{assembly, mcp_server};

/// A minimal-but-real [`AppState`]: no DB, no AI providers, no background
/// workers — the F0.7 surface is static metadata + an embedded asset.
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
        resource_subs: resources::ResourceSubscriptions::default(),
        sampling: Arc::new(sampling::SamplingSessionRegistry::new()),
        tool_discovery: ibis_server::tool_discovery::ToolDiscovery::new(false),
    })
}

/// The real server over an in-memory duplex, plus the connected client.
struct TestClient {
    service: RunningService<RoleClient, ClientConfig>,
    server: tokio::task::JoinHandle<Result<(), String>>,
}

impl TestClient {
    async fn connect() -> TestClient {
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
        let service = match ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("curator-app-test", "0.0.0"),
        )
        .serve_with_lifecycle(client_stream, ClientLifecycleMode::Initialize)
        .await
        {
            Ok(service) => service,
            Err(e) => {
                let server_result = server.await;
                panic!("client init failed ({e}); server task: {server_result:?}")
            }
        };
        TestClient { service, server }
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

#[tokio::test]
async fn curator_app_listed_and_readable_with_app_mime() {
    let client = TestClient::connect().await;
    let peer = client.peer();

    // resources/list: the guide plus the Memory Curator app.
    let listed = peer
        .list_resources(Default::default())
        .await
        .expect("resources/list");
    assert_eq!(
        listed.resources.len(),
        2,
        "guide + curator: {listed:?}"
    );
    let declared = listed
        .resources
        .iter()
        .find(|r| r.uri == curator::CURATOR_URI)
        .expect("curator app listed");
    assert_eq!(declared.name, curator::CURATOR_NAME);
    assert_eq!(declared.title.as_deref(), Some(curator::CURATOR_NAME));
    assert_eq!(
        declared.mime_type.as_deref(),
        Some(curator::CURATOR_MIME),
        "SEP-1865 UI MIME type on the declaration"
    );
    // The declaration carries the loopback-only CSP policy.
    let connect = declared
        .meta
        .as_ref()
        .and_then(|m| m.get("ui"))
        .and_then(|ui| ui.get("csp"))
        .and_then(|csp| csp.get("connectDomains"))
        .and_then(|v| v.as_array())
        .expect("declaration _meta.ui.csp.connectDomains");
    let domains: Vec<&str> = connect
        .iter()
        .map(|v| v.as_str().expect("string domain"))
        .collect();
    assert_eq!(domains, curator::CURATOR_CONNECT_DOMAINS);

    // resources/read returns the self-contained document.
    let read = peer
        .read_resource(ReadResourceRequestParams::new(curator::CURATOR_URI))
        .await
        .expect("read curator app");
    match &read.contents[0] {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            meta,
        } => {
            assert_eq!(uri, curator::CURATOR_URI);
            assert_eq!(
                mime_type.as_deref(),
                Some(curator::CURATOR_MIME),
                "SEP-1865 MIME type on the contents"
            );
            assert!(text.starts_with("<!DOCTYPE html>"), "HTML5 document");
            assert!(
                text.contains(curator::CURATOR_CSP),
                "document embeds the loopback-only CSP"
            );
            assert!(
                text.contains("ui/initialize"),
                "app speaks the MCP Apps bridge"
            );
            let contents_connect = meta
                .as_ref()
                .and_then(|m| m.get("ui"))
                .and_then(|ui| ui.get("csp"))
                .and_then(|csp| csp.get("connectDomains"))
                .and_then(|v| v.as_array())
                .expect("contents _meta.ui.csp.connectDomains");
            assert_eq!(contents_connect.len(), curator::CURATOR_CONNECT_DOMAINS.len());
        }
        other => panic!("expected text contents, got {other:?}"),
    }

    // A foreign ui:// URI is not the app: unknown resource, protocol error.
    let err = peer
        .read_resource(ReadResourceRequestParams::new("ui://example.com/app"))
        .await
        .expect_err("foreign ui:// resource must error");
    let (code, message) = mcp_error(&err);
    assert_eq!(code, ErrorCode::INVALID_PARAMS.0);
    assert!(message.contains("unknown resource"), "error: {err}");

    client.shutdown().await;
}

#[tokio::test]
async fn memory_tools_link_the_curator_app() {
    let client = TestClient::connect().await;
    let peer = client.peer();

    let tools = peer
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    for name in ["memory_add", "memory_search", "memory_expand"] {
        let tool = tools
            .tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} announced on tools/list"));
        let uri = tool
            .meta
            .as_ref()
            .and_then(|m| m.get("ui"))
            .and_then(|ui| ui.get("resourceUri"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{name} missing _meta.ui.resourceUri"));
        assert_eq!(
            uri,
            curator::CURATOR_URI,
            "{name} links the Memory Curator app"
        );
    }

    // Tools outside the linkage stay meta-free.
    let get_rules = tools
        .tools
        .iter()
        .find(|t| t.name == "get_rules")
        .expect("get_rules announced");
    assert!(
        get_rules.meta.is_none(),
        "non-curator tools carry no _meta: {get_rules:?}"
    );
    client.shutdown().await;
}

/// The serialized declaration keeps the `_meta` key on the wire (spec shape
/// gate, no client needed).
#[test]
fn curator_declaration_wire_shape() {
    let resource = mcp_server::curator_resource();
    let wire: Value = serde_json::to_value(&resource).expect("resource serializes");
    assert_eq!(wire["uri"], Value::String(curator::CURATOR_URI.to_string()));
    assert_eq!(
        wire["mimeType"],
        Value::String(curator::CURATOR_MIME.to_string())
    );
    assert!(wire["_meta"]["ui"]["csp"]["connectDomains"].is_array());
    assert!(wire["_meta"]["ui"]["prefersBorder"].is_boolean());
}