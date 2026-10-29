//! The rmcp MCP server handler — (`NewMCPServer`
//! options + `RegisterGuideResource`) and the plugin stdio binding hooks
//! (`attachPluginWorkspaceHooks`).
//!
//! Server surface:
//! - name `Threski Studio`, version from D14 (workspace version,
//!   commit const default "none" — 1.1.0-equivalent identity)
//! - `WithToolCapabilities(true)` → tools capability
//! - `WithResourceCapabilities(false, true)` → resources, subscribe=false,
//!   list_changed=true
//! - `WithLogging` only when mode ≠ stdio
//! - instructions recommending Streamable HTTP `POST /mcp` + `threski://guide`
//! - 1 resource `threski://guide` (listable, not subscribable)
//! - the 56-tool registry from [`crate::registry`]

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{
    ListResourcesResult, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
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
}

#[tool_handler(router = self.router)]
impl ServerHandler for ThreskiServer {
    fn get_info(&self) -> ServerConfig {
        // WithResourceCapabilities(false, true): subscribe=false,
        // list_changed=true (ResourcesCapability is non-exhaustive, so the
        // default instance is mutated in place).
        let mut resources = rmcp::model::ResourcesCapability::default();
        resources.subscribe = Some(false);
        resources.list_changed = Some(true);
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
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

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
        if request.uri != threski_httpserver::discovery::GUIDE_URI {
            return Err(rmcp::ErrorData::invalid_params(
                format!("unknown resource {}", request.uri),
                None,
            ));
        }
        let cfg = self.state.config();
        let guide =
            threski_httpserver::discovery::guide_markdown(&cfg, &crate::registry::discovery_tools()());
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(guide, request.uri).with_mime_type("text/markdown")
        ])
        .into())
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<RoleServer>) {
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
