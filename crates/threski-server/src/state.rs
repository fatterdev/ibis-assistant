//! Shared server state and service assembly — the server prologue
//! (config → logger →
//! discovery → embedded DB → AI providers → ticketing/repopr/search wiring
//! → background workers).
//!
//! D8 (degraded mode): the SurrealDB executor is a first-class
//! `Option<Arc<dyn Executor>>`; a failed connect leaves the server running
//! clone-only. The plugin stdio binding swaps the DB (and rebuilds the
//! search service) lazily on workspace bind — mutated a shared `*Config`
//! and a captured-by-value interface from a goroutine without a lock
//! (N-A01/N-A11); the Rust state machine swaps `Arc` snapshots
//! behind locks instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use threski_ai::{
    AIProvider, ClaudeProvider, Client, CloudPool, CursorLMProvider, FallbackChain, GeminiProvider,
    McpSamplingProvider, OllamaChatProvider, OllamaProvider, OpenAICompatProvider, ReasoningRouter,
    RouterConfig, SamplingTransport,
};
use threski_core::config::Config;
use threski_db::executor::{Executor, Vars};
use threski_db::process::ProcessManager;
use threski_db::value::SurrealValue;
use threski_ingest_dynamic::queue::ProjectIngestionManager;
use threski_repopr::service::Service as RepoprService;
use threski_search::search::SearchService;
use threski_ticketing::service::Service as TicketingService;

/// Bridges the orchestrating [`Client`] into the [`AIProvider`] surface the
/// search service and optimizer consume ( passed `*ai.Client` where an
/// `AIProvider` was expected via method-set compatibility; Rust needs an
/// explicit adapter).
pub struct ClientAIAdapter(pub Arc<Client>);

#[async_trait::async_trait]
impl AIProvider for ClientAIAdapter {
    fn name(&self) -> &str {
        "threski-ai-client"
    }

    async fn is_functional(&self) -> bool {
        self.0.is_functional()
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, threski_ai::AiError> {
        self.0.embed_text(text).await
    }

    async fn generate_content(
        &self,
        contents: &[threski_ai::Content],
        config: threski_ai::GenerationConfig,
    ) -> Result<threski_ai::Candidate, threski_ai::AiError> {
        self.0.generate_content(contents, config).await
    }

    async fn stop(&self) {
        self.0.stop().await;
    }
}

/// One `init_project`-style session record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoSession {
    pub project_name: String,
    pub origin_url: String,
    pub branch: String,
    pub commit: String,
}

/// The services shared by every tool handler.
pub struct AppState {
    /// Live config — the single canonical store, shared with the settings
    /// API router (the original mutated one `*cfg`; this crate keeps one
    /// `Arc<RwLock<Config>>` so the plugin binding and `settings_apply`
    /// swaps are visible everywhere without the original race, N-A01).
    pub cfg: Arc<RwLock<Config>>,
    /// D8 degraded-mode state machine: `None` = clone-only. Shared cell so
    /// the ingestion closure sees the plugin late binding too.
    pub db: Arc<RwLock<Option<Arc<dyn Executor>>>>,
    /// Search service; rebuilt when the DB binds (plugin late binding).
    pub search: Arc<RwLock<Arc<SearchService>>>,
    pub ai: Arc<Client>,
    pub ticket: Arc<TicketingService>,
    pub repopr: Arc<RepoprService>,
    /// init_project / update_project_status / repo_pr session map.
    pub sessions: Mutex<HashMap<String, RepoSession>>,
    /// Fire-and-forget ingestion queue (`update_project_status`).
    pub ingestion: Arc<ProjectIngestionManager>,
    /// Discovered + configured repository roots (background indexing targets).
    pub active_repos: Vec<String>,
    /// The embedded SurrealDB process handle (stopped on shutdown).
    pub db_process: Mutex<Option<ProcessManager>>,
    /// F0.4 live-update registry: `resources/subscribe` /
    /// `subscriptions/listen` subscribers notified with
    /// `notifications/resources/updated` on memory mutations.
    pub resource_subs: crate::resources::ResourceSubscriptions,
    /// F0.5 MCP sampling: the session-peer registry feeding the
    /// `mcp_sampling` reasoning provider (tool dispatch attaches the peer).
    pub sampling: Arc<crate::sampling::SamplingSessionRegistry>,
    /// F0.6 dynamic tool discovery: the session-peer broadcast set for
    /// `notifications/tools/list_changed` (initialized sessions) plus the
    /// ticketing visibility the tool list last carried.
    pub tool_discovery: crate::tool_discovery::ToolDiscovery,
}

