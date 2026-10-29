//! F0.4 protocol gate: the real [`ThreskiServer`] over the rmcp in-memory
//! duplex (the same NDJSON framing stdio uses) serves the resource surface —
//! capability advertisement (`resources.subscribe`), the three
//! `resources/templates/list` rows, `resources/read` (guide, template
//! dispatch with degraded-mode errors, unknown-URI rejection) and the live
//! update flow: `subscriptions/listen` (2026-07-28 clients, the rmcp
//! default) and legacy `resources/subscribe`/`resources/unsubscribe`
//! (≤ 2025-11-25 clients), both receiving `notifications/resources/updated`
//! when a mutation fires the registry.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::future::FutureExt;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ErrorCode, Implementation, ProtocolVersion,
    ReadResourceRequestParams, ResourceUpdatedNotificationParam, SubscribeRequestParams,
    SubscriptionFilter, UnsubscribeRequestParams,
};
use rmcp::service::{
    ClientLifecycleMode, ClientServiceExt, NotificationContext, Peer, RoleClient, RunningService,
    ServiceError, ServiceExt,
};
use rmcp::ClientHandler;
use serde_json::{json, Value};
use threski_ai::Client;
use threski_core::config::{Config, ProjectConfig};
use threski_server::resources;
use threski_server::state::AppState;
use threski_server::{assembly, mcp_server};

/// A minimal-but-real [`AppState`]: no DB (degraded clone-only), no AI
/// providers, no background workers — enough state for the resource surface
/// (reads degrade, the subscription registry is local).
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
    let db: Option<Arc<dyn threski_db::executor::Executor>> = None;
    let ticket = assembly::assemble_ticketing(db.clone()).await.0;
    let repopr = assembly::assemble_repopr(&ticket.config().clone());
    Arc::new(AppState {
        cfg: Arc::new(RwLock::new((*cfg).clone())),
        db: Arc::new(RwLock::new(db)),
        search: Arc::new(RwLock::new(Arc::new(
            threski_search::search::SearchService::new(None, None),
        ))),
        ai: Arc::new(Client::new(None, None, None)),
        ticket,
        repopr,
        sessions: Mutex::new(std::collections::HashMap::new()),
        ingestion: Arc::new(threski_ingest_dynamic::queue::ProjectIngestionManager::new(
            cfg.clone(),
            None,
        )),
        active_repos: vec![],
        db_process: Mutex::new(None),
        resource_subs: resources::ResourceSubscriptions::default(),
    })
}

/// The real server over an in-memory duplex, plus the connected client.
/// The service stays alive for the whole test (the peer borrows from it);
/// [`TestClient::shutdown`] tears both sides down.
struct TestClient {
    service: RunningService<RoleClient, ClientConfig>,
    server: tokio::task::JoinHandle<Result<(), String>>,
    state: Arc<AppState>,
}

impl TestClient {
    /// The legacy initialize lifecycle (rmcp default): the handshake
    /// negotiates 2025-11-25, so `resources/subscribe` is the served
    /// subscription mechanism.
    async fn connect(projects: &[&str]) -> TestClient {
        Self::connect_with_lifecycle(projects, ClientLifecycleMode::Initialize).await
    }

