//! Log-upload gate, driven through
//! the **assembled** server (the real `threski_httpserver::mux::handler` chain:
//! auth middleware → mux → nested upload router), not the bare handler.
//!
//! built `logUploadHandler(cfg, dbMock, nil)` and served `httptest`
//! requests; the Rust gate mounts the same seams in-process on a real TCP
//! listener (spawned exe not needed — the mux assembly is exposed by the
//! library and the handler chain is identical to `run --mode sse`):
//! - cases 1/2/4 mirror verbatim (401 missing key, 405 GET, 403 disabled);
//! - case 3 uses the REAL analyzer wiring (`log_upload_handler` with a mock
//!   `Executor` + nil AI, exactly the `dbMock`/`nil` aiClient) and asserts
//!   the `AI client is not functional` 500;
//! - the endpoint-contract scenarios from (wrong key, JSON/raw
//!   bodies, `text` splitting, defaults, 413 plain-text protocol,
//!   `truncated` flag) ride the same assembled mux through the
//!   `log_upload_with_analyzer` seam ( pinned these in
//!   behavior; pins the endpoint contract).

use std::sync::{Arc, Mutex};

use axum::Router;
use threski_core::config::Config;
use threski_db::{DbError, Executor, QueryOutcome, Vars};
use threski_httpserver::upload::{log_upload_handler, log_upload_with_analyzer, LogUploadAnalyzer};
use threski_httpserver::{handler, serve, streamable_http_config};
use threski_ingest_logs::SemanticError;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Minimal MCP handler standing in for the 56-tool registry (the upload
/// scenarios never touch the MCP surface; the full registry rides the same
/// mux in `discovery_snapshot_test.rs` and the e2e gate).
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

    #[tool(description = "stand-in for the registry")]
    fn probe(&self, _ctx: RequestContext<RoleServer>) -> String {
        "ok".to_string()
    }
}

#[tool_handler]
impl ServerHandler for TestMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::default())
            .with_server_info(Implementation::new("t", "1"))
    }
}

fn mcp_service() -> StreamableHttpService<TestMcp, LocalSessionManager> {
    StreamableHttpService::new(
        || Ok(TestMcp::new()),
        Arc::new(LocalSessionManager::default()),
        streamable_http_config(),
    )
}

/// `cfg := &config.Config{LogsRoot: "test_logs", LogIngestion.HTTP{Enabled: true, APIKey: "secret-key"}}.
fn upload_cfg(enabled: bool) -> Arc<Config> {
    Arc::new(Config {
        logs_root: "test_logs".into(),
        log_ingestion: threski_core::config::LogIngestionConfig {
            http: threski_core::config::HTTPIngestionConfig {
                enabled,
                api_key: "secret-key".into(),
            },
            ..Default::default()
        },
        ..Default::default()
    })
}

/// `MockUploadDB: captures every statement; SELECTs decode as empty
/// result sets ( returned `[]interface{}{}`).
struct MockUploadDB {
    captured: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Executor for MockUploadDB {
    async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
        self.captured.lock().unwrap().push(sql.to_string());
        Ok(QueryOutcome::Single(threski_db::value::Value::None))
    }

    async fn smart_query(&self, sql: &str, _vars: Vars) -> Result<QueryOutcome, DbError> {
        self.captured.lock().unwrap().push(sql.to_string());
        Ok(QueryOutcome::Single(threski_db::value::Value::None))
    }
}

/// Inputs captured by the fake analyzer: (project, file_name, lines).
type Captured = (String, String, Vec<String>);

struct FakeAnalyzer {
    result: Result<Vec<SemanticError>, String>,
    seen: Arc<Mutex<Option<Captured>>>,
}

#[async_trait::async_trait]
impl LogUploadAnalyzer for FakeAnalyzer {
    async fn analyze(
        &self,
        project: &str,
        file_name: &str,
        lines: &[String],
    ) -> Result<Vec<SemanticError>, String> {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some((project.to_string(), file_name.to_string(), lines.to_vec()));
        }
        self.result.clone()
    }
}

fn fake(
    result: Result<Vec<SemanticError>, String>,
    seen: Arc<Mutex<Option<Captured>>>,
) -> Arc<dyn LogUploadAnalyzer> {
    Arc::new(FakeAnalyzer { result, seen })
}

fn capture() -> Arc<Mutex<Option<Captured>>> {
    Arc::new(Mutex::new(None))
}

/// Assembles the full mux (discovery + MCP + upload nest) and serves it on a
/// free loopback port. Returns the base URL and the shutdown token.
async fn serve_assembled(upload: Router) -> (String, CancellationToken) {
    let cfg = Arc::new(Config {
        port: 3030,
        runtime_mode: "personal".into(),
        ..Default::default()
    });
    let app = handler(
        cfg,
        mcp_service(),
        threski_httpserver::discovery::no_tools(),
        Some(upload),
        None,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = serve(listener, app, server_shutdown).await;
    });
    (format!("http://{addr}"), shutdown)
}