impl AppState {
    /// Current config snapshot (cheap deep clone; the settings router holds
    /// the same lock).
    pub fn config(&self) -> Arc<Config> {
        Arc::new(self.cfg.read().unwrap().clone())
    }

    /// F0.6 dynamic tool discovery gate: `true` when at least one ticketing
    /// provider is registered — the condition under which the `ticket_*`
    /// group is announced on `tools/list`.
    pub fn ticketing_configured(&self) -> bool {
        !self.ticket.capabilities().providers.is_empty()
    }

    /// F0.6: re-checks the config-gated tool visibility and, when it flipped,
    /// announces `notifications/tools/list_changed` to every initialized
    /// session. Called after config swaps (the `settings_apply` tool and the
    /// settings HTTP API; both hot-reload the ticketing service first).
    pub async fn refresh_tool_visibility(&self) {
        self.tool_discovery
            .publish_ticketing_visibility(self.ticketing_configured())
            .await;
    }

    /// Current DB snapshot (D8).
    pub fn db(&self) -> Option<Arc<dyn Executor>> {
        self.db.read().unwrap().clone()
    }

    /// Current search service snapshot.
    pub fn search(&self) -> Arc<SearchService> {
        self.search.read().unwrap().clone()
    }

    /// Swaps the DB and rebuilds the search service over it (plugin late
    /// binding). Returns whether a DB was already bound.
    pub fn bind_db(&self, db: Arc<dyn Executor>) -> bool {
        let replaced = {
            let mut guard = self.db.write().unwrap();
            let replaced = guard.is_some();
            *guard = Some(db);
            replaced
        };
        self.rebuild_search();
        replaced
    }

    /// Rebuilds the search service against the current DB/AI snapshot,
    /// installing the git-bound claim plumbing and the runtime policy in one
    /// pass ( ; the plugin late binding rebuild lands here too
    /// — N-A11).
    pub fn rebuild_search(&self) {
        let db = self.db();
        let ai: Option<Arc<dyn AIProvider>> = Some(Arc::new(ClientAIAdapter(self.ai.clone())));
        let mut service = SearchService::new(db, ai);
        let runner = threski_git::GitRunner::anonymous();
        service.git_anchors = Some(Arc::new(threski_git::GitAnchorRunner::new(runner.clone())));
        service.claim_writer = Some(Arc::new(threski_git::GitCommitter::new(runner.clone())));
        service.notes = Some(Arc::new(threski_git::GitNotesRunner::new(runner)));
        let cfg = self.config();
        service.personal_claims_policy =
            threski_search::memory::claim_store::resolve_personal_claims_policy(
                &cfg.memory.git_store.personal_claims,
                &cfg.runtime_mode,
            );
        service.hot_ref_cache_disabled = cfg.world_state.code_cache_disabled;
        *self.search.write().unwrap() = Arc::new(service);
    }

    /// Sets a project session (`setSession; empty names are ignored).
    pub fn set_session(&self, session: RepoSession) {
        if session.project_name.trim().is_empty() {
            return;
        }
        self.sessions
            .lock()
            .unwrap()
            .insert(session.project_name.clone(), session);
    }

    /// Reads a project session.
    pub fn get_session(&self, project_name: &str) -> Option<RepoSession> {
        self.sessions.lock().unwrap().get(project_name).cloned()
    }

