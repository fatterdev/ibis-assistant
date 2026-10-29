//! The lane-M coordination seam: the settings API Router merges (absolute
//! paths) and the settings UI Router nests (prefix stripped, relative routes)
//! into `threski_httpserver::mux::handler` exactly as wave-4 `threski-server` will
//! wire them.

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use threski_core::config::Config;
use threski_httpserver::discovery::no_tools;
use threski_httpserver::mux::{handler, streamable_http_config};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::ServerHandler;
use tempfile::TempDir;
use tower::ServiceExt;

/// Minimal MCP handler standing in for the wave-4 56-tool registry
/// (mirrors the mux's own test seam).
#[derive(Clone)]
struct TestMcp;

impl ServerHandler for TestMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::default())
            .with_server_info(Implementation::new("test", "1"))
    }
}

fn full_app(config_path: &str) -> (axum::Router, Arc<RwLock<Config>>) {
    let mux_cfg = Arc::new(Config {
        port: 3030,
        runtime_mode: "personal".into(),
        ..Default::default()
    });
    let mut settings_cfg = Config {
        port: 3030,
        runtime_mode: "personal".into(),
        ..Default::default()
    };
    settings_cfg.config_path = config_path.to_string();
    let lock = Arc::new(RwLock::new(settings_cfg));

    let settings_api = threski_settings::settings_api_router(lock.clone(), None);
    let settings_ui = threski_settings::settings_ui_router();
    let mcp = StreamableHttpService::new(
        || Ok(TestMcp),
        Arc::new(LocalSessionManager::default()),
        streamable_http_config(),
    );
    let app = handler(
        mux_cfg,
        mcp,
        no_tools(),
        None,
        Some(settings_api),
        Some(settings_ui),
    );
    (app, lock)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::ACCEPT, "*/*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ctype, String::from_utf8_lossy(&bytes).into_owned())
}

/// The absolute-path API merge and the relative-path UI nest work through the
/// real mux — the seam wave-4 `threski-server` wires.
#[tokio::test]
async fn settings_routers_slot_into_mux() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let (app, _) = full_app(&path.display().to_string());

    // GET /api/v1/settings (absolute, merged — no strip) → settings JSON.
    let (status, ctype, body) = get(&app, "/api/v1/settings").await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(ctype.starts_with("application/json"));
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(value["summary"].as_str().unwrap().contains("Tier"));
    assert!(value["config"]["port"].is_number());

    // GET /settings/ (nested, stripped) → index.html.
    let (status, ctype, body) = get(&app, "/settings/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype, "text/html; charset=utf-8");
    assert!(body.contains("Threski Studio Settings"));

    // Assets under the stripped prefix.
    let (status, ctype, _) = get(&app, "/settings/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype, "text/javascript; charset=utf-8");
    let (status, ctype, _) = get(&app, "/settings/style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype, "text/css; charset=utf-8");

    // Bare /settings redirects ( 302).
    let (status, _, _) = get(&app, "/settings").await;
    assert_eq!(status, StatusCode::FOUND);

    // Unknown API subpath → ServeMux 404 text.
    let (status, _, body) = get(&app, "/api/v1/settings/bogus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.trim(), "404 page not found");
}

/// POST through the mux writes config.json and updates the shared lock —
/// the same shared state the MCP `settings_apply` tool will read.
#[tokio::test]
async fn post_settings_through_mux() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let (app, lock) = full_app(&path.display().to_string());

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/settings")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"ai_mode":"none","local_model":"auto"}"#))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["path"], path.display().to_string());
    assert!(path.exists(), "config written");
    assert_eq!(lock.read().unwrap().ai.reasoning.provider, "none");
}

/// The mux still serves MCP beside the settings mounts (no route conflicts
/// from the merge).
#[tokio::test]
async fn mcp_unaffected_by_settings_merge() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let (app, _) = full_app(&path.display().to_string());
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from("not jsonrpc"))
        .unwrap();
    let resp = app.oneshot(request).await.unwrap();
    assert_ne!(resp.status(), StatusCode::NOT_FOUND, "mcp route alive");
}
