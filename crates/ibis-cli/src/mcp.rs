//! MCP client glue.
//!
//! The CLI drives MCP over **Streamable HTTP only**
//! (`mcpclient.NewStreamableHttpClient(mcpEndpoint)`) with:
//!
//! - endpoint = `http://127.0.0.1:<port>/mcp`, port read from a minimal
//!   decode of `config.json` in the process CWD, default 3030 ;
//! - `Meta.ProgressToken = payload.Name` on every `tools/call`
//!   so the server's throttled progress notifications are subscribed;
//! - a notification handler printing `notifications/progress` lines to
//!   stderr ;
//! - Ctrl+C/SIGTERM cancellation of the in-flight call ;
//! - tool content printed to stdout (or stderr for error results), with
//!   JSON-looking text re-printed through a 2-space `MarshalIndent`
//!
//!
//! The rmcp client performs the initialize handshake inside `serve`
//! (equivalent of the `Start` + `Initialize`); this crate adds no timeout,
//! likewise. There is no stdio transport in the CLI client — stdio is a
//! server-side binding.

use std::io::Write;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    NumberOrString, ProgressToken, RequestParamsMeta,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService, ServiceExt};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
use rmcp::ClientHandler;
use serde::Deserialize;
use serde_json::Value;

use crate::{CliError, ToolCallPayload};

/// Code-default port.
const DEFAULT_HTTP_PORT: i64 = 3030;

/// Minimal `config.json` decode: only the `port` key.
#[derive(Debug, Deserialize)]
struct PortOnlyConfig {
    #[serde(default)]
    port: i64,
}

/// `mcpEndpoint` — Streamable HTTP URL of a running Ibis Assistant server.
///
/// Reads `config.json` from the process CWD; a missing file or a missing/zero
/// `port` falls back to 3030. Unlike the reference behavior, a malformed `config.json` prints a
/// stderr warning instead of being swallowed silently (L-A06).
pub fn default_mcp_endpoint() -> String {
    default_mcp_endpoint_from_config("config.json")
}

fn default_mcp_endpoint_from_config(path: &str) -> String {
    let mut port = DEFAULT_HTTP_PORT;
    if let Ok(contents) = std::fs::read_to_string(path) {
        match serde_json::from_str::<PortOnlyConfig>(&contents) {
            Ok(config) if config.port > 0 => port = config.port,
            Ok(_) => {}
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "Warning: ignoring malformed {path}: {error}"
                );
            }
        }
    }
    format!("http://127.0.0.1:{port}/mcp")
}

/// Client handler: advertises the original CLI identity and renders progress
/// notifications (the `c.OnNotification` callback).
struct ProgressPrinter;

impl ClientHandler for ProgressPrinter {
    fn get_info(&self) -> ClientConfig {
        // Scenario: InitializeParams ClientInfo: name "ibis-cli", version
        // "1.1.0" ; LATEST protocol version negotiated by rmcp.
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("ibis-cli", "1.1.0"),
        )
    }

    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Scenario: skips notifications without a message
        let Some(message) = params.message.as_deref().filter(|m| !m.is_empty()) else {
            return;
        };
        let rendered = match params.total {
            Some(total) => format!("[progress {}/{}] {}", params.progress, total, message),
            None => format!("[progress] {message}"),
        };
        let _ = writeln!(std::io::stderr(), "{rendered}");
    }
}

/// `callServerTool` — forward a CLI subcommand as MCP `tools/call` over
/// Streamable HTTP against the default endpoint, cancelling the in-flight
/// call on Ctrl+C/SIGTERM (the signal.NotifyContext).
///
/// The caller (wave-4 binary) is responsible for printing the returned error
/// to stderr and exiting 1; this function itself never exits the process.
pub async fn call_server_tool(payload: ToolCallPayload) -> Result<(), CliError> {
    let endpoint = default_mcp_endpoint();
    tokio::select! {
        result = call_mcp_tool(&endpoint, &payload) => result,
        _ = interrupt_signal() => Err(CliError::Transport(format!(
            "MCP tools/call {} failed: context canceled",
            payload.name
        ))),
    }
}