    /// `resolveMemoryWriteProject : the explicit name
    /// wins; otherwise the single configured/discovered candidate resolves.
    pub fn resolve_memory_write_project(&self, explicit: &str) -> Result<String, String> {
        let name = explicit.trim();
        if !name.is_empty() {
            return Ok(name.to_string());
        }
        let cfg = self.config();
        let mut candidates: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut add = |name: &str| {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            let key = name.to_lowercase();
            if seen.insert(key) {
                candidates.push(name.to_string());
            }
        };
        for p in &cfg.projects {
            if !p.name.trim().is_empty() {
                add(&p.name);
            } else if !p.working_repo_path.trim().is_empty() {
                add(&basename_of(p.working_repo_path.trim()));
            }
        }
        for root in &self.active_repos {
            add(&basename_of(root.trim()));
        }
        for name in self.sessions.lock().unwrap().keys() {
            add(name);
        }
        if candidates.len() == 1 {
            return Ok(candidates.remove(0));
        }
        Err("project_name is required".to_string())
    }

    /// `resolveMemoryProjectionRoot
    pub fn resolve_memory_projection_root(&self, project_name: &str) -> String {
        let want = project_name.trim();
        let cfg = self.config();
        for p in &cfg.projects {
            let root = p.working_repo_path.trim();
            if root.is_empty() {
                continue;
            }
            let base = basename_of(root);
            if want.is_empty()
                || p.name.eq_ignore_ascii_case(want)
                || base.eq_ignore_ascii_case(want)
            {
                return root.to_string();
            }
        }
        for root in &self.active_repos {
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            let base = basename_of(root);
            if want.is_empty() || base.eq_ignore_ascii_case(want) {
                return root.to_string();
            }
        }
        for root in &cfg.git_repos {
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            let base = basename_of(root);
            if want.is_empty() || base.eq_ignore_ascii_case(want) {
                return root.to_string();
            }
        }
        String::new()
    }

    /// Optional `.threski/memory` projection after memory writes (
    /// `maybeProjectMemory`).
    pub async fn maybe_project_memory(&self, project_name: &str) {
        let cfg = self.config();
        if !cfg.memory.projection_enabled {
            return;
        }
        let root = self.resolve_memory_projection_root(project_name);
        if root.is_empty() {
            return;
        }
        let mut name = project_name.trim().to_string();
        if name.is_empty() {
            name = basename_of(&root);
        }
        let service = self.search();
        if let Err(e) =
            threski_search::memory::projection::write_memory_projection(&service, &root, &name)
                .await
        {
            tracing::warn!("memory projection failed: {e}");
        }
    }
}

/// Basename of a path as an owned String (module-level fn: inline closures
/// over `&OsStr` are inferred at a concrete lifetime inside Send futures —
/// rustc limitation).
pub(crate) fn basename_of(path: &str) -> String {
    match std::path::Path::new(path).file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => String::new(),
    }
}

/// The assembled AI stack ( result).
pub struct AiAssembly {
    pub client: Arc<Client>,
}