    /// The 2026-07-28 discover lifecycle (`server/discover`, per-request
    /// `_meta`): `subscriptions/listen` is the served subscription mechanism.
    async fn connect_discover(projects: &[&str]) -> TestClient {
        Self::connect_with_lifecycle(
            projects,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
    }

    async fn connect_with_lifecycle(
        projects: &[&str],
        lifecycle: ClientLifecycleMode,
    ) -> TestClient {
        let projects: Vec<String> = projects.iter().map(|s| s.to_string()).collect();
        let state = test_state(&projects).await;
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn({
            let state = state.clone();
            async move {
                let service = mcp_server::ThreskiServer::new(state, false);
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
            Implementation::new("resources-protocol-test", "0.0.0"),
        )
        .serve_with_lifecycle(client_stream, lifecycle)
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

const MEMORY_URI: &str = "threski://memory/decision/memory:abc123";

/// Waits until the server-side registry has `expected` subscribers (the
/// 2026-07-28 acknowledgment precedes the server's own `listen` hook, so
/// fires are gated on registration).
async fn wait_for_subscribers(state: &Arc<AppState>, expected: usize) {
    for _ in 0..200 {
        if state.resource_subs.subscriber_count() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "registry never reached {expected} subscribers (now {})",
        state.resource_subs.subscriber_count()
    );
}

#[tokio::test]
async fn resources_capability_templates_and_reads() {
    let client = TestClient::connect(&[]).await;
    let peer = client.peer();

    // Capability advertisement: subscribe=true on the negotiated initialize
    // result (live updates are part of the surface).
    let info = peer
        .peer_info()
        .expect("initialize result cached by the client");
    let res = info
        .capabilities
        .resources
        .as_ref()
        .expect("resources capability");
    assert_eq!(res.subscribe, Some(true), "subscribe advertised: {info:?}");
    assert_eq!(res.list_changed, Some(true), "list_changed advertised");

    // resources/list stays the static guide surface (templates are separate).
    let listed = peer
        .list_resources(Default::default())
        .await
        .expect("resources/list");
    assert_eq!(listed.resources.len(), 1, "guide only: {listed:?}");

    // resources/templates/list: exactly the F0.4 rows.
    let templates = peer
        .list_resource_templates(Default::default())
        .await
        .expect("resources/templates/list");
    assert_eq!(templates.resource_templates.len(), 3);
    let mut rows: Vec<(&str, &str)> = templates
        .resource_templates
        .iter()
        .map(|t| {
            (
                t.uri_template.as_str(),
                t.mime_type.as_deref().unwrap_or(""),
            )
        })
        .collect();
    rows.sort_unstable();
    assert_eq!(
        rows,
        [
            (resources::MEMORY_TEMPLATE, "application/json"),
            (resources::RULES_TEMPLATE, "application/json"),
            (resources::TICKET_TEMPLATE, "application/json"),
        ]
    );

    // The guide resource still reads.
    let guide = peer
        .read_resource(ReadResourceRequestParams::new(
            threski_httpserver::discovery::GUIDE_URI,
        ))
        .await
        .expect("read guide");
    let guide_text = match &guide.contents[0] {
        rmcp::model::ResourceContents::TextResourceContents {
            text, mime_type, ..
        } => {
            assert_eq!(mime_type.as_deref(), Some("text/markdown"));
            text.clone()
        }
        other => panic!("expected text contents, got {other:?}"),
    };
    assert!(guide_text.contains("Threski"), "guide body: {guide_text}");

    // Template reads degrade honestly without a DB (resource not found, not
    // a silent empty body). This client negotiates 2025-11-25, so the SDK
    // keeps the legacy RESOURCE_NOT_FOUND code on the wire.
    let memory = peer
        .read_resource(ReadResourceRequestParams::new(resources::memory_uri(
            "decision",
            "memory:abc123",
        )))
        .await
        .expect_err("degraded memory read must error");
    let (code, message) = mcp_error(&memory);
    assert_eq!(code, ErrorCode::RESOURCE_NOT_FOUND.0);
    assert!(
        message.contains("database not connected"),
        "degraded memory read: {memory}"
    );
    let rule = peer
        .read_resource(ReadResourceRequestParams::new(resources::rule_uri("nope")))
        .await
        .expect_err("degraded rules read must error");
    let (code, _) = mcp_error(&rule);
    assert_eq!(code, ErrorCode::RESOURCE_NOT_FOUND.0);
    let ticket = peer
        .read_resource(ReadResourceRequestParams::new(resources::ticket_uri(
            "PROJ-1",
        )))
        .await
        .expect_err("unconfigured ticketing read must error");
    let (code, message) = mcp_error(&ticket);
    assert_eq!(code, ErrorCode::INTERNAL_ERROR.0);
    assert!(message.contains("ticketing"), "ticket read: {ticket}");

    // Malformed template URIs and foreign schemes are invalid params.
    for bad in [
        "threski://memory/decision",
        "threski://memory/unknownkind/memory:abc123",
        "threski://rules/",
        "threski://ticket/a/b",
        "https://example.com/x",
    ] {
        let err = peer
            .read_resource(ReadResourceRequestParams::new(bad))
            .await
            .expect_err("unknown resource must error");
        let (code, message) = mcp_error(&err);
        assert_eq!(code, ErrorCode::INVALID_PARAMS.0, "{bad}");
        assert!(message.contains("unknown resource"), "{bad}: {err}");
    }
    client.shutdown().await;
}

#[tokio::test]
async fn subscriptions_listen_delivers_resources_updated() {
    // The 2026-07-28 discover lifecycle: `server/discover` instead of the
    // initialize handshake, so `subscriptions/listen` is the served
    // subscription mechanism.
    let client = TestClient::connect_discover(&[]).await;
    let peer = client.peer();
    let state = client.state.clone();

    // 2026-07-28 subscription (the rmcp default lifecycle): the accepted
    // filter echoes the requested URIs.
    let mut subscription = peer
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(MEMORY_URI)
                .build(),
        )
        .await
        .expect("subscriptions/listen");
    let acknowledged = subscription.acknowledged().clone();
    assert_eq!(
        acknowledged.resource_subscriptions,
        Some(vec![MEMORY_URI.to_string()]),
        "accepted filter: {acknowledged:?}"
    );

    wait_for_subscribers(&state, 1).await;

    // A mutation on the subscribed URI reaches the subscription stream with
    // the subscription metadata attached (validated by `next`).
    state.resource_subs.fire(MEMORY_URI).await;
    let notification = subscription
        .next()
        .await
        .expect("notification received")
        .expect("subscription not ended");
    match notification {
        rmcp::model::ServerNotification::ResourceUpdatedNotification(update) => {
            assert_eq!(update.params.uri, MEMORY_URI);
        }
        other => panic!("expected resources/updated, got {other:?}"),
    }

    // A mutation on another URI does not leak onto this stream.
    state
        .resource_subs
        .fire("threski://memory/gotcha/memory:other")
        .await;
    let nothing = tokio::time::timeout(Duration::from_millis(300), subscription.next()).await;
    assert!(nothing.is_err(), "unexpected notification on a foreign URI");

    // Cancelling the listen request removes the registration (the server's
    // listen hook returns and unregisters by token).
    subscription.cancel().await.expect("subscription cancel");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        state.resource_subs.subscriber_count(),
        0,
        "cancelled subscription unregistered"
    );
    client.shutdown().await;
}

/// A legacy-protocol client (≤ 2025-11-25) that records
/// `notifications/resources/updated` through its handler.
struct CapturingClient {
    config: ClientConfig,
    updated: Arc<Mutex<Vec<String>>>,
}

impl ClientHandler for CapturingClient {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.updated.lock().unwrap().push(params.uri.to_string());
    }
}

#[tokio::test]
async fn legacy_subscribe_unsubscribe_round_trip() {
    let state = test_state(&[]).await;
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn({
        let state = state.clone();
        async move {
            let service = mcp_server::ThreskiServer::new(state, false);
            match service.serve(server_stream).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    });
    let updated = Arc::new(Mutex::new(Vec::<String>::new()));
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("resources-legacy-test", "0.0.0"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_11_25);
    let client_service = CapturingClient {
        config,
        updated: updated.clone(),
    };
    let service = client_service
        .serve(client_stream)
        .await
        .expect("legacy client init");
    let peer = service.peer();

    // Legacy resources/subscribe is served on ≤ 2025-11-25, and a mutation
    // fires the registered peer.
    #[allow(deprecated)]
    {
        peer.subscribe(SubscribeRequestParams::new(MEMORY_URI))
            .await
            .expect("resources/subscribe");
    }
    assert_eq!(state.resource_subs.subscriber_count(), 1);
    state.resource_subs.fire(MEMORY_URI).await;
    for _ in 0..100 {
        if !updated.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        updated.lock().unwrap().as_slice(),
        [MEMORY_URI],
        "legacy peer received resources/updated"
    );

    // Unsubscribe removes the registration; further fires stay silent.
    #[allow(deprecated)]
    {
        peer.unsubscribe(UnsubscribeRequestParams::new(MEMORY_URI))
            .await
            .expect("resources/unsubscribe");
    }
    assert_eq!(state.resource_subs.subscriber_count(), 0);
    let before = updated.lock().unwrap().len();
    state.resource_subs.fire(MEMORY_URI).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        updated.lock().unwrap().len(),
        before,
        "no delivery after unsubscribe"
    );

    let _ = service.cancel().await;
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("server serve error: {e}"),
        Err(join) => std::panic::resume_unwind(join.into_panic()),
    }
}

/// Wire shapes of the F0.4 surface: templates serialize as RFC 6570
/// uriTemplate rows with title + description + mime.
#[test]
fn templates_wire_shapes() {
    let templates = resources::resource_templates();
    let wire: Vec<Value> = templates
        .iter()
        .map(|t| serde_json::to_value(t).expect("template serializes"))
        .collect();
    assert_eq!(wire.len(), 3);
    for row in &wire {
        assert_eq!(row["mimeType"], json!("application/json"));
        assert!(row["uriTemplate"].is_string(), "uriTemplate: {row}");
        assert!(row["name"].is_string(), "name: {row}");
        assert!(row["title"].is_string(), "title: {row}");
        assert!(row["description"].is_string(), "description: {row}");
    }
    let memory = wire
        .iter()
        .find(|r| r["uriTemplate"] == json!(resources::MEMORY_TEMPLATE))
        .expect("memory template listed");
    assert_eq!(memory["name"], json!("Typed Memory"));
}

/// The fire helper behind the mutation hooks: memory URIs carry kind and
/// id; rules twin their `threski://memory/rule/{id}` URI with
/// `threski://rules/{title}`.
#[tokio::test]
async fn fire_targets_both_rule_surfaces() {
    let registry = resources::ResourceSubscriptions::default();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let notify: resources::NotifyFn = {
        let seen = seen.clone();
        Arc::new(move |uri: String| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(uri);
            }
            .boxed()
        })
    };
    registry.subscribe(
        "threski://rules/no-deps-on-node-builtins",
        1,
        notify.clone(),
    );
    registry.subscribe("threski://memory/rule/memory:rule1", 2, notify);

    // The mutation hook fires the memory URI and (for rules) the rules twin.
    registry
        .fire(&resources::memory_uri("rule", "memory:rule1"))
        .await;
    registry
        .fire(&resources::rule_uri("no-deps-on-node-builtins"))
        .await;
    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        [
            "threski://memory/rule/memory:rule1",
            "threski://rules/no-deps-on-node-builtins"
        ]
    );
}
