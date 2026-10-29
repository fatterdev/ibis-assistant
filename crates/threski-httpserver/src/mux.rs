//! The HTTP mux.
//!
//! Route table (parity, minus the D11-removed `/sse` + `/message`):
//!
//! | Route | Handler |
//! |---|---|
//! | `GET /` (any method) | discovery, content-negotiated |
//! | `/mcp`, `/mcp/` | injected rmcp Streamable HTTP service behind the origin gate |
//! | `/api/v1/logs/upload` | log upload (nested, handler sees `/`) |
//! | `/api/v1/settings[...]` | settings API `Router` — **merged**, paths stay absolute (`mux.Handle without StripPrefix) |
//! | `/settings[...]` | settings UI `Router` — **nested**, prefix stripped |
//! | anything else | 404 `404 page not found` ( ServeMux fallback text) |
//!
//! The whole router is wrapped in the auth middleware.
//!
//! [`serve`] reproduces the server settings: `ReadHeaderTimeout: 10s`
//! and a graceful shutdown bounded by 5s (
//! ).

use crate::auth::auth_middleware;
use crate::discovery::{discovery_handler, DiscoveryState, ToolsProvider};
use crate::origin::origin_allowed;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use threski_core::config::{split_host_port, Config};
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::Layer as _;

/// Streamable HTTP config for the MCP mount: 30s SSE keep-alive (
/// `WithHeartbeatInterval(30*time.Second)`), JSON bodies for
/// terminal responses (the observable default of the streamable server),
/// no Host allowlist (the original had none; browser DNS-rebinding is covered by the
/// origin gate), and a 32 MiB POST body cap matching the other product-wide
/// limit.
pub fn streamable_http_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_sse_keep_alive(Some(Duration::from_secs(30)))
        .with_json_response(true)
        .disable_allowed_hosts()
        .with_max_request_body_bytes(32 << 20)
}

/// The bind host of the listener (`bindHost): host part of
/// `cfg.listen_addr`, empty on parse failure.
pub fn bind_host(cfg: &Config) -> String {
    split_host_port(&cfg.listen_addr()).unwrap_or_default()
}

/// Axum middleware enforcing the origin gate on the MCP routes (
/// `originGate`).
async fn origin_gate_middleware(
    State(bind_host): State<String>,
    req: Request,
    next: Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !origin_allowed(origin, &bind_host) {
        // Scenario: http.Error(w, "forbidden origin", 403).
        return (
            StatusCode::FORBIDDEN,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            "forbidden origin\n",
        )
            .into_response();
    }
    next.run(req).await
}

/// Builds the full HTTP router (`HandlerWithSettings).
///
/// `mcp` is the injected Streamable HTTP service — wave-4 `threski-server` wires
/// the 56-tool rmcp registry via [`streamable_http_config`] +
/// `StreamableHttpService::new`; tests may pass a minimal handler.
/// `tools` feeds the discovery `tools[]` list (registry provider; empty by
/// default). `log_upload` mounts the upload endpoint; `settings_api` is
/// merged (absolute paths, no strip) and `settings_ui` nested (prefix
/// stripped) — the mount seams for the settings lane.
pub fn handler<S, SM>(
    cfg: Arc<Config>,
    mcp: StreamableHttpService<S, SM>,
    tools: ToolsProvider,
    log_upload: Option<Router>,
    settings_api: Option<Router>,
    settings_ui: Option<Router>,
) -> Router
where
    S: rmcp::ServerHandler + Send + 'static,
    SM: SessionManager + 'static,
{
    let host = bind_host(&cfg);
    let mcp_router = Router::new()
        .route_service("/mcp", mcp.clone())
        .route_service("/mcp/", mcp)
        .layer(middleware::from_fn_with_state(host, origin_gate_middleware));

    let discovery = DiscoveryState::new(cfg.clone(), tools);

    let mut root = Router::new()
        .route("/", any(discovery_handler).with_state(discovery))
        .merge(mcp_router);

    if let Some(upload) = log_upload {
        root = root.nest("/api/v1/logs/upload", upload);
    }
    if let Some(api) = settings_api {
        root = root.merge(api);
    }
    if let Some(ui) = settings_ui {
        // `mux.Handle("/settings/", http.StripPrefix("/settings/", ui))`
        // plus a 302 from `/settings` . axum's `nest` matches
        // `/settings` and `/settings/<rest>` but not the bare trailing-slash
        // form, so the SPA mount is registered explicitly for both shapes.
        let strip = StripSettingsLayer;
        root = root
            .route_service("/settings/", strip.clone().layer(ui.clone()))
            .route_service("/settings/{*rest}", strip.layer(ui))
            .route(
                "/settings",
                axum::routing::any(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "/settings/")],
                        "Found.\n",
                    )
                }),
            );
    }
    // Scenario: ServeMux 404 body for unmounted paths.
    root = root.fallback(|| async {
        (
            StatusCode::NOT_FOUND,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            "404 page not found\n",
        )
    });
    // `Auth(mux) — outermost wrapper.
    root.layer(middleware::from_fn(auth_middleware))
}