/// Builds the AI client from config (parity with the original provider switches).
/// `sampling` is the F0.5 MCP-sampling seam (`SamplingSessionRegistry`): the
/// `ide_sampling`/`mcp_sampling` reasoning provider sends
/// `sampling/createMessage` through it, with the lm-bridge and local Ollama
/// steps of the chain as fallbacks.
pub async fn assemble_ai(
    cfg: &Config,
    db: Option<Arc<dyn Executor>>,
    plugin_unbound: bool,
    sampling: Arc<dyn SamplingTransport>,
) -> AiAssembly {
    let probe = threski_settings::probe(&cfg.ai.embedding.url).await;
    let local_model = threski_settings::resolve_local_model(&cfg.ai.reasoning, &probe);
    tracing::info!(
        "AI probe: tier={} vram_mib={} cpu_only={} ollama={} recommended_local={}",
        probe.tier.as_str(),
        probe.vram_mib,
        probe.cpu_only,
        probe.ollama_reachable,
        local_model
    );

    // 6a. Embedding provider.
    let mut runner: Option<Arc<threski_ai::OllamaProcess>> = None;
    let usage_store: Option<Arc<dyn threski_ai::UsageStore>> = db
        .clone()
        .map(|exec| Arc::new(DbUsageStore(exec)) as Arc<dyn threski_ai::UsageStore>);
    let embedding: Option<Arc<dyn AIProvider>> = match cfg.ai.embedding.provider.as_str() {
        "ollama" => {
            if !plugin_unbound {
                match threski_ai::ensure_ollama(
                    &cfg.ai.embedding.url,
                    &cfg.ai.embedding.model,
                    cfg.ai.embedding.auto_start,
                    cfg.ai.embedding.auto_update,
                )
                .await
                {
                    Ok(outcome) => {
                        if let Some(process) = outcome.process {
                            runner = Some(Arc::new(process));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Ollama initialization failed/skipped: {e}. Embedding provider might be non-functional."
                        )
                    }
                }
            }
            Some(Arc::new(OllamaProvider::new(
                &cfg.ai.embedding.model,
                &cfg.ai.embedding.url,
            )) as Arc<dyn AIProvider>)
        }
        "gemini" => Some(Arc::new(GeminiProvider::new(
            effective_gemini_keys(cfg),
            usage_store.clone(),
        )) as Arc<dyn AIProvider>),
        other => {
            tracing::warn!("Unknown embedding provider: {other}. Defaulting to Ollama.");
            Some(Arc::new(OllamaProvider::new(
                &cfg.ai.embedding.model,
                &cfg.ai.embedding.url,
            )) as Arc<dyn AIProvider>)
        }
    };

    // 6b. Reasoning provider. F0.5: in plugin runtime mode the default
    // (`hybrid` = user-unset) becomes `ide_sampling` — the MCP client (the
    // IDE) is the primary reasoning via `sampling/createMessage`, with the
    // deprecated-but-working LM bridge and local Ollama as the chain's
    // fallbacks.
    let provider_raw = cfg.ai.reasoning.provider.trim().to_lowercase();
    let provider = if (provider_raw.is_empty() || provider_raw == "hybrid")
        && threski_core::config::is_plugin_runtime(&cfg.runtime_mode)
    {
        "ide_sampling".to_string()
    } else {
        provider_raw
    };
    let mut local_reasoning: Option<Box<dyn AIProvider>> = None;
    if matches!(
        provider.as_str(),
        "hybrid" | "ollama" | "" | "ide_sampling" | "mcp_sampling"
    ) {
        if cfg.ai.embedding.provider == "ollama"
            && !plugin_unbound
            && runner.is_none()
            && cfg.ai.embedding.auto_start
        {
            match threski_ai::ensure_ollama(
                &cfg.ai.embedding.url,
                &local_model,
                cfg.ai.embedding.auto_start,
                cfg.ai.embedding.auto_update,
            )
            .await
            {
                Ok(outcome) => {
                    if let Some(process) = outcome.process {
                        runner = Some(Arc::new(process));
                    }
                }
                Err(e) => tracing::warn!("Ollama chat model ensure failed: {e}"),
            }
        } else if cfg.ai.embedding.provider == "ollama"
            && cfg.ai.embedding.auto_update
            && !plugin_unbound
        {
            let _ =
                threski_ai::ensure_ollama(&cfg.ai.embedding.url, &local_model, false, true).await;
        }
        local_reasoning = Some(Box::new(OllamaChatProvider::new(
            &local_model,
            &cfg.ai.embedding.url,
        )) as Box<dyn AIProvider>);
    }

    // Cloud pool members (gemini keys, openai_compat, claude, cursor).
    let gemini_keys = effective_gemini_keys(cfg);
    let mut cloud_providers: HashMap<String, Arc<dyn AIProvider>> = HashMap::new();
    let mut direct_pick: HashMap<String, Arc<dyn AIProvider>> = HashMap::new();
    let mut remember = |name: &str, provider: Arc<dyn AIProvider>| {
        direct_pick.insert(name.to_string(), provider.clone());
        cloud_providers.insert(name.to_string(), provider);
    };
    if !gemini_keys.is_empty() {
        remember(
            "gemini",
            Arc::new(GeminiProvider::new(gemini_keys, usage_store)) as Arc<dyn AIProvider>,
        );
    }
    let oa = &cfg.ai.reasoning.clouds.openai_compat;
    if !oa.api_key.trim().is_empty() {
        remember(
            "openai_compat",
            Arc::new(OpenAICompatProvider::new(
                &oa.base_url,
                &oa.api_key,
                &oa.model,
            )) as Arc<dyn AIProvider>,
        );
    }
    let cl = &cfg.ai.reasoning.clouds.claude;
    if !cl.api_key.trim().is_empty() {
        remember(
            "claude",
            Arc::new(ClaudeProvider::new(&cl.api_key, &cl.model)) as Arc<dyn AIProvider>,
        );
    }
    let mut order = cfg.ai.reasoning.routing.cloud_fallback_order.clone();
    if order.is_empty() {
        order = vec!["gemini", "openai_compat", "claude"]
            .into_iter()
            .map(String::from)
            .collect();
    }
    let mut cloud_primary = cfg.ai.reasoning.resolve_cloud_primary();
    if cfg.ai.reasoning.use_ide_provided_models
        || cfg.ai.reasoning.provider.eq_ignore_ascii_case("cursor")
    {
        let cursor = CursorLMProvider::new("");
        if cursor.is_functional().await {
            remember("cursor", Arc::new(cursor) as Arc<dyn AIProvider>);
            if !order.iter().any(|n| n == "cursor") {
                order.insert(0, "cursor".to_string());
            }
            cloud_primary = "cursor".to_string();
        } else {
            tracing::info!(
                "IDE-provided models enabled but LM bridge not reachable (set THRESKI_LM_BRIDGE_URL / start ide-host RPC)"
            );
        }
    }
    let cloud_pool = CloudPool::new(&cloud_primary, &order, cloud_providers).await;

    // 6b switch: pick the reasoning provider.
    let reasoning: Option<Arc<dyn AIProvider>> = match provider.as_str() {
        "gemini" | "openai_compat" | "claude" | "cursor" => {
            let name = provider.clone();
            match direct_pick.get(&name) {
                Some(p) => Some(p.clone()),
                None => {
                    tracing::warn!("Reasoning provider {name} selected but not configured");
                    None
                }
            }
        }
        "ollama" => None, // replaced by the local chat provider below
        "none" => {
            tracing::info!("Reasoning provider=none (context-only mode)");
            None
        }
        "ide_sampling" | "mcp_sampling" => {
            // F0.5 chain: sampling → lm_bridge → ollama. The LM bridge is
            // deprecated but still working (`THRESKI_LM_BRIDGE_*` only as
            // fallback); Ollama is the final fallback. The bridge reports
            // non-functional without a reachable host probe, Ollama without
            // a running daemon — the chain skips them in order.
            let ollama_leg: Arc<dyn AIProvider> = match local_reasoning.take() {
                Some(local) => Arc::from(local),
                None => Arc::new(OllamaChatProvider::new(&local_model, &cfg.ai.embedding.url)),
            };
            let steps: Vec<Arc<dyn AIProvider>> = vec![
                Arc::new(McpSamplingProvider::new(sampling.clone())),
                Arc::new(CursorLMProvider::new("")),
                ollama_leg,
            ];
            let chain = FallbackChain::new(steps, cfg.ai.reasoning.context_only_fallback);
            tracing::info!("Reasoning chain: {}", chain.step_names().join(" → "));
            Some(Arc::new(chain) as Arc<dyn AIProvider>)
        }
        "hybrid" | "" => Some(Arc::new(ReasoningRouter::new(
            local_reasoning.take(),
            Some(Box::new(cloud_pool)),
            RouterConfig {
                use_cloud_when_no_gpu: cfg.ai.reasoning.routing.use_cloud_when_no_gpu,
                cpu_only: probe.cpu_only,
                local_timeout: std::time::Duration::from_millis(
                    cfg.ai.reasoning.routing.local_timeout_ms.max(0) as u64,
                ),
                context_only: cfg.ai.reasoning.context_only_fallback,
            },
        )) as Arc<dyn AIProvider>),
        other => {
            tracing::warn!("Unknown reasoning provider: {other}");
            None
        }
    };

    // The ollama case keeps the local chat provider (rebuilt here because the
    // hybrid branch moved the boxed one into the router).
    let reasoning = match provider.as_str() {
        "ollama" => Some(
            Arc::new(OllamaChatProvider::new(&local_model, &cfg.ai.embedding.url))
                as Arc<dyn AIProvider>,
        ),
        _ => reasoning,
    };

    let client = Arc::new(Client::new(embedding, reasoning, runner));

    if !client.is_functional() {
        tracing::warn!(
            "\u{26a0}\u{fe0f}  AI is limited (no local Ollama reasoning/embed and no cloud keys)."
        );
        tracing::warn!(
            "   - Run 'threski config' or open /settings/ to add keys and enable hybrid AI."
        );
        tracing::warn!("   - ENV keys (GEMINI_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY) are optional overrides only.");
    }

    AiAssembly { client }
}