async fn stop(base: String, shutdown: CancellationToken) {
    shutdown.cancel();
    drop(base);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

/// POST with headers; returns (status, content-type, body).
async fn post(
    base: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{base}{uri}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.body(body.to_string()).send().await.unwrap();
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (status, content_type, resp.text().await.unwrap())
}

async fn get(base: &str, headers: &[(&str, &str)]) -> u16 {
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{base}/api/v1/logs/upload"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.unwrap();
    resp.status().as_u16()
}

/// Scenario: case 1: Unauthorized (missing API key) → 401.
#[tokio::test]
async fn case1_missing_api_key_unauthorized() {
    let (base, shutdown) =
        serve_assembled(log_upload_with_analyzer(upload_cfg(true), None, None, None)).await;
    let (status, _, body) = post(&base, "/api/v1/logs/upload", &[], "log line").await;
    assert_eq!(status, 401, "expected 401, got {status}");
    assert!(
        body.contains("Unauthorized: invalid or missing API key"),
        "body: {body}"
    );
    stop(base, shutdown).await;
}

/// Scenario: case 2: Method not allowed (GET with a valid key) → 405.
#[tokio::test]
async fn case2_get_method_not_allowed() {
    let (base, shutdown) =
        serve_assembled(log_upload_with_analyzer(upload_cfg(true), None, None, None)).await;
    let status = get(&base, &[("X-API-Key", "secret-key")]).await;
    assert_eq!(status, 405, "expected 405, got {status}");
    stop(base, shutdown).await;
}

/// Scenario: case 3: JSON body through the real analyzer wiring (mock DB + nil AI
/// client) → 500 with `Analysis failed: AI client is not functional`.
#[tokio::test]
async fn case3_json_ai_not_functional() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mock = MockUploadDB {
        captured: captured.clone(),
    };
    let (base, shutdown) = serve_assembled(log_upload_handler(
        upload_cfg(true),
        Some(Arc::new(mock)),
        None,
    ))
    .await;
    let body = json!({
        "project": "project-x",
        "file_name": "upload.log",
        "lines": ["error line 1", "error line 2"],
    });
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[
            ("X-API-Key", "secret-key"),
            ("Content-Type", "application/json"),
        ],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, 500, "expected 500, got {status}");
    assert!(
        resp.contains("Analysis failed: AI client is not functional"),
        "expected 'AI client is not functional' body, got {resp:?}"
    );
    // The mock DB sat in the analyzer path (the MockUploadDB contract: the
    // SELECT for known patterns must come back as an empty result set).
    let statements = captured.lock().unwrap().clone();
    assert!(
        statements.iter().any(|s| s.contains("SELECT")),
        "mock DB never saw the analyzer SELECT: {statements:?}"
    );
    stop(base, shutdown).await;
}

/// Scenario: case 4: ingestion disabled → 403.
#[tokio::test]
async fn case4_ingestion_disabled_forbidden() {
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(false),
        None,
        None,
        None,
    ))
    .await;
    let (status, _, body) = post(
        &base,
        "/api/v1/logs/upload",
        &[("X-API-Key", "secret-key")],
        "log line",
    )
    .await;
    assert_eq!(status, 403, "expected 403, got {status}");
    assert!(
        body.contains("HTTP Log Ingestion is disabled in configuration"),
        "body: {body}"
    );
    stop(base, shutdown).await;
}

/// Wrong X-API-Key value → 401 (same unauthorized protocol as a missing key).
#[tokio::test]
async fn wrong_api_key_unauthorized() {
    let (base, shutdown) =
        serve_assembled(log_upload_with_analyzer(upload_cfg(true), None, None, None)).await;
    let (status, _, _) = post(
        &base,
        "/api/v1/logs/upload",
        &[("X-API-Key", "nope")],
        "log line",
    )
    .await;
    assert_eq!(status, 401, "expected 401, got {status}");
    stop(base, shutdown).await;
}

/// JSON `{project, file_name, lines[]}` success: response shape
/// `{status:"processed", project, file_name, anomalies[]}`; lines are
/// trimmed/emptied before analysis; no `truncated` field when uncapped.
#[tokio::test]
async fn json_success_shape() {
    let anomalies = vec![SemanticError {
        category: "timeout".into(),
        stack_trace: "boom".into(),
        severity: 7,
        ..Default::default()
    }];
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(anomalies), seen.clone())),
        None,
    ))
    .await;
    let body = json!({
        "project": "project-x",
        "file_name": "upload.log",
        "lines": ["  error line 1  ", "", "error line 2"],
    });
    let (status, content_type, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[
            ("X-API-Key", "secret-key"),
            ("Content-Type", "application/json"),
        ],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    assert!(
        content_type.starts_with("application/json"),
        "{content_type}"
    );
    let value: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(value["status"], "processed");
    assert_eq!(value["project"], "project-x");
    assert_eq!(value["file_name"], "upload.log");
    assert_eq!(value["anomalies"][0]["category"], "timeout");
    assert_eq!(value["anomalies"][0]["severity"], 7);
    assert!(value.get("truncated").is_none(), "uncapped: {value}");
    let captured = seen.lock().unwrap().clone().unwrap();
    assert_eq!(captured.0, "project-x");
    assert_eq!(captured.1, "upload.log");
    assert_eq!(captured.2, vec!["error line 1", "error line 2"]);
    stop(base, shutdown).await;
}