/// Errors surfaced by [`serve`].
#[derive(Debug)]
pub enum HttpServeError {
    Io(std::io::Error),
}

/// `http.StripPrefix("/settings/")` equivalent: rewrites the request path from
/// `/settings[/...]` to `/[/...]` before handing it to the settings SPA router
///
#[derive(Clone, Copy)]
struct StripSettingsLayer;

impl<S> tower::Layer<S> for StripSettingsLayer {
    type Service = StripSettings<S>;
    fn layer(&self, inner: S) -> Self::Service {
        StripSettings { inner }
    }
}

#[derive(Clone, Copy)]
struct StripSettings<S> {
    inner: S,
}

impl<S> tower::Service<Request> for StripSettings<S>
where
    S: tower::Service<Request, Error = std::convert::Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let path = req.uri().path();
        let rest = path
            .strip_prefix("/settings")
            .unwrap_or(path)
            .trim_start_matches('/');
        let rewritten = match req.uri().query() {
            Some(q) if !q.is_empty() => format!("/{rest}?{q}"),
            _ => format!("/{rest}"),
        };
        if let Ok(new_uri) = axum::http::Uri::from_maybe_shared(bytes::Bytes::from(rewritten)) {
            *req.uri_mut() = new_uri;
        }
        self.inner.call(req)
    }
}

impl std::fmt::Display for HttpServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpServeError::Io(e) => write!(f, "http server error: {e}"),
        }
    }
}

impl std::error::Error for HttpServeError {}

/// Connection-level wrapper that stamps `ConnectInfo<SocketAddr>` on every
/// request so the auth middleware can apply loopback-trust semantics ( read
/// `r.RemoteAddr`).
#[derive(Clone)]
struct WithPeerAddr<S> {
    inner: S,
    peer: std::net::SocketAddr,
}

impl<S, B> tower::Service<axum::http::Request<B>> for WithPeerAddr<S>
where
    S: tower::Service<axum::http::Request<B>, Error = std::convert::Infallible> + Send + 'static,
    S::Future: Send,
    B: http_body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::http::Request<B>) -> Self::Future {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(self.peer));
        self.inner.call(req)
    }
}

/// Serves `app` on `listener` until `shutdown` fires: per-connection
/// `ReadHeaderTimeout: 10s`, graceful connection shutdown on
/// the token, and a 5s drain budget after the accept loop ends (
/// ).
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: CancellationToken,
) -> Result<(), HttpServeError> {
    let mut connections: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let graceful = shutdown.child_token();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (io, peer) = accepted.map_err(HttpServeError::Io)?;
                let service = hyper_util::service::TowerToHyperService::new(WithPeerAddr {
                    inner: app.clone(),
                    peer,
                });
                let mut builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                builder
                    .http1()
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(Some(Duration::from_secs(10)));
                let conn = builder
                    .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(io), service)
                    .into_owned();
                let conn_shutdown = graceful.child_token();
                connections.spawn(async move {
                    tokio::pin!(conn);
                    tokio::select! {
                        result = &mut conn => result.map_err(|e| e.to_string()),
                        () = conn_shutdown.cancelled() => {
                            // Stop reading new requests; finish in-flight ones.
                            conn.as_mut().graceful_shutdown();
                            conn.await.map_err(|e| e.to_string())
                        }
                    }
                });
            }
        }
    }
    // 5s drain budget for in-flight requests ( shutdown timeout).
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    Ok(())
}

