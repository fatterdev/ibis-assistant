//! The rmcp MCP server handler — (`NewMCPServer`
//! options + `RegisterGuideResource`) and the plugin stdio binding hooks
//! (`attachPluginWorkspaceHooks`).
//!
//! Server surface:
//! - name `Ibis Assistant`, version from D14 (workspace version,
//!   commit const default "none" — 1.1.0-equivalent identity)
//! - `WithToolCapabilities(true)` → tools capability
//! - `WithResourceCapabilities(true, true)` → resources with subscribe=true,
//!   list_changed=true
//! - `enable_prompts()` + `enable_completions()` → the F0.3 prompt surface
//!   (`crate::prompts`: readiness, status, why_changed, blast_radius,
//!   remember — with argument completions)
//! - `WithLogging` only when mode ≠ stdio
//! - instructions recommending Streamable HTTP `POST /mcp` + `ibis://guide`
//! - resources: `ibis://guide` (listable), the F0.7 MCP App
//!   `ui://ibis/memory-curator` (SEP-1865 `text/html;profile=mcp-app`)
//!   and the F0.4 templates
//!   `ibis://memory/{kind}/{id}`, `ibis://rules/{name}`,
//!   `ibis://ticket/{id}` (`crate::resources` — read dispatch + the
//!   live-update registry feeding `notifications/resources/updated`; both
//!   the legacy `resources/subscribe` peers and the 2026-07-28
//!   `subscriptions/listen` sinks are served)
//! - the 58-tool registry from [`crate::registry`]

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CompleteRequestParams,
    CompleteResult, ContentBlock, GetPromptRequestParams, GetPromptResponse, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    PromptMessage, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResultType, Role, ServerCapabilities,
    ServerConfig, SubscribeRequestParams, SubscriptionFilter, Tool, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::tool_handler;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use tracing::Instrument as _;

use crate::state::AppState;

/// Link-time identity (D14: `-ldflags -X main.version/-main.commit).
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"));
/// Release injection (D14, -ldflags -X main.commit equivalent): builds set
/// `IBIS_COMMIT` via the cargo env (e.g. `IBIS_COMMIT=$(git rev-parse --short
/// HEAD) cargo build --release`); dev builds stay "none".
pub const COMMIT: &str = match option_env!("IBIS_COMMIT") {
    Some(commit) => commit,
    None => "none",
};

/// — the MCP instructions string.
const INSTRUCTIONS: &str =
    "Ibis Assistant MCP. Prefer Streamable HTTP POST /mcp. Read resource ibis://guide for install and usage.";

/// The rmcp service: tools router + shared state.
pub struct IbisServer {
    /// Built by [`crate::registry::build_tool_router`] (D3).
    pub router: ToolRouter<IbisServer>,
    pub state: Arc<AppState>,
    /// `WithLogging` only when mode ≠ stdio
    pub with_logging: bool,
    /// Plugin stdio binding: fires once on the session `initialized`
    /// notification (`AddOnRegisterSession hook).
    plugin_binding_done: std::sync::atomic::AtomicBool,
    /// G20: last resolved harness profile. `get_tool` has no request context,
    /// so it uses the profile resolved by the most recent `list_tools`/
    /// `call_tool` (the same last-caller-wins limitation as sampling).
    last_profile: std::sync::Mutex<crate::harness::ResolvedHarnessProfile>,
    /// G20: connections already logged at info (first sighting only).
    seen_connections: std::sync::Mutex<std::collections::HashSet<u64>>,
}

/// F0.7 — the MCP App "Ibis Assistant Memory Curator": the `ui://` UI
/// resource declared with the SEP-1865 MIME type and the loopback-only
/// `_meta.ui.csp` policy (the document itself embeds the same CSP).
pub fn curator_resource() -> Resource {
    Resource::new(
        ibis_httpserver::curator::CURATOR_URI,
        ibis_httpserver::curator::CURATOR_NAME,
    )
    .with_title(ibis_httpserver::curator::CURATOR_NAME)
    .with_description(ibis_httpserver::curator::CURATOR_DESCRIPTION)
    .with_mime_type(ibis_httpserver::curator::CURATOR_MIME)
    .with_meta(ibis_httpserver::curator::ui_resource_meta())
}

