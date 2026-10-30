//! H0 spike (b): rmcp stdio-style duplex handshake + Streamable HTTP server.
//!
//! Test 1 exercises the server handler + tool macros over an in-memory duplex
//! pipe (the same NDJSON framing stdio uses). Test 2 mounts the Streamable
//! HTTP tower service on axum and connects the rmcp streamable-http client —
//! the automated equivalent of driving the endpoint from MCP Inspector.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, ContentBlock, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::service::ServiceExt;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::json;

#[derive(Clone)]
#[allow(dead_code)] // router is wired by the generated macros
struct SpikeServer {
    router: ToolRouter<SpikeServer>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
struct EchoArgs {
    msg: String,
}

#[tool_router]
impl SpikeServer {
    fn new() -> Self {
        Self {
            router: Self::tool_router(),
        }
    }

    #[tool(description = "echo the provided message")]
    fn echo(&self, Parameters(EchoArgs { msg }): Parameters<EchoArgs>) -> String {
        format!("echo: {msg}")
    }
}

#[tool_handler]
impl ServerHandler for SpikeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::default())
            .with_server_info(Implementation::new("spike", "0.0.0"))
    }
}

#[tokio::test]
async fn spike_duplex_handshake_and_tool_call() {
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let service = SpikeServer::new().serve(server_stream).await.unwrap();
        let _ = service.waiting().await;
    });

    let client = ().serve(client_stream).await.expect("client init");
    let tools = client
        .peer()
        .list_tools(Default::default())
        .await
        .expect("list_tools");
    assert!(
        tools.tools.iter().any(|t| t.name == "echo"),
        "echo tool listed: {tools:?}"
    );

    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(json!({ "msg": "hello ibis" }).as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool");
    let text = match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert_eq!(text, "echo: hello ibis");
    let _ = client.cancel().await;
    server.abort();
}

#[tokio::test]
async fn spike_streamable_http_handshake_and_tool_call() {
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/mcp");

    let service: StreamableHttpService<SpikeServer, LocalSessionManager> =
        StreamableHttpService::new(
            || Ok(SpikeServer::new()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
    let app = axum::Router::new().route_service("/mcp", service);

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client =
        ().serve(StreamableHttpClientTransport::from_uri(url))
            .await
            .expect("streamable http client init");

    let tools = client
        .peer()
        .list_tools(Default::default())
        .await
        .expect("list_tools");
    assert!(
        tools.tools.iter().any(|t| t.name == "echo"),
        "echo tool listed: {tools:?}"
    );

    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("echo")
                .with_arguments(json!({ "msg": "via http" }).as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool");
    let text = match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert_eq!(text, "echo: via http");

    let _ = client.cancel().await;
    server.abort();
}
