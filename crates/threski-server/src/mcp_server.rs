//! The rmcp MCP server handler — (`NewMCPServer`
//! options + `RegisterGuideResource`) and the plugin stdio binding hooks
//! (`attachPluginWorkspaceHooks`).
//!
//! Server surface:
//! - name `Threski Studio`, version from D14 (workspace version,
//!   commit const default "none" — 1.1.0-equivalent identity)
//! - `WithToolCapabilities(true)` → tools capability
//! - `WithResourceCapabilities(true, true)` → resources with subscribe=true,
//!   list_changed=true
//! - `enable_prompts()` + `enable_completions()` → the F0.3 prompt surface
//!   (`crate::prompts`: readiness, status, why_changed, blast_radius,
//!   remember — with argument completions)
//! - `WithLogging` only when mode ≠ stdio
//! - instructions recommending Streamable HTTP `POST /mcp` + `threski://guide`
//! - resources: `threski://guide` (listable) plus the F0.4 templates
//!   `threski://memory/{kind}/{id}`, `threski://rules/{name}`,
//!   `threski://ticket/{id}` (`crate::resources` — read dispatch + the
//!   live-update registry feeding `notifications/resources/updated`; both
//!   the legacy `resources/subscribe` peers and the 2026-07-28
//!   `subscriptions/listen` sinks are served)
//! - the 56-tool registry from [`crate::registry`]

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CompleteRequestParams, CompleteResult,
    GetPromptRequestParams, GetPromptResponse, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, PromptMessage, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResultType, Role, ServerCapabilities, ServerConfig, SubscribeRequestParams,
    SubscriptionFilter, Tool, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::tool_handler;
use rmcp::RoleServer;
use rmcp::ServerHandler;

use crate::state::AppState;

/// Link-time identity (D14: `-ldflags -X main.version/-main.commit).
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"));
/// Release injection (D14, -ldflags -X main.commit equivalent): builds set
/// `THRESKI_COMMIT` via the cargo env (e.g. `THRESKI_COMMIT=$(git rev-parse --short
/// HEAD) cargo build --release`); dev builds stay "none".
pub const COMMIT: &str = match option_env!("THRESKI_COMMIT") {
    Some(commit) => commit,
    None => "none",
};

/// — the MCP instructions string.
const INSTRUCTIONS: &str =
    "Threski Studio MCP. Prefer Streamable HTTP POST /mcp. Read resource threski://guide for install and usage.";

/// The rmcp service: tools router + shared state.
pub struct ThreskiServer {
    /// Built by [`crate::registry::build_tool_router`] (D3).
    pub router: ToolRouter<ThreskiServer>,
    pub state: Arc<AppState>,
    /// `WithLogging` only when mode ≠ stdio
    pub with_logging: bool,
    /// Plugin stdio binding: fires once on the session `initialized`
    /// notification (`AddOnRegisterSession hook).
    plugin_binding_done: std::sync::atomic::AtomicBool,
}

impl ThreskiServer {
    pub fn new(state: Arc<AppState>, with_logging: bool) -> Self {
        ThreskiServer {
            router: crate::registry::build_tool_router(),
            state,
            with_logging,
            plugin_binding_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn guide_resource() -> Resource {
        Resource::new(
            threski_httpserver::discovery::GUIDE_URI,
            "Threski Studio Guide",
        )
        .with_title("Threski Studio Guide")
        .with_description("Full install and usage guide, including the complete MCP tool list")
        .with_mime_type("text/markdown")
    }

    /// F0.6 dynamic tool discovery: `false` for the config-gated ticketing
    /// group when no provider is registered (unconfigured installs announce
    /// the core tool set only).
    fn tool_visible(&self, name: &str) -> bool {
        !crate::registry::is_ticketing_tool(name) || self.state.ticketing_configured()
    }
}

#[tool_handler(router = self.router)]
impl ServerHandler for ThreskiServer {
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
                threski_httpserver::discovery::SERVER_NAME,
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
        let mut tools = self.router.list_all();
        if !self.state.ticketing_configured() {
            tools.retain(|tool| !crate::registry::is_ticketing_tool(&tool.name));
        }
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
        self.router
            .get(name)
            .filter(|tool| self.tool_visible(&tool.name))
            .cloned()
    }

    /// F0.6: a config-hidden tool call gets a configuration error naming the
    /// fix, instead of the router's bare "tool not found" (the caller may be
    /// working from a stale tool list).
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        if !self.tool_visible(&request.name) {
            return Err(rmcp::ErrorData::invalid_params(
                format!(
                    "tool '{}' is not available: ticketing is not configured \
                     (add a provider to config/ticketing_config.json)",
                    request.name
                ),
                None,
            ));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.router.call(tcc).await
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![Self::guide_resource()],
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
        if request.uri == threski_httpserver::discovery::GUIDE_URI {
            let cfg = self.state.config();
            let guide = threski_httpserver::discovery::guide_markdown(
                &cfg,
                &crate::registry::discovery_tools()(),
            );
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(guide, request.uri).with_mime_type("text/markdown")
            ])
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
            && request.uri != threski_httpserver::discovery::GUIDE_URI
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
        // F0.6: the session joins the `notifications/tools/list_changed`
        // broadcast set (fires when the config-gated ticketing visibility
        // flips).
        self.state.tool_discovery.register(&context.peer);
        // Plugin stdio binding — AddOnRegisterSession hook
        let cfg = self.state.config();
        if !cfg.is_plugin() || threski_core::workspace::plugin_has_bound_workspace(&cfg) {
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
                path = threski_core::workspace::file_uri_to_path(&root.uri);
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
            let _ = threski_core::workspace::ensure_opened_workspace(&mut next);
            let bound = threski_core::workspace::plugin_has_bound_workspace(&next);
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
                            tracing::info!("Plugin workspace bound; embedded DB connected.");
                        }
                    }
                    Err(e) => tracing::error!("CRITICAL: {e}"),
                }
            }
        });
    }
}