pub fn tasks_resource() -> Resource {
    Resource::new(crate::resources::TASKS_URI, "Active Tasks")
        .with_title("Active Tasks")
        .with_description(
            "Durable task envelopes currently queued or running (E16): lease, checkpoint, \
             claims and recoveries.",
        )
        .with_mime_type("application/json")
}

impl IbisServer {
    pub fn new(state: Arc<AppState>, with_logging: bool) -> Self {
        let last_profile = crate::harness::resolve(&state.config().harness, None);
        IbisServer {
            router: crate::registry::build_tool_router(),
            state,
            with_logging,
            plugin_binding_done: std::sync::atomic::AtomicBool::new(false),
            last_profile: std::sync::Mutex::new(last_profile),
            seen_connections: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// G20: resolves the harness profile for this request and stores it for
    /// the context-less `get_tool`.
    fn resolve_profile(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> crate::harness::ResolvedHarnessProfile {
        self.resolve_profile_for_peer(&context.peer)
    }

    fn resolve_profile_for_peer(
        &self,
        peer: &rmcp::service::Peer<RoleServer>,
    ) -> crate::harness::ResolvedHarnessProfile {
        let client_name = peer.peer_info().map(|info| info.client_info.name.clone());
        let harness = self.state.cfg.read().unwrap().harness.clone();
        let profile = crate::harness::resolve(&harness, client_name.as_deref());
        let connection = crate::resources::peer_connection_id(peer);
        if self.seen_connections.lock().unwrap().insert(connection) {
            tracing::info!(
                "Harness profile '{}' for connection {connection}",
                profile.name
            );
        } else {
            tracing::debug!(
                "Harness profile '{}' for connection {connection}",
                profile.name
            );
        }
        *self.last_profile.lock().unwrap() = profile.clone();
        profile
    }

    fn guide_resource() -> Resource {
        Resource::new(
            ibis_httpserver::discovery::GUIDE_URI,
            "Ibis Assistant Guide",
        )
        .with_title("Ibis Assistant Guide")
        .with_description("Full install and usage guide, including the complete MCP tool list")
        .with_mime_type("text/markdown")
    }

    /// F0.6 dynamic tool discovery: `false` for the config-gated ticketing
    /// group when no provider is registered (unconfigured installs announce
    /// the core tool set only). G20: a harness profile can additionally hide
    /// its `hidden_tool_groups`.
    fn tool_visible(&self, name: &str, profile: &crate::harness::ResolvedHarnessProfile) -> bool {
        let config_visible =
            !crate::registry::is_ticketing_tool(name) || self.state.ticketing_configured();
        config_visible && !profile.hides(name)
    }
}

/// Message text of a panic payload (`panic!("…")` / `unwrap` style).
/// Shared by the server panic hook (`main.rs`) and tool-dispatch recovery.
pub fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|m| (*m).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// Runs a tool dispatch future, converting a panic into an `isError` tool
/// result so the caller gets a fast error instead of a hung MCP call —
/// regression: the fetch-limit clamp panic killed the handler task and the
/// IDE waited out its 60s timeout. The process-wide panic hook still logs
/// payload + backtrace.
async fn catch_tool_panic(
    tool_name: &str,
    dispatch: impl std::future::Future<Output = Result<CallToolResponse, rmcp::ErrorData>>,
) -> (Result<CallToolResponse, rmcp::ErrorData>, bool) {
    use futures::FutureExt as _;
    match std::panic::AssertUnwindSafe(dispatch).catch_unwind().await {
        Ok(response) => (response, false),
        Err(payload) => {
            let message = panic_payload_message(&*payload);
            tracing::error!(
                tool = tool_name,
                "tool panicked; returning an error result: {message}"
            );
            (
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "internal error: tool '{tool_name}' panicked: {message}\nsee the server log for details"
                ))])
                .into()),
                true,
            )
        }
    }
}