/// Binds `cfg.listen_addr` and serves until `shutdown` fires (
/// `NewHTTPServer` + `ListenAndServe`).
pub async fn run(
    cfg: &Config,
    app: Router,
    shutdown: CancellationToken,
) -> Result<(), HttpServeError> {
    let listener = tokio::net::TcpListener::bind(cfg.listen_addr())
        .await
        .map_err(HttpServeError::Io)?;
    serve(listener, app, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::{log_upload_with_analyzer, LogUploadAnalyzer};
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use threski_core::auth::Credentials;
    use threski_ingest_logs::SemanticError;
    use rmcp::handler::server::tool::ToolRouter;
    use rmcp::model::{CallToolRequestParams, Implementation, ServerCapabilities, ServerConfig};
    use rmcp::service::{RequestContext, ServiceExt as _};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
    use serde_json::json;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    /// Minimal MCP handler standing in for the wave-4 registry ( tests used
    /// `server.NewMCPServer("t", "1")`). The `probe` tool reports the
    /// credentials the mux injected through the HTTP request extensions —
    /// the Rust replacement for the `WithHTTPContextFunc(InjectAuth)`.
    #[derive(Clone)]
    #[allow(dead_code)] // router is wired by the generated macros
    struct TestMcp {
        router: ToolRouter<TestMcp>,
    }

    #[tool_router]
    impl TestMcp {
        fn new() -> Self {
            Self {
                router: Self::tool_router(),
            }
        }

        #[tool(description = "report the injected HTTP credentials")]
        fn probe(&self, ctx: RequestContext<RoleServer>) -> String {
            let creds = ctx
                .extensions
                .get::<axum::http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<Credentials>());
            format!(
                "actor={:?} remote_untrusted={}",
                creds.and_then(|c| c.actor.clone()),
                creds.map(|c| c.remote_untrusted).unwrap_or(true)
            )
        }
    }

    #[tool_handler]
    impl ServerHandler for TestMcp {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::default())
                .with_server_info(Implementation::new("t", "1"))
        }
    }

    fn cfg() -> Arc<Config> {
        Arc::new(Config {
            port: 3030,
            runtime_mode: "personal".into(),
            ..Default::default()
        })
    }

    fn test_mcp_service() -> StreamableHttpService<TestMcp, LocalSessionManager> {
        StreamableHttpService::new(
            || Ok(TestMcp::new()),
            Arc::new(LocalSessionManager::default()),
            streamable_http_config(),
        )
    }

    fn app() -> Router {
        handler(
            cfg(),
            test_mcp_service(),
            crate::discovery::no_tools(),
            None,
            None,
            None,
        )
    }

    async fn oneshot(app: Router, request: axum::http::Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    fn get(path: &str, accept: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method("GET")
            .uri(path)
            .header(header::ACCEPT, accept)
            .body(Body::empty())
            .unwrap()
    }

    /// Scenario: TestDiscoveryJSONAndMarkdown through the mux.
    #[tokio::test]
    async fn discovery_json_and_markdown() {
        let resp = oneshot(app(), get("/", "application/json")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["endpoints"]["streamable_http"],
            "http://127.0.0.1:3030/mcp"
        );
        assert_eq!(
            body["endpoints"]["settings_ui"],
            "http://127.0.0.1:3030/settings/"
        );
        assert!(body["endpoints"].get("sse_legacy").is_none(), "D11");
        let vers = body["protocol_versions"].as_array().unwrap();
        assert!(vers.iter().any(|v| v == "2026-07-28"));

        let resp = oneshot(app(), get("/", "text/markdown")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("Threski Studio"));
    }

    /// Scenario: TestOriginRejectedOnMCP.
    #[tokio::test]
    async fn origin_rejected_on_mcp() {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = oneshot(app(), request).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes).trim(), "forbidden origin");
    }

    /// Loopback browser origins pass the gate and reach the MCP service.
    #[tokio::test]
    async fn loopback_origin_reaches_mcp() {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::ORIGIN, "http://localhost:5173")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from("not jsonrpc"))
            .unwrap();
        // The gate lets it through; the MCP service rejects the payload
        // (400-class response proves the gate did not fire).
        let resp = oneshot(app(), request).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Scenario: TestCLICallRouteRemoved.
    #[tokio::test]
    async fn cli_call_route_removed() {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/cli/call")
            .body(Body::from(r#"{"name":"ask_project"}"#))
            .unwrap();
        let resp = oneshot(app(), request).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes).trim(), "404 page not found");
    }

    /// D11: the legacy SSE routes are gone entirely.
    #[tokio::test]
    async fn sse_routes_removed() {
        for path in ["/sse", "/message"] {
            let resp = oneshot(app(), get(path, "*/*")).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path} must be gone");
        }
    }

    /// Trailing-slash MCP route reaches the service too ( mounted `/mcp/`).
    #[tokio::test]
    async fn mcp_trailing_slash_mounted() {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp/")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from("not jsonrpc"))
            .unwrap();
        let resp = oneshot(app(), request).await;
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Scenario: TestMCPServerDiscover (D11-updated): `server/discover`
    /// over Streamable HTTP returns the supported protocol versions.
    #[tokio::test]
    async fn mcp_server_discover() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            serve(listener, app(), server_shutdown).await.unwrap();
        });

        let client = reqwest::Client::new();
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let resp = client
            .post(format!("http://{addr}/mcp"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "server/discover")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let msg: serde_json::Value = resp.json().await.unwrap();
        let result = msg["result"].as_object().expect("discover result");
        let versions = result["supportedVersions"].as_array().expect("versions");
        assert!(
            versions.iter().any(|v| v == "2026-07-28"),
            "supportedVersions={versions:?}"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }

    /// The settings mount seams: API router merges (absolute paths), UI
    /// router nests (prefix stripped), upload nests.
    #[tokio::test]
    async fn settings_and_upload_mounts() {
        let settings_api = Router::new()
            .route(
                "/api/v1/settings",
                axum::routing::get(|| async { "api-root" }).post(|| async { "api-post" }),
            )
            .route(
                "/api/v1/settings/memory",
                axum::routing::get(|| async { "api-memory" }),
            );
        let settings_ui = Router::new()
            .route("/", axum::routing::get(|| async { "spa-index" }))
            .route("/app.js", axum::routing::get(|| async { "spa-app" }));
        let upload = log_upload_with_analyzer(cfg(), None, None, None);

        let app = handler(
            cfg(),
            test_mcp_service(),
            crate::discovery::no_tools(),
            Some(upload),
            Some(settings_api),
            Some(settings_ui),
        );

        let resp = oneshot(app.clone(), get("/api/v1/settings", "*/*")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, "api-root");

        let resp = oneshot(app.clone(), get("/api/v1/settings/memory", "*/*")).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = oneshot(app.clone(), get("/settings/", "*/*")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, "spa-index");

        let resp = oneshot(app.clone(), get("/settings/app.js", "*/*")).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, "spa-app");

        // parity: bare `/settings` redirects to `/settings/` (302).
        let resp = oneshot(app.clone(), get("/settings", "*/*")).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/settings/")
        );

        // Upload mounted: GET → 405 from the handler itself.
        let resp = oneshot(app, get("/api/v1/logs/upload", "*/*")).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// The auth middleware stamps Credentials into extensions, visible
    /// downstream (`Auth → `InjectAuth`).
    #[tokio::test]
    async fn auth_middleware_stamps_credentials() {
        // A marker route that reports what it saw in the extensions.
        async fn probe(
            State(creds): State<Arc<std::sync::Mutex<Credentials>>>,
            req: Request,
        ) -> Response {
            if let Some(seen) = req.extensions().get::<Credentials>() {
                *creds.lock().unwrap() = seen.clone();
            }
            StatusCode::OK.into_response()
        }
        let seen = Arc::new(std::sync::Mutex::new(Credentials::default()));
        let app = Router::new()
            .route(
                "/probe",
                axum::routing::post(probe).with_state(seen.clone()),
            )
            .layer(middleware::from_fn(auth_middleware));

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/probe")
            .header("X-Threski-Actor", "alice")
            .header("X-Redmine-API-Key", "rk")
            // Peer extension as the accept loop would stamp it (loopback).
            .extension(ConnectInfo::<SocketAddr>(
                "127.0.0.1:54321".parse().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(request).await.unwrap();
        let creds = seen.lock().unwrap().clone();
        assert_eq!(creds.actor.as_deref(), Some("alice"));
        assert_eq!(creds.redmine_key.as_deref(), Some("rk"));
        assert!(!creds.remote_untrusted);
    }

    /// A success upload through the full mux (fake analyzer seam), and 's
    /// empty-configured-key semantics: a missing `X-API-Key` header is 401
    /// even when the configured key is empty.
    #[tokio::test]
    async fn upload_success_through_mux() {
        struct OkAnalyzer;
        #[async_trait::async_trait]
        impl LogUploadAnalyzer for OkAnalyzer {
            async fn analyze(
                &self,
                _project: &str,
                _file_name: &str,
                _lines: &[String],
            ) -> Result<Vec<SemanticError>, String> {
                Ok(vec![SemanticError {
                    category: "cat".into(),
                    severity: 5,
                    ..Default::default()
                }])
            }
        }
        let mut upload_cfg = (*cfg()).clone();
        upload_cfg.log_ingestion.http.enabled = true;
        upload_cfg.log_ingestion.http.api_key = "secret-key".into();
        let upload_cfg = Arc::new(upload_cfg);
        let upload =
            log_upload_with_analyzer(upload_cfg.clone(), None, Some(Arc::new(OkAnalyzer)), None);
        let app = handler(
            cfg(),
            test_mcp_service(),
            crate::discovery::no_tools(),
            Some(upload),
            None,
            None,
        );
        // No X-API-Key header → 401 (-faithful).
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/logs/upload")
            .body(Body::from("ignored"))
            .unwrap();
        let resp = oneshot(app.clone(), request).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Matching key → success shape through the mux.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/logs/upload")
            .header("X-API-Key", "secret-key")
            .body(Body::from("log line"))
            .unwrap();
        let resp = oneshot(app, request).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "processed");
        assert_eq!(value["anomalies"][0]["category"], "cat");
    }

    /// Full legacy (initialize-handshake) MCP session over Streamable HTTP
    /// through the real serve stack — the exact path the `mcp-bridge` drives
    /// for Claude Desktop and wave-4 `threski-server` wires. Exercises the origin
    /// gate (client sends no Origin → allowed), the auth middleware and the
    /// rmcp session manager.
    #[tokio::test]
    async fn mcp_client_handshake_and_probe_tool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            serve(listener, app(), server_shutdown).await.unwrap();
        });

        let client = ()
            .serve(StreamableHttpClientTransport::from_uri(format!(
                "http://{addr}/mcp"
            )))
            .await
            .expect("client init");
        let tools = client
            .peer()
            .list_tools(Default::default())
            .await
            .expect("list_tools");
        assert!(
            tools.tools.iter().any(|t| t.name == "probe"),
            "probe tool listed: {tools:?}"
        );
        let result = client
            .peer()
            .call_tool(CallToolRequestParams::new("probe"))
            .await
            .expect("call probe");
        let text = match result.content.first() {
            Some(rmcp::model::ContentBlock::Text(t)) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        // Real loopback TCP peer: remote-untrusted must be false (the mux
        // injected Credentials through the request extensions), actor absent.
        assert_eq!(text, "actor=None remote_untrusted=false", "probe: {text}");

        let _ = client.cancel().await;
        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }

    /// The origin gate also guards the rmcp client path: a hostile Origin on
    /// the wire must be rejected before the MCP service sees the request.
    #[tokio::test]
    async fn origin_gate_blocks_rmcp_client_with_hostile_origin() {
        // Raw POST with an Origin header — a browser-borne request.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::ORIGIN, "http://10.0.0.5:3030")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from("{}"))
            .unwrap();
        let resp = oneshot(app(), request).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