/// Ctrl+C (all platforms) and SIGTERM (unix) — the os.Interrupt + SIGTERM set.
async fn interrupt_signal() {
    #[cfg(unix)]
    {
        let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term {
                    Ok(mut sig) => { sig.recv().await; }
                    Err(_) => std::future::pending::<()>().await,
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `callMCPTool` — connect, initialize, call the tool, print the result.
///
/// Errors carry the original messages: "failed to start MCP client: …\nMake sure
/// the server is running (ibis start or ibis run)." and
/// "MCP tools/call <tool> failed: …"
pub async fn call_mcp_tool(endpoint: &str, payload: &ToolCallPayload) -> Result<(), CliError> {
    // the NewStreamableHttpClient + Start + Initialize map to rmcp's
    // initialize-performing serve.
    let service = ProgressPrinter
        .serve(StreamableHttpClientTransport::from_uri(endpoint))
        .await
        .map_err(|error| {
            CliError::Transport(format!(
                "failed to start MCP client: {error}\nMake sure the server is running (ibis start or ibis run)."
            ))
        })?;
    let result = match call_tool(&service, payload).await {
        Ok(result) => result,
        Err(error) => {
            // the `defer c.Close`.
            let _ = service.cancel().await;
            return Err(error);
        }
    };
    // the `defer c.Close`.
    let _ = service.cancel().await;
    if result.is_error.unwrap_or(false) {
        eprintln!("Tool returned an error:");
        let mut stderr = std::io::stderr().lock();
        print_tool_content(&mut stderr, &result);
        return Err(CliError::Transport(format!("tool {} failed", payload.name)));
    }
    let mut stdout = std::io::stdout().lock();
    print_tool_content(&mut stdout, &result);
    Ok(())
}

async fn call_tool(
    service: &RunningService<RoleClient, ProgressPrinter>,
    payload: &ToolCallPayload,
) -> Result<CallToolResult, CliError> {
    let mut params =
        CallToolRequestParams::new(payload.name.clone()).with_arguments(payload.arguments.clone());
    // Meta{ProgressToken: payload.Name} — the tool name doubles as the
    // progress token . Single call per process, so the token
    // cannot collide (L-A08, parity kept).
    params.set_progress_token(ProgressToken(NumberOrString::String(Arc::from(
        payload.name.as_str(),
    ))));
    service.call_tool(params).await.map_err(|error| {
        CliError::Transport(format!("MCP tools/call {} failed: {error}", payload.name))
    })
}

/// `printToolContent` — text blocks pass through `printMaybeJSON`; other
/// content blocks are pretty-printed as JSON
fn print_tool_content<W: Write>(w: &mut W, result: &CallToolResult) {
    for item in &result.content {
        match item {
            rmcp::model::ContentBlock::Text(text) => print_maybe_json(w, &text.text),
            other => match serde_json::to_value(other)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
            {
                Some(pretty) => {
                    let _ = writeln!(w, "{pretty}");
                }
                // the fallback prints the value with %v.
                None => {
                    let _ = writeln!(w, "{other:?}");
                }
            },
        }
    }
}

/// `printMaybeJSON` — re-print JSON-looking text through a 2-space indent
/// pretty-printer; anything else is written verbatim
///
/// Note: the `json.MarshalIndent` HTML-escapes `<`, `>`, `&`; serde_json
/// does not (cosmetic drift, L-A09).
fn print_maybe_json<W: Write>(w: &mut W, text: &str) {
    let trimmed = text.trim();
    let looks_like_json = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if looks_like_json {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                let _ = writeln!(w, "{pretty}");
                return;
            }
        }
    }
    let _ = writeln!(w, "{text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, ContentBlock, ErrorCode, ServerCapabilities,
        ServerConfig,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use rmcp::ServerHandler;
    use serde_json::json;

    /// Stub MCP server mirroring : one `echo_tool` that asserts
    /// the CLI sent a progress token and echoes `msg` as a JSON text result.
    #[derive(Clone)]
    struct EchoServer {
        saw_progress_token: Arc<AtomicBool>,
    }

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("t", "1"))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
            if request.name != "echo_tool" {
                // Same shape as rmcp's own router for unknown tools.
                return Err(rmcp::model::ErrorData::invalid_params(
                    "tool not found",
                    None,
                ));
            }
            if context.meta.get_progress_token().is_some() {
                self.saw_progress_token.store(true, Ordering::SeqCst);
            }
            let msg = request
                .arguments
                .as_ref()
                .and_then(|args| args.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(format!(r#"{{"ok":true,"msg":"{msg}"}}"#)),
            ])))
        }
    }

    fn arguments(pairs: serde_json::Value) -> serde_json::Map<String, Value> {
        pairs.as_object().expect("object").clone()
    }

    fn empty_arguments() -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }

    async fn spawn_stub_server(
        saw_progress_token: Arc<AtomicBool>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/mcp");
        let server = EchoServer { saw_progress_token };
        let service: StreamableHttpService<EchoServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            );
        let app = axum::Router::new().route_service("/mcp", service);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, handle)
    }

    /// TestCallMCPToolOverStreamableHTTP: success path + progress token sent.
    #[tokio::test]
    async fn call_mcp_tool_over_streamable_http() {
        let saw_progress_token = Arc::new(AtomicBool::new(false));
        let (url, server) = spawn_stub_server(saw_progress_token.clone()).await;

        let result = call_mcp_tool(
            &url,
            &ToolCallPayload::new("echo_tool", arguments(json!({ "msg": "hi" }))),
        )
        .await;
        assert!(result.is_ok(), "call_mcp_tool failed: {result:?}");
        assert!(
            saw_progress_token.load(Ordering::SeqCst),
            "missing progressToken"
        );

        server.abort();
    }

    /// TestCallMCPToolUnknownTool: calling an absent tool errors.
    #[tokio::test]
    async fn call_mcp_tool_unknown_tool() {
        let (url, server) = spawn_stub_server(Arc::new(AtomicBool::new(false))).await;

        let result = call_mcp_tool(&url, &ToolCallPayload::new("nope", empty_arguments())).await;
        assert!(result.is_err(), "expected error, got {result:?}");
        assert!(matches!(result.unwrap_err(), CliError::Transport(_)));

        server.abort();
    }

    /// TestDefaultMCPEndpoint: the endpoint path is `/mcp`.
    #[test]
    fn default_mcp_endpoint_contains_mcp_path() {
        let endpoint = default_mcp_endpoint();
        assert!(endpoint.contains("/mcp"), "endpoint {endpoint}");
    }

    /// Extra pin for L-A06: a valid config.json port overrides the default.
    #[tokio::test]
    async fn default_mcp_endpoint_reads_config_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"port": 4444}"#).unwrap();
        let endpoint = default_mcp_endpoint_from_config(path.to_str().unwrap());
        assert_eq!(endpoint, "http://127.0.0.1:4444/mcp");
    }

    /// Extra pin for L-A06: malformed config.json falls back to 3030.
    #[tokio::test]
    async fn default_mcp_endpoint_falls_back_on_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "not json").unwrap();
        let endpoint = default_mcp_endpoint_from_config(path.to_str().unwrap());
        assert_eq!(
            endpoint,
            format!("http://127.0.0.1:{DEFAULT_HTTP_PORT}/mcp")
        );
    }

    /// The progress-token wire value is the tool name, like the Meta.
    #[test]
    fn progress_token_is_the_tool_name() {
        let payload = ToolCallPayload::new("echo_tool", empty_arguments());
        let mut params = CallToolRequestParams::new(payload.name.clone());
        params.set_progress_token(ProgressToken(NumberOrString::String(Arc::from(
            payload.name.as_str(),
        ))));
        assert_eq!(
            params
                .progress_token()
                .map(|token| token.0.into_json_value()),
            Some(json!("echo_tool"))
        );
    }

    /// printMaybeJSON pretty-prints JSON-looking text and passes other text
    /// through verbatim.
    #[test]
    fn print_maybe_json_formatting() {
        // Keys sort alphabetically, exactly like the map[string]interface{}
        // re-marshal through MarshalIndent
        let mut out: Vec<u8> = Vec::new();
        print_maybe_json(&mut out, "{\"ok\":true,\"msg\":\"hi\"}");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\n  \"msg\": \"hi\",\n  \"ok\": true\n}\n"
        );

        let mut out: Vec<u8> = Vec::new();
        print_maybe_json(&mut out, "plain analysis text\nwith two lines");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "plain analysis text\nwith two lines\n"
        );

        // Braces that do not parse as JSON stay verbatim.
        let mut out: Vec<u8> = Vec::new();
        print_maybe_json(&mut out, "{not json}");
        assert_eq!(String::from_utf8(out).unwrap(), "{not json}\n");
    }

    /// Silence the unused-variant lint for ErrorCode in test builds: kept as
    /// documentation of the wire error class the stub returns.
    #[test]
    fn unknown_tool_error_class() {
        let error = rmcp::model::ErrorData::invalid_params("tool not found", None);
        assert_eq!(error.code.0, ErrorCode::INVALID_PARAMS.0);
    }
}