#[tool_handler(router = self.router)]
impl ServerHandler for IbisServer {
    fn get_info(&self) -> ServerConfig {
        // WithResourceCapabilities(true, true): subscribe=true (F0.4 live
        // updates), list_changed=true (ResourcesCapability is
        // non-exhaustive, so the default instance is mutated in place).
        let mut resources = rmcp::model::ResourcesCapability::default();
        resources.subscribe = Some(true);
        resources.list_changed = Some(true);
        let mut capabilities = ServerCapabilities::builder()
            // F0.6: tools may appear/disappear with the ticketing config —
            // `listChanged` advertised so clients expect the notification.
            .enable_tools()
            .enable_tool_list_changed()
            .enable_prompts()
            .enable_completions()
            .enable_resources_with(resources)
            .build();
        if self.with_logging {
            capabilities.logging = Some(Default::default());
        }
        ServerConfig::new(capabilities)
            .with_server_info(rmcp::model::Implementation::new(
                ibis_httpserver::discovery::SERVER_NAME,
                VERSION,
            ))
            .with_instructions(INSTRUCTIONS)
    }

    /// F0.6 dynamic tool discovery: the macro's list shape (cache hints on
    /// 2026-07-28 peers) plus the live config-gated filter — the
    /// `ticket_*` group is announced only when a ticketing provider is
    /// registered.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        let profile = self.resolve_profile(&context);
        let mut tools = self.router.list_all();
        tools.retain(|tool| self.tool_visible(&tool.name, &profile));
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    /// F0.6: hidden tools answer `tools/get`-style lookups (the Streamable
    /// HTTP `Mcp-Param-*` validation reads this) as absent.
    fn get_tool(&self, name: &str) -> Option<Tool> {
        let profile = self.last_profile.lock().unwrap().clone();
        self.router
            .get(name)
            .filter(|tool| self.tool_visible(&tool.name, &profile))
            .cloned()
    }