/// `geminiKeysFromCfg / `GeminiKeysEffective` with the default-RPM fill.
fn effective_gemini_keys(cfg: &Config) -> Vec<threski_ai::KeyConfig> {
    let mut keys = Vec::new();
    for k in cfg.ai.reasoning.gemini_keys_effective() {
        let mut rpm = k.rpm;
        if rpm <= 0 {
            rpm = cfg.gemini_default_rpm;
        }
        keys.push(threski_ai::KeyConfig {
            key: k.key.clone(),
            rpm,
            tpm: k.tpm,
            rpd: k.rpd,
            owner: k.owner.clone(),
            allow_overage: k.allow_overage,
            ..Default::default()
        });
    }
    keys
}

/// Gemini per-key RPD usage persistence over the executor
/// (`worker.loadUsageFromDB + `updateDBUsage`; D17: one UPSERT replaces
/// the UPDATE-then-CREATE fallback pair).
struct DbUsageStore(Arc<dyn Executor>);

#[async_trait::async_trait]
impl threski_ai::UsageStore for DbUsageStore {
    async fn load_requests(&self, usage_id: &str) -> Result<f64, threski_ai::AiError> {
        if !threski_db::ids::is_safe_record_id(usage_id) {
            return Ok(0.0);
        }
        let sql = format!("SELECT requests FROM {usage_id};");
        let outcome = match self.0.execute(&sql).await {
            Ok(outcome) => outcome,
            Err(_) => return Ok(0.0),
        };
        #[derive(Debug, SurrealValue)]
        struct RequestsRow {
            requests: Option<threski_db::value::F64>,
        }
        let rows = threski_db::value::decode_row_list::<RequestsRow>(
            outcome.first().unwrap_or(&threski_db::value::Value::None),
            &sql,
        )
        .unwrap_or_default();
        Ok(rows
            .first()
            .and_then(|r| r.requests)
            .map(|v| v.get())
            .unwrap_or(0.0))
    }

    async fn save_requests(
        &self,
        usage_id: &str,
        requests: i64,
        owner: &str,
    ) -> Result<(), threski_ai::AiError> {
        if !threski_db::ids::is_safe_record_id(usage_id) {
            return Ok(());
        }
        let sql = format!(
            "UPSERT {usage_id} SET requests = $req, owner = $owner, last_updated = time::now();"
        );
        let mut vars = Vars::new();
        vars.insert("req".into(), threski_db::value::Value::from_t(requests));
        vars.insert("owner".into(), threski_db::value::Value::from_t(owner));
        self.0
            .smart_query(&sql, vars)
            .await
            .map_err(|e| threski_ai::AiError::other(e.to_string()))?;
        Ok(())
    }
}