/// JSON `text` field splits into lines when provided
#[tokio::test]
async fn json_text_field_splits_lines() {
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(vec![]), seen.clone())),
        None,
    ))
    .await;
    let body = json!({ "project": "p", "text": "one\ntwo\n" });
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[
            ("X-API-Key", "secret-key"),
            ("Content-Type", "application/json"),
        ],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    let captured = seen.lock().unwrap().clone().unwrap();
    assert_eq!(captured.2, vec!["one", "two"]);
    stop(base, shutdown).await;
}

/// Raw body with `?project=&file_name=` query params
#[tokio::test]
async fn raw_body_with_query_params() {
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(vec![]), seen.clone())),
        None,
    ))
    .await;
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload?project=raw-proj&file_name=app.log",
        &[("X-API-Key", "secret-key")],
        "line one\nline two",
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    let value: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(value["status"], "processed");
    assert_eq!(value["project"], "raw-proj");
    assert_eq!(value["file_name"], "app.log");
    assert_eq!(value["anomalies"], json!([]));
    assert!(value.get("truncated").is_none(), "uncapped: {value}");
    let captured = seen.lock().unwrap().clone().unwrap();
    assert_eq!(captured.2, vec!["line one", "line two"]);
    stop(base, shutdown).await;
}

/// Defaults `HTTPUpload` / `uploaded.log` when the raw request carries none
///
#[tokio::test]
async fn raw_defaults_http_upload_uploaded_log() {
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(vec![]), seen.clone())),
        None,
    ))
    .await;
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[("X-API-Key", "secret-key")],
        "line one",
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    let value: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(value["project"], "HTTPUpload");
    assert_eq!(value["file_name"], "uploaded.log");
    let captured = seen.lock().unwrap().clone().unwrap();
    assert_eq!(captured.0, "HTTPUpload");
    assert_eq!(captured.1, "uploaded.log");
    stop(base, shutdown).await;
}

/// Empty cleaned lines → success without touching the analyzer (
/// early return).
#[tokio::test]
async fn raw_empty_lines_success_without_analyzer() {
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(vec![]), seen.clone())),
        None,
    ))
    .await;
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload?project=raw-proj&file_name=app.log",
        &[("X-API-Key", "secret-key")],
        "\n   \n",
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    let value: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(value["anomalies"], json!([]));
    assert!(
        seen.lock().unwrap().is_none(),
        "analyzer must not run on an empty batch"
    );
    stop(base, shutdown).await;
}

/// Oversized JSON body → **plain-text** 413 `Request body exceeds <N> byte
/// limit` ( decode-failure protocol; the cap is injected
/// small — the production default is 32 MiB).
#[tokio::test]
async fn json_oversized_plain_text_413() {
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(Ok(vec![]), capture())),
        Some(16),
    ))
    .await;
    let body = json!({ "project": "p", "lines": ["y".repeat(64)] });
    let (status, content_type, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[
            ("X-API-Key", "secret-key"),
            ("Content-Type", "application/json"),
        ],
        &body.to_string(),
    )
    .await;
    assert_eq!(status, 413, "got {resp}");
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert_eq!(
        resp.trim_end(),
        "Request body exceeds 16 byte limit",
        "413 must be plain text, got {resp:?}"
    );
    stop(base, shutdown).await;
}

/// Oversized raw body: analysis proceeds on the capped prefix and the success
/// response carries `truncated:true` + `warning` — only when the body was
/// capped
#[tokio::test]
async fn raw_truncated_flag_and_warning() {
    let seen = capture();
    let (base, shutdown) = serve_assembled(log_upload_with_analyzer(
        upload_cfg(true),
        None,
        Some(fake(
            Ok(vec![SemanticError {
                category: "err".into(),
                severity: 3,
                ..Default::default()
            }]),
            seen.clone(),
        )),
        Some(16),
    ))
    .await;
    let big = "x".repeat(64);
    let (status, _, resp) = post(
        &base,
        "/api/v1/logs/upload",
        &[("X-API-Key", "secret-key")],
        &big,
    )
    .await;
    assert_eq!(status, 200, "got {resp}");
    let value: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(value["truncated"], json!(true));
    assert!(
        value["warning"]
            .as_str()
            .unwrap()
            .contains("Body truncated at 16 bytes"),
        "warning missing: {value}"
    );
    let captured = seen.lock().unwrap().clone().unwrap();
    assert_eq!(captured.2, vec!["x".repeat(16)], "capped prefix analyzed");
    stop(base, shutdown).await;
}