    /// F0.6: a config-hidden tool call gets a configuration error naming the
    /// fix, instead of the router's bare "tool not found" (the caller may be
    /// working from a stale tool list). A panicking handler is caught and
    /// returned as an `isError` result — never a hung call.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let profile = self.resolve_profile(&context);
        if !self.tool_visible(&request.name, &profile) {
            // A hidden tool call is diagnostic gold (stale client list, a
            // caller probing for unconfigured lanes): log it, don't fail
            // silently like the pre-warm 1.0.2 null call did.
            tracing::warn!(
                "MCP Tool Rejected: {} (hidden by config — dynamic tool discovery)",
                request.name
            );
            let message = if profile.hides(&request.name) {
                format!(
                    "tool '{}' is hidden by the '{}' harness profile \
                     (harness.profiles[].hidden_tool_groups)",
                    request.name, profile.name
                )
            } else {
                format!(
                    "tool '{}' is not available: ticketing is not configured \
                     (add a provider to config/ticketing_config.json)",
                    request.name
                )
            };
            return Err(rmcp::ErrorData::invalid_params(message, None));
        }
        // G20: update the sampling policy only for an accepted call; a
        // rejected hidden-tool call must not flip the shared leg.
        self.state
            .sampling
            .set_sampling_enabled(profile.sampling == crate::harness::SamplingPolicy::Auto);
        let tool_name = request.name.clone();
        // F17: the only argument that reaches telemetry is the project name.
        let project = crate::telemetry::project_from_arguments(request.arguments.as_ref());
        // Unknown names are a client-side string: keep the exported value
        // bounded (`<unknown>`) instead of an arbitrary cardinality/PII leak.
        let span_tool_name = crate::telemetry::tool_span_name(&tool_name);
        let read_only = crate::telemetry::tool_read_only(&tool_name);
        let runtime_mode = self.state.config().runtime_mode.clone();
        let span = crate::telemetry::active_tool_span(
            span_tool_name,
            read_only,
            project.as_deref(),
            &runtime_mode,
        );
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let t0 = std::time::Instant::now();
        let (response, panicked) =
            catch_tool_panic(&tool_name, self.router.call(tcc).instrument(span.clone())).await;
        let elapsed = t0.elapsed();
        match &response {
            Ok(CallToolResponse::Complete(result)) => {
                if result.is_error == Some(true) {
                    tracing::warn!("MCP Tool Done: {tool_name} is_error in {elapsed:?}");
                } else {
                    tracing::info!("MCP Tool Done: {tool_name} ok in {elapsed:?}");
                }
            }
            Ok(_) => tracing::info!("MCP Tool Done: {tool_name} ok in {elapsed:?}"),
            Err(err) => {
                tracing::warn!(
                    "MCP Tool Done: {tool_name} ERROR in {elapsed:?}: {}",
                    err.message
                );
            }
        }
        let outcome = if panicked {
            crate::telemetry::ToolOutcome::Panic
        } else {
            match &response {
                Ok(CallToolResponse::Complete(result)) if result.is_error == Some(true) => {
                    crate::telemetry::ToolOutcome::Error
                }
                Ok(_) => crate::telemetry::ToolOutcome::Ok,
                Err(_) => crate::telemetry::ToolOutcome::Error,
            }
        };
        crate::telemetry::record_tool_outcome(&span, outcome);
        response
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![
                Self::guide_resource(),
                crate::mcp_server::curator_resource(),
                crate::mcp_server::tasks_resource(),
            ],
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, rmcp::ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(
            crate::resources::resource_templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        if request.uri == ibis_httpserver::discovery::GUIDE_URI {
            let cfg = self.state.config();
            let guide = ibis_httpserver::discovery::guide_markdown(
                &cfg,
                &crate::registry::discovery_tools()(),
            );
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(guide, request.uri).with_mime_type("text/markdown")
            ])
            .into());
        }
        // F0.7 MCP App read: the Memory Curator document goes out with the
        // SEP-1865 MIME type and the loopback CSP metadata (hosts without
        // MCP Apps support never fetch it — the memory tools keep their
        // always-present textual output).
        if request.uri == ibis_httpserver::curator::CURATOR_URI {
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                ibis_httpserver::curator::curator_html(),
                request.uri,
            )
            .with_mime_type(ibis_httpserver::curator::CURATOR_MIME)
            .with_meta(ibis_httpserver::curator::ui_resource_meta())])
            .into());
        }
        // F0.4 template reads (memory / rules / ticket); anything else is a
        // protocol-level bad request.
        let parsed = crate::resources::parse_uri(&request.uri).ok_or_else(|| {
            rmcp::ErrorData::invalid_params(format!("unknown resource {}", request.uri), None)
        })?;
        let actor = crate::tools::actor_context(&context);
        let ticket_auth = crate::tools::ticket_auth(&context);
        let body =
            crate::resources::read_parsed(&self.state, &actor, &ticket_auth, &parsed).await?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(body.content, request.uri).with_mime_type(body.mime_type)
        ])
        .into())
    }

    /// Legacy `resources/subscribe` (protocol ≤ 2025-11-25): the
    /// connection's peer is registered per URI and receives
    /// `notifications/resources/updated` from the F0.4 registry.
    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        if crate::resources::parse_uri(&request.uri).is_none()
            && request.uri != ibis_httpserver::discovery::GUIDE_URI
            && request.uri != ibis_httpserver::curator::CURATOR_URI
        {
            return Err(rmcp::ErrorData::invalid_params(
                format!("unknown resource {}", request.uri),
                None,
            ));
        }
        let conn = crate::resources::peer_connection_id(&context.peer);
        let notify = crate::resources::peer_notify(context.peer.clone());
        self.state
            .resource_subs
            .subscribe(&request.uri, conn, notify);
        Ok(())
    }

    /// Legacy `resources/unsubscribe` — removes this connection's
    /// registration for the URI.
    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), rmcp::ErrorData> {
        let conn = crate::resources::peer_connection_id(&context.peer);
        self.state.resource_subs.unsubscribe(&request.uri, conn);
        Ok(())
    }

    /// 2026-07-28 subscriptions: accept the requested filter (the SDK
    /// intersects it with the advertised capabilities) and hold the
    /// subscription's sink in the F0.4 registry until the listen request is
    /// cancelled.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), rmcp::ErrorData> {
        let accepted = context.accepted().clone();
        let conn = crate::resources::peer_connection_id(&context.request_context().peer);
        let mut tokens = Vec::new();
        if let Some(uris) = accepted.resource_subscriptions {
            for uri in uris {
                let notify = crate::resources::sink_notify(context.sink().clone());
                tokens.push(self.state.resource_subs.subscribe(&uri, conn, notify));
            }
        }
        context.cancelled().await;
        for token in tokens {
            self.state.resource_subs.unsubscribe_token(token);
        }
        Ok(())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, rmcp::ErrorData> {
        Ok(ListPromptsResult::with_all_items(
            crate::prompts::prompt_definitions(),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, rmcp::ErrorData> {
        let spec = crate::prompts::spec_of(&request.name).ok_or_else(|| {
            rmcp::ErrorData::invalid_params(format!("unknown prompt {}", request.name), None)
        })?;
        let values = crate::prompts::ArgValues::new(request.arguments.as_ref());
        let message = PromptMessage::new_text(Role::User, (spec.render)(&values));
        let result =
            rmcp::model::GetPromptResult::new(vec![message]).with_description(spec.description);
        Ok(result.into())
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, rmcp::ErrorData> {
        Ok(crate::prompts::complete_with_state(&request, &self.state))
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<RoleServer>) {
        // G20: resolve the profile at session start so `get_tool` (which has
        // no request context) already sees the per-host visibility.
        self.resolve_profile_for_peer(&context.peer);
        // F0.6: the session joins the `notifications/tools/list_changed`
        // broadcast set (fires when the config-gated ticketing visibility
        // flips).
        self.state.tool_discovery.register(&context.peer);
        // Plugin stdio binding — AddOnRegisterSession hook
        let cfg = self.state.config();
        if !cfg.is_plugin() || ibis_core::workspace::plugin_has_bound_workspace(&cfg) {
            return;
        }
        if !self
            .plugin_binding_done
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return;
        }
        // 3s ListRoots round-trip ( context.WithTimeout 3s).
        let peer = context.peer.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            // Roots is deprecated by SEP-2577 in rmcp 3.7 but remains the
            // exact wire mechanism the original plugin binding used (3s ListRoots).
            #[allow(deprecated)]
            let request = rmcp::model::ListRootsRequest::default();
            #[allow(deprecated)]
            let roots = match tokio::time::timeout(
                std::time::Duration::from_secs(3),
                peer.send_request(rmcp::model::ServerRequest::ListRootsRequest(request)),
            )
            .await
            {
                #[allow(deprecated)]
                Ok(Ok(rmcp::model::ClientResult::ListRootsResult(result))) => result.roots,
                Ok(Ok(other)) => {
                    tracing::debug!("plugin workspace bind: unexpected ListRoots result");
                    let _ = other;
                    return;
                }
                Ok(Err(e)) => {
                    tracing::debug!("plugin workspace bind: ListRoots failed: {e}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("plugin workspace bind: ListRoots timed out");
                    return;
                }
            };
            let mut path = String::new();
            #[allow(deprecated)]
            for root in &roots {
                path = ibis_core::workspace::file_uri_to_path(&root.uri);
                if !path.is_empty() {
                    break;
                }
            }
            if path.is_empty() {
                return;
            }
            // Swap the config snapshot: Workspace + projects[0] +
            // DiscoveryRoot/GitRepos ( mutated cfg in place; N-A01 fix).
            let mut next = (*state.config()).clone();
            next.workspace = path.clone();
            let _ = ibis_core::workspace::ensure_opened_workspace(&mut next);
            let bound = ibis_core::workspace::plugin_has_bound_workspace(&next);
            if bound && !next.projects.is_empty() && !next.projects[0].working_repo_path.is_empty()
            {
                next.discovery_root = next.projects[0].working_repo_path.clone();
                next.git_repos
                    .push(next.projects[0].working_repo_path.clone());
            }
            *state.cfg.write().unwrap() = next;
            if bound && state.db().is_none() {
                // Lazily start the embedded DB ( startPluginEmbeddedDB).
                let cfg = state.config();
                match crate::assembly::start_plugin_embedded_db(&cfg).await {
                    Ok((db, process)) => {
                        if let Some(process) = process {
                            *state.db_process.lock().unwrap() = Some(process);
                        }
                        if let Some(db) = db {
                            state.bind_db(db);
                            if let Some(tasks) = state.tasks() {
                                tasks.set_events(Some(Arc::new(
                                    crate::tasks::ResourceTaskEvents::new(
                                        std::sync::Arc::downgrade(&state),
                                    ),
                                )));
                                tasks.recover_and_prune().await;
                            }
                            tracing::info!("Plugin workspace bound; embedded DB connected.");
                        }
                    }
                    Err(e) => tracing::error!("CRITICAL: {e}"),
                }
            }
        });
    }
}

#[cfg(test)]
mod panic_guard_tests {
    use super::*;

    async fn panicking_probe() -> Result<CallToolResponse, rmcp::ErrorData> {
        panic!("probe boom");
    }

    /// A handler panic must surface as an `isError` tool result, not as a
    /// hung call (regression: the fetch-limit clamp panic killed the handler
    /// task and the IDE waited out its 60s timeout).
    #[tokio::test]
    async fn tool_panic_becomes_error_result() {
        let (response, panicked) = catch_tool_panic("probe_tool", panicking_probe()).await;
        assert!(panicked, "panic flag must be set");
        let response = response.expect("panic must be converted, not propagated");
        let CallToolResponse::Complete(result) = response else {
            panic!("expected a complete tool response");
        };
        assert_eq!(result.is_error, Some(true));
        match result.content.first().expect("text block always present") {
            ContentBlock::Text(text) => {
                assert!(
                    text.text
                        .contains("internal error: tool 'probe_tool' panicked: probe boom"),
                    "unexpected error text: {}",
                    text.text
                );
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// The guard must not alter the normal outcome path.
    #[tokio::test]
    async fn ok_results_pass_through() {
        let (response, panicked) = catch_tool_panic("probe_tool", async {
            Ok::<CallToolResponse, rmcp::ErrorData>(
                CallToolResult::success(vec![ContentBlock::text("fine")]).into(),
            )
        })
        .await;
        assert!(!panicked, "normal dispatch is not a panic");
        let response = response.expect("ok dispatch");
        let CallToolResponse::Complete(result) = response else {
            panic!("expected a complete tool response");
        };
        assert_eq!(result.is_error, Some(false));
    }

    /// `panic_payload_message` decodes `&str` and `String` payloads.
    #[test]
    fn payload_message_decodes_str_and_string() {
        let from_str = std::panic::catch_unwind(|| panic!("str payload")).unwrap_err();
        assert_eq!(panic_payload_message(&*from_str), "str payload");
        let from_string =
            std::panic::catch_unwind(|| panic!("{}", String::from("string payload"))).unwrap_err();
        assert_eq!(panic_payload_message(&*from_string), "string payload");
    }
}
