//! Discovery snapshot gate (D3) — `GET /` with `Accept: application/json`
//! must serve the committed `tools[]` fixture: 56 rows `{name, description,
//! input_schema}` sorted by name, matching the D3 registry byte-for-byte.
//!
//! The fixture was produced by the live discovery handler (see the
//! `#[ignore]`d `generate_discovery_fixture` below) and verified against the
//! reference oracle (the reference server binary on a free port, same request) — see the
//! wave-4 gates report for the recorded deltas (`sse_legacy` removal per
//! D11/F-005, the `required:[]` marshaling artifact). Regenerate the
//! fixture only for deliberate registry changes (D15 lockstep):
//!
//! `text
//! cargo test -p ibis-server --test discovery_snapshot_test -- --ignored generate_discovery_fixture
//! `

use std::sync::Arc;

use axum::Router;
use ibis_core::config::Config;
use ibis_httpserver::{handler, serve, streamable_http_config};
use ibis_server::registry;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

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

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("discovery_tools.json")
}

/// The assembled discovery app: the real mux with the real 56-tool
/// `registry::discovery_tools` provider (D3).
fn app() -> Router {
    let cfg = Arc::new(Config {
        port: 3030,
        runtime_mode: "server".into(),
        ..Default::default()
    });
    let mcp: StreamableHttpService<TestMcp, LocalSessionManager> = StreamableHttpService::new(
        || Ok(TestMcp::new()),
        Arc::new(LocalSessionManager::default()),
        streamable_http_config(),
    );
    handler(cfg, mcp, registry::discovery_tools(), None, None, None)
}

/// Serves `app` on a free loopback port and GETs `/` with
/// `Accept: application/json` — the live handler chain.
async fn live_discovery() -> Value {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = serve(listener, app(), server_shutdown).await;
    });
    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    shutdown.cancel();
    body
}

fn tools_of(doc: &Value) -> Vec<Value> {
    doc["tools"]
        .as_array()
        .expect("discovery tools[] array")
        .clone()
}

fn assert_tools_shape(tools: &[Value]) {
    assert_eq!(tools.len(), 56, "the D3 registry has exactly 56 tools");
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let sorted = names.clone();
    names.sort_unstable();
    assert_eq!(names, sorted, "tools[] must be sorted by name");
    names.dedup();
    assert_eq!(names.len(), 56, "tool names are unique");
    for tool in tools {
        let row = tool.as_object().expect("tool row object");
        assert_eq!(
            row.len(),
            3,
            "tool row carries exactly {{name, description, input_schema}}: {tool}"
        );
        for key in ["name", "description", "input_schema"] {
            assert!(row.contains_key(key), "missing {key} in {tool}");
        }
        let schema = &tool["input_schema"];
        assert_eq!(schema["type"], "object", "{}", tool["name"]);
        assert!(
            schema["properties"].is_object(),
            "{} properties object",
            tool["name"]
        );
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
}

/// D3 gate: the live discovery `tools[]` matches the committed fixture
/// exactly (sorted, exact fields, schemas identical).
#[tokio::test]
async fn live_discovery_matches_fixture() {
    let doc = live_discovery().await;
    let tools = tools_of(&doc);
    assert_tools_shape(&tools);

    let fixture_raw = std::fs::read_to_string(fixture_path()).expect("committed fixture");
    let fixture: Value = serde_json::from_str(&fixture_raw).expect("fixture parses");
    let fixture_tools = tools_of(&fixture);

    assert_eq!(
        serde_json::to_string(&tools).unwrap(),
        serde_json::to_string(&fixture_tools).unwrap(),
        "live discovery tools[] drifted from tests/fixtures/discovery_tools.json \
         (regenerate only for a deliberate registry change, D15 lockstep)"
    );

    // The rest of the document keeps the -discovery shape minus the
    // deliberate deltas (D11/F-005: no `sse_legacy`; guide text speaks the
    // Rust build).
    assert_eq!(doc["status"], "online");
    assert_eq!(doc["name"], "Ibis Assistant");
    assert_eq!(
        doc["endpoints"]["streamable_http"],
        "http://127.0.0.1:3030/mcp"
    );
    assert_eq!(
        doc["endpoints"]["log_push_upload"],
        "http://127.0.0.1:3030/api/v1/logs/upload"
    );
    assert!(
        doc["endpoints"].get("sse_legacy").is_none(),
        "D11: sse_legacy must be gone: {}",
        doc["endpoints"]
    );
    let vers = doc["protocol_versions"].as_array().unwrap();
    assert_eq!(
        Value::Array(vers.clone()),
        serde_json::json!([
            "2026-07-28",
            "2025-11-25",
            "2025-06-18",
            "2025-03-26",
            "2024-11-05"
        ]),
        "protocol_versions newest-first (matches the reference order)"
    );
    assert!(
        doc["guide_markdown"]
            .as_str()
            .unwrap()
            .contains("init_project"),
        "guide enumerates the registered tools"
    );
}

/// Fixture generator (deliberate registry changes only — D15 lockstep).
/// Writes the live discovery `tools[]` snapshot to
/// `tests/fixtures/discovery_tools.json`. Run with:
/// `cargo test -p ibis-server --test discovery_snapshot_test -- --ignored generate_discovery_fixture`
#[tokio::test]
#[ignore = "fixture generator: run explicitly to refresh tests/fixtures/discovery_tools.json"]
async fn generate_discovery_fixture() {
    let doc = live_discovery().await;
    let tools = tools_of(&doc);
    assert_tools_shape(&tools);
    let pretty = serde_json::to_string_pretty(&serde_json::json!({ "tools": tools })).unwrap();
    let path = fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, pretty + "\n").unwrap();
    println!("wrote {}", path.display());
}
