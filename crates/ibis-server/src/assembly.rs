//! Service assembly and background workers — the `runServer` body between
//! config load and the server run loop
//!
//! Port notes:
//! - The embedded-DB block and
//!   `startPluginEmbeddedDB` were duplicated originally;
//!   this crate factors them into [`ensure_database`] /
//!   [`start_plugin_embedded_db`] (finding N-A08).
//! - The batch manager is wired through the lane-C trait seams with a
//!   DB-backed [`DbChunkStore`] adapter (bound variables, D17 UPSERT).

use std::sync::Arc;

use ibis_ai::batch_manager::{BatchManager, DiscoveryRootProbe, GitPatchSource};
use ibis_core::config::Config;
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::process::ProcessManager;
use ibis_db::value::{RecordId, SurrealValue, Value};
// B6: sha256 record ids for the L2 embedding cache (Digest trait in scope).
use ibis_ingest_dynamic::queue::{IngestionJob, ProcessJobFn, ProjectIngestionManager};
use ibis_repopr::service::Service as RepoprService;
use ibis_search::search::SearchService;
use sha2::Digest;

use crate::state::AppState;

/// The embedded-DB bootstrap shared by startup and the plugin late binding
/// ( + startPluginEmbeddedDB).
pub async fn ensure_database(cfg: &Config) -> (Option<Arc<dyn Executor>>, Option<ProcessManager>) {
    if let Err(err) = ibis_ingest_code::install::ensure_ast_grep(cfg.db_auto_update).await {
        tracing::warn!("Could not ensure ast-grep binary: {err}");
    }

    // Extract port from DBUrl ( parsed the last colon segment).
    let db_port = cfg
        .db_url
        .rsplit(':')
        .next()
        .and_then(|p| p.split('/').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8000);

    tracing::debug!("Attempting to start embedded database on port {db_port}...");
    let mut process = None;
    match ProcessManager::start_embedded(
        &cfg.db_user,
        &cfg.db_password,
        &cfg.db_data_path,
        db_port,
        cfg.db_auto_update,
    )
    .await
    {
        Ok(proc) => {
            process = Some(proc);
            tracing::info!("Embedded SurrealDB started successfully.");
        }
        Err(err) => {
            tracing::info!(
                "Note: Could not start embedded database (or it is already running): {err}"
            );
        }
    }

    tracing::info!("Connecting to SurrealDB at {}...", cfg.db_url);
    let mut client = match SurrealDbExecutor::connect(
        &cfg.db_url,
        &cfg.db_namespace,
        &cfg.db_database,
        &cfg.db_user,
        &cfg.db_password,
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            tracing::error!("CRITICAL: Failed to connect to SurrealDB: {err}");
            return (None, process);
        }
    };
    if cfg.db_timeout > 0 {
        client.set_timeout(std::time::Duration::from_secs(cfg.db_timeout as u64));
    }
    tracing::info!("Successfully connected to SurrealDB.");
    let executor: Arc<dyn Executor> = Arc::new(client);
    if let Err(err) = ibis_db::schema::init_schema(executor.as_ref()).await {
        tracing::warn!("Database schema initialization warning: {err}");
    }
    if let Err(err) = ibis_db::migrate::migrate(executor.as_ref()).await {
        tracing::warn!("Database migration warning: {err}");
    }
    (Some(executor), process)
}

/// The plugin late-binding variant: boots the DB and returns
/// `(executor, process)`.
pub async fn start_plugin_embedded_db(
    cfg: &Config,
) -> Result<(Option<Arc<dyn Executor>>, Option<ProcessManager>), String> {
    let (db, process) = ensure_database(cfg).await;
    if db.is_none() {
        return Err("Failed to connect to SurrealDB".to_string());
    }
    Ok((db, process))
}

/// Builds the ticketing service + providers
pub async fn assemble_ticketing(
    db: Option<Arc<dyn Executor>>,
) -> (Arc<ibis_ticketing::service::Service>, String) {
    let (cfg, err) = match ibis_ticketing::load_config() {
        Ok(cfg) => (cfg, String::new()),
        Err(e) => {
            tracing::warn!(
                "Ticketing config not found or invalid ({e}). Falling back to defaults."
            );
            (ibis_ticketing::default_config(), e.to_string())
        }
    };
    let service = ibis_ticketing::service::Service::new(cfg.clone(), db.clone());
    for provider in ticketing_providers(&cfg) {
        service.register_provider(provider);
    }
    if db.is_some() {
        if let Err(err) = ibis_ticketing::migrate_legacy_issues(db.as_ref()).await {
            tracing::warn!("Legacy issue migration warning: {err}");
        }
    }
    (Arc::new(service), err)
}

/// F0.6: the provider set for a ticketing config (startup assembly and hot
/// reload build the same list: a provider registers only when its connection
/// block is non-empty).
fn ticketing_providers(
    cfg: &ibis_ticketing::Config,
) -> Vec<Arc<dyn ibis_ticketing::TicketProvider>> {
    let mut providers: Vec<Arc<dyn ibis_ticketing::TicketProvider>> = Vec::new();
    if !cfg.providers.redmine.base_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: redmine ({})",
            cfg.providers.redmine.base_url
        );
        providers.push(Arc::new(ibis_ticketing::RedmineProvider::new(
            &cfg.providers.redmine,
        )));
    }
    if !cfg.providers.jira.base_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: jira ({})",
            cfg.providers.jira.base_url
        );
        providers.push(Arc::new(ibis_ticketing::JiraProvider::new(
            &cfg.providers.jira,
        )));
    }
    if !cfg.providers.azure_devops.organization_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: azure_devops ({})",
            cfg.providers.azure_devops.organization_url
        );
        providers.push(Arc::new(ibis_ticketing::AzureDevOpsProvider::new(
            &cfg.providers.azure_devops,
        )));
    }
    providers
}

/// F0.6 hot reload: re-reads `config/ticketing_config.json`, swaps the
/// live ticketing service in place (the F0.4 ingestion wiring shares the
/// same `Arc`, so late reference resolution sees the new providers too) and
/// announces `notifications/tools/list_changed` when the visibility gate
/// flipped. Logs only when the configured provider set changes.
pub async fn reload_ticketing(state: &crate::state::AppState) {
    let cfg = match ibis_ticketing::load_config() {
        Ok(cfg) => cfg,
        // Missing/invalid file at reload time is the routine unconfigured
        // case (startup already warned once); degrade silently to defaults.
        Err(e) => {
            tracing::debug!("Ticketing config not found or invalid on reload ({e}); defaults.");
            ibis_ticketing::default_config()
        }
    };
    reload_ticketing_with_config(state, cfg).await;
}

/// [`reload_ticketing`] with an explicit config (tests drive flips without
/// touching the process CWD). Returns the visibility the gate carries now.
pub async fn reload_ticketing_with_config(
    state: &crate::state::AppState,
    cfg: ibis_ticketing::Config,
) -> bool {
    let before = state.ticket.capabilities().providers;
    state.ticket.reload(cfg.clone(), ticketing_providers(&cfg));
    let after = state.ticket.capabilities().providers;
    if after != before {
        if after.is_empty() {
            tracing::info!("Ticketing providers reloaded: none; ticket_* tools hidden.");
        } else {
            tracing::info!("Ticketing providers reloaded: {}", after.join(", "));
        }
    }
    state.refresh_tool_visibility().await;
    state.ticketing_configured()
}

/// Builds the repopr service : ADO registered only when
/// `providers.azure_devops.organization_url` is set.
pub fn assemble_repopr(ticket_cfg: &ibis_ticketing::Config) -> Arc<RepoprService> {
    let project_provider_map = ticket_cfg
        .project_provider_map
        .iter()
        .map(|(k, v)| (k.clone(), ibis_repopr::ProviderName(v.0.clone())))
        .collect();
    let project_target_branch: std::collections::HashMap<String, String> = ticket_cfg
        .pr
        .project_target_branch
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut service = ibis_repopr::service::Service::new(ibis_repopr::Config {
        default_provider: ibis_repopr::ProviderName(ticket_cfg.default_provider.0.clone()),
        project_provider_map,
        default_target_branch: ticket_cfg.pr.default_target_branch.clone(),
        project_target_branch,
    });
    if !ticket_cfg
        .providers
        .azure_devops
        .organization_url
        .is_empty()
    {
        service.register_provider(Arc::new(ibis_repopr::AzureDevOpsProvider::new(
            ibis_repopr::providers::azuredevops::Config {
                organization_url: ticket_cfg.providers.azure_devops.organization_url.clone(),
                project: ticket_cfg.providers.azure_devops.project.clone(),
                repository: ticket_cfg.providers.azure_devops.repository.clone(),
                pat: ticket_cfg.providers.azure_devops.pat.clone(),
            },
        )));
    }
    Arc::new(service)
}

/// Starts the background indexing manager wiring :
/// the queue's `ProcessJob` runs git ingest + code ingest + stale refresh.
///
/// Error-path note (finding N-A12): `DynamicError` has no generic variant,
/// so the closure logs failures itself (the queue logs only its own
/// `Err(...)` receipt) and completes the job — the wiring's `on_complete`
/// callback is never set (`update_project_status passed no callbacks
/// either), so observable behavior is unchanged.
#[allow(clippy::type_complexity)]
pub fn wire_ingestion_manager(
    manager: &mut ProjectIngestionManager,
    cfg: Arc<Config>,
    db_cell: Arc<std::sync::RwLock<Option<Arc<dyn Executor>>>>,
    ai: Arc<ibis_ai::Client>,
    ticket: Arc<ibis_ticketing::service::Service>,
    search_cell: Arc<std::sync::RwLock<Arc<SearchService>>>,
) {
    let concurrency = cfg.redmine_concurrency.max(1) as usize;
    let process_job: ProcessJobFn = Arc::new(move |job: IngestionJob, repo_path: String| {
        let db_cell = db_cell.clone();
        let ai = ai.clone();
        let ticket = ticket.clone();
        let search_cell = search_cell.clone();
        let cfg = cfg.clone();
        Box::pin(async move {
            let repo_name = job.project_name.clone();
            tracing::info!("Background: Indexing Git history for {repo_name}...");
            let patterns = ticket.reference_patterns_for_project(&repo_name);
            let mut pattern_refs: Vec<&str> = Vec::with_capacity(patterns.len());
            for p in &patterns {
                pattern_refs.push(p.as_str());
            }
            let Some(db) = db_cell.read().unwrap().clone() else {
                tracing::error!("Ingestion error for {repo_name}: database not connected");
                return Ok(());
            };
            if let Err(e) = ibis_ingest_git::ingest_repo(
                db.clone(),
                Some(ticket.clone()),
                &repo_path,
                &repo_name,
                concurrency,
                &pattern_refs,
            )
            .await
            {
                tracing::error!("Ingestion error for {repo_name}: git ingestion error: {e}");
                return Ok(());
            }
            tracing::info!("Background: Vectorizing codebase for {repo_name}...");
            if let Err(e) = ibis_ingest_code::ingest_codebase(
                db.clone(),
                Arc::new(ibis_ingest_code::AiClientAdapter(ai.clone())),
                &repo_path,
                &repo_name,
                Some(&cfg),
                None,
            )
            .await
            {
                tracing::error!("Ingestion error for {repo_name}: code ingestion error: {e}");
                return Ok(());
            }
            let search = search_cell.read().unwrap().clone();
            refresh_stale_after_ingest(&db, &search, &repo_path, &repo_name).await;
            Ok(())
        })
    });
    manager.set_process_job(process_job);
}

/// Post-ingest reconcile pass (`refreshStaleAfterIngest).
pub async fn refresh_stale_after_ingest(
    db: &Arc<dyn Executor>,
    service: &Arc<SearchService>,
    repo_path: &str,
    project_name: &str,
) {
    // History may have moved: cached ancestor sets must not leak stale DAGs.
    ibis_search::memory::ancestry::invalidate_ancestors(service, repo_path);
    match service.index_claim_store(repo_path, project_name).await {
        Ok(c) if c > 0 => {
            tracing::info!("IndexClaimStore: reconciled {c} claim rows");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("IndexClaimStore after ingest: {e}"),
    }
    match service.refresh_stale_memories_for_repo(repo_path).await {
        Ok(n) if n > 0 => {
            tracing::info!("RefreshStaleMemories: marked {n} memories stale");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("RefreshStaleMemories after ingest: {e}"),
    }
    match service.refresh_orphaned_git_bounds(repo_path).await {
        Ok(o) if o > 0 => {
            tracing::info!(
                "RefreshOrphanedGitBounds: marked {o} memories stale for orphaned commits"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("RefreshOrphanedGitBounds after ingest: {e}"),
    }
    match service.promote_dirty_anchors_for_repo(repo_path).await {
        Ok(p) if p > 0 => {
            tracing::info!("PromoteDirtyAnchors: bound {p} promoted claims to their commit");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("PromoteDirtyAnchors after ingest: {e}"),
    }
    match service.prune_file_at_commit_cache().await {
        Ok(pr) if pr > 0 => {
            tracing::info!("PruneFileAtCommitCache: dropped {pr} cached reads older than TTL");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("PruneFileAtCommitCache after ingest: {e}"),
    }
    let _ = db;
}

/// Starts the background SMTP notification aggregator
pub fn start_notification_aggregator(
    state: &Arc<AppState>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let cfg = state.config();
    if !cfg.smtp.enabled || state.db().is_none() {
        return;
    }
    let window = if cfg.smtp.aggregation_window.trim().is_empty() {
        "1h".to_string()
    } else {
        cfg.smtp.aggregation_window.clone()
    };
    let duration = ibis_ingest_logs::parse_go_duration(&window).unwrap_or_else(|| {
        tracing::warn!("Invalid aggregation window duration '{window}', defaulting to 1 hour");
        std::time::Duration::from_secs(3600)
    });
    tracing::info!("Starting notification aggregator worker (interval: {duration:?})...");
    let state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(duration);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("Aggregator: context cancelled, flushing final pending notifications...");
                    let cfg = state.config();
                    let notifier = ibis_ingest_logs::Notifier::new(cfg, state.db());
                    let flush = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        notifier.process_pending_notifications(),
                    );
                    match flush.await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::error!("Aggregator: final flush failed: {e}"),
                        Err(_) => tracing::error!("Aggregator: final flush timed out"),
                    }
                    return;
                }
                _ = ticker.tick() => {
                    tracing::debug!("Aggregator tick: processing pending notifications...");
                    let cfg = state.config();
                    let notifier = ibis_ingest_logs::Notifier::new(cfg, state.db());
                    if let Err(e) = notifier.process_pending_notifications().await {
                        tracing::error!("Aggregator: failed to process pending notifications: {e}");
                    }
                }
            }
        }
    });
}

/// Starts the remote log polling manager
pub fn start_polling_manager(state: &Arc<AppState>, shutdown: tokio_util::sync::CancellationToken) {
    if state.db().is_none() {
        return;
    }
    let cfg = state.config();
    let mut manager =
        ibis_ingest_logs::PollingManager::new(cfg, state.db(), Some(state.ai.clone()));
    manager.start();
    tokio::spawn(async move {
        shutdown.cancelled().await;
        manager.stop().await;
    });
}

/// Starts the batch embedding manager
///
/// F-009 note: the discovery-root probe is the wired resolver — the
/// repo-record linkage primary cannot resolve paths because the `repo`
/// table carries no path column (report finding N-A13; the probe keeps
/// parity for the owned-clone layouts it was written for).
pub fn start_batch_manager(state: &Arc<AppState>) -> Option<Arc<BatchManager>> {
    let db = state.db()?;
    let cfg = state.config();
    let store: Arc<dyn ibis_ai::ChunkStore> = Arc::new(DbChunkStore(db));
    let resolver = Arc::new(DiscoveryRootProbe::new(cfg.discovery_root.clone()));
    let ai: Option<Arc<dyn ibis_ai::AIProvider>> =
        Some(Arc::new(crate::state::ClientAIAdapter(state.ai.clone()))
            as Arc<dyn ibis_ai::AIProvider>);
    let manager = Arc::new(BatchManager::new(
        Some(store),
        ai,
        Arc::new(GitPatchSource),
        resolver,
        cfg.max_delta_size.max(0) as usize,
    ));
    manager.start();
    Some(manager)
}

/// B6: DB-backed L2 embedding cache over the executor (exact-key lookups).
/// Record ids are the SHA-256 of the cache key — deterministic, so the UPSERT
/// is idempotent under concurrency. Failures are logged at debug and
/// swallowed: the seam is best-effort by contract, the HTTP path stays the
/// only correctness path.
pub struct DbEmbeddingCacheStore {
    exec: Arc<dyn Executor>,
    model: String,
    dim: usize,
}

#[derive(Debug, SurrealValue)]
struct EmbeddingCacheRow {
    embedding: Option<ibis_db::value::Embedding>,
}

impl DbEmbeddingCacheStore {
    pub fn new(exec: Arc<dyn Executor>, model: String, dim: usize) -> Self {
        DbEmbeddingCacheStore { exec, model, dim }
    }

    /// Deterministic record id for a cache key: `embedding_cache:⟨sha256(key)⟩`.
    fn record_id_for(&self, key: &str) -> String {
        let sum = sha2::Sha256::digest(key.as_bytes());
        let hash: String = sum.iter().map(|b| format!("{b:02x}")).collect();
        format!("embedding_cache:⟨{hash}⟩")
    }
}

#[async_trait::async_trait]
impl ibis_ai::EmbeddingCacheStore for DbEmbeddingCacheStore {
    async fn get(&self, key: &str) -> Option<Vec<f32>> {
        let record_id = self.record_id_for(key);
        let sql = format!("SELECT embedding FROM {record_id};");
        let outcome = match self.exec.execute(&sql).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::debug!("embedding cache get failed (best-effort): {e}");
                return None;
            }
        };
        let rows = match ibis_db::value::decode_row_list::<EmbeddingCacheRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("embedding cache decode failed (best-effort): {e}");
                return None;
            }
        };
        let vec = rows.first().and_then(|r| r.embedding.as_ref())?;
        if vec.0.len() != self.dim {
            tracing::debug!(
                "embedding cache row dimension mismatch on {key} ({} != {})",
                vec.0.len(),
                self.dim
            );
            return None;
        }
        Some(vec.0.clone())
    }

    async fn put(&self, key: &str, embedding: Vec<f32>) {
        if embedding.len() != self.dim {
            tracing::debug!("embedding cache put skipped: dimension mismatch for {key}");
            return;
        }
        let record_id = self.record_id_for(key);
        let sql = format!(
            "UPSERT {record_id} SET key = $key, model = $model, dim = $dim, embedding = $vec, created_at = time::now();"
        );
        let mut vars = Vars::new();
        vars.insert("key".into(), Value::from_t(key));
        vars.insert("model".into(), Value::from_t(self.model.clone()));
        vars.insert("dim".into(), Value::from_t(self.dim as i64));
        vars.insert(
            "vec".into(),
            Value::from_t(
                embedding
                    .iter()
                    .map(|f| Value::from_t(*f))
                    .collect::<Vec<Value>>(),
            ),
        );
        if let Err(e) = self.exec.smart_query(&sql, vars).await {
            tracing::debug!("embedding cache put failed (best-effort): {e}");
        }
    }

    async fn prune(&self, ttl_secs: u64) -> usize {
        // 4.0 semantics (live-probed): `RETURN AFTER` on DELETE yields NONE
        // (the rows are gone post-op); `RETURN BEFORE` restores the
        // deleted-row list, which is the documented contract for the count.
        let sql = format!(
            "DELETE embedding_cache WHERE created_at < time::now() - {ttl_secs}s RETURN BEFORE;"
        );
        let outcome = match self.exec.execute(&sql).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::debug!("embedding cache prune failed (best-effort): {e}");
                return 0;
            }
        };
        ibis_db::value::decode_row_list::<EmbeddingCacheRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        )
        .map(|rows| rows.len())
        .unwrap_or(0)
    }
}

/// D12: DB-backed exact-match LLM response cache over the executor. Record
/// ids are the SHA-256 of the cache key — deterministic, so the UPSERT is
/// idempotent under concurrency. Failures are logged at debug and swallowed:
/// the seam is best-effort, the live provider stays the correctness path.
pub struct DbResponseCacheStore {
    exec: Arc<dyn Executor>,
}

#[derive(Debug, SurrealValue)]
struct ResponseCacheRow {
    response: Option<String>,
    finish_reason: Option<String>,
}

impl DbResponseCacheStore {
    pub fn new(exec: Arc<dyn Executor>) -> Self {
        DbResponseCacheStore { exec }
    }

    /// Deterministic record id for a cache key:
    /// `llm_response_cache:⟨sha256(key)⟩`.
    fn record_id_for(&self, key: &str) -> String {
        let sum = sha2::Sha256::digest(key.as_bytes());
        let hash: String = sum.iter().map(|b| format!("{b:02x}")).collect();
        format!("llm_response_cache:⟨{hash}⟩")
    }
}

#[async_trait::async_trait]
impl ibis_ai::ResponseCacheStore for DbResponseCacheStore {
    async fn get(&self, key: &str) -> Option<ibis_ai::CachedResponse> {
        let record_id = self.record_id_for(key);
        let sql = format!("SELECT response, finish_reason FROM {record_id};");
        let outcome = match self.exec.execute(&sql).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::debug!("response cache get failed (best-effort): {e}");
                return None;
            }
        };
        let rows = match ibis_db::value::decode_row_list::<ResponseCacheRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        ) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("response cache decode failed (best-effort): {e}");
                return None;
            }
        };
        let row = rows.first()?;
        let text = row.response.clone().filter(|text| !text.is_empty())?;
        Some(ibis_ai::CachedResponse {
            text,
            finish_reason: row.finish_reason.clone().unwrap_or_default(),
        })
    }

    async fn put(&self, key: &str, response: ibis_ai::CachedResponse) {
        let record_id = self.record_id_for(key);
        let sql = format!(
            "UPSERT {record_id} SET key = $key, response = $response, finish_reason = $finish_reason, created_at = time::now();"
        );
        let mut vars = Vars::new();
        vars.insert("key".into(), Value::from_t(key));
        vars.insert("response".into(), Value::from_t(response.text));
        vars.insert(
            "finish_reason".into(),
            Value::from_t(response.finish_reason),
        );
        if let Err(e) = self.exec.smart_query(&sql, vars).await {
            tracing::debug!("response cache put failed (best-effort): {e}");
        }
    }

    async fn prune(&self, ttl_secs: u64) -> usize {
        // 4.0 semantics (live-probed on B6): `RETURN BEFORE` restores the
        // deleted-row list; `RETURN AFTER` yields NONE.
        let sql = format!(
            "DELETE llm_response_cache WHERE created_at < time::now() - {ttl_secs}s RETURN BEFORE;"
        );
        let outcome = match self.exec.execute(&sql).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::debug!("response cache prune failed (best-effort): {e}");
                return 0;
            }
        };
        ibis_db::value::decode_row_list::<ResponseCacheRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        )
        .map(|rows| rows.len())
        .unwrap_or(0)
    }
}

/// DB-backed chunk store for the batch manager (lane-C trait adapter over
/// the executor; bound variables + UPSERT per D17/F-002).
pub struct DbChunkStore(pub Arc<dyn Executor>);

#[derive(Debug, SurrealValue)]
struct IdContentRow {
    /// Record ids decode as driver `RecordId` values, not strings (the
    /// `SurrealValue` codec keeps native shapes) — normalized via
    /// [`ibis_db::ids::record_id_to_plain`] before use.
    id: Option<RecordId>,
    content: Option<String>,
}

/// `SELECT id, hash, date, repo.name as repo_name, message FROM commit …`
/// row projection (record ids decode like [`IdContentRow::id`]).
#[derive(Debug, SurrealValue)]
struct CommitRow {
    id: Option<RecordId>,
    hash: Option<String>,
    date: Option<String>,
    repo_name: Option<String>,
    message: Option<String>,
}

#[async_trait::async_trait]
impl ibis_ai::ChunkStore for DbChunkStore {
    async fn pending_chunks(
        &self,
        table: &str,
        limit: usize,
    ) -> Result<Vec<ibis_ai::PendingChunkRow>, ibis_ai::AiError> {
        let sql = format!(
            // 4.0: LIMIT without ORDER BY returns an arbitrary subset of a
            // split scan — the batch order is `id` (WQ-7).
            "SELECT id, content FROM {table} WHERE batch_status = 'pending' ORDER BY id LIMIT {limit};"
        );
        let outcome = self
            .0
            .execute(&sql)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        let rows = ibis_db::value::decode_row_list::<IdContentRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        )
        .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| ibis_ai::PendingChunkRow {
                id: r.id.map(|rid| ibis_db::ids::record_id_to_plain(&rid)),
                content: r.content,
            })
            .collect())
    }

    async fn pending_commits(
        &self,
        limit: usize,
    ) -> Result<Vec<ibis_ai::PendingCommitRow>, ibis_ai::AiError> {
        let sql = format!(
            "SELECT id, hash, date, repo.name as repo_name, message FROM commit WHERE batch_status = 'pending' ORDER BY id LIMIT {limit};"
        );
        let outcome = self
            .0
            .execute(&sql)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        let rows = ibis_db::value::decode_row_list::<CommitRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        )
        .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| ibis_ai::PendingCommitRow {
                id: r.id.map(|rid| ibis_db::ids::record_id_to_plain(&rid)),
                hash: r.hash.unwrap_or_default(),
                repo_name: r.repo_name.unwrap_or_default(),
                message: r.message.unwrap_or_default(),
                date: r.date,
            })
            .collect())
    }

    async fn save_chunk_embeddings(
        &self,
        updates: &[(String, Vec<f32>)],
    ) -> Result<(), ibis_ai::AiError> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut sql = String::from("BEGIN TRANSACTION; ");
        let mut vars = Vars::new();
        for (idx, (id, vec)) in updates.iter().enumerate() {
            if !ibis_db::ids::is_safe_record_id(id) {
                continue;
            }
            let var = format!("vec{idx}");
            vars.insert(
                var.clone(),
                Value::from_t(
                    vec.iter()
                        .map(|f| Value::from_t(*f))
                        .collect::<Vec<Value>>(),
                ),
            );
            sql.push_str(&format!(
                "UPDATE {id} SET embedding = ${var}, batch_status = 'completed'; "
            ));
        }
        sql.push_str("COMMIT TRANSACTION;");
        self.0
            .smart_query(&sql, vars)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(())
    }

    async fn apply_commit_txn(&self, txn: &ibis_ai::CommitTxn) -> Result<(), ibis_ai::AiError> {
        if txn.chunk_writes.is_empty()
            && txn.completed_commits.is_empty()
            && txn.failed_commits.is_empty()
        {
            return Ok(());
        }
        let mut sql = String::from("BEGIN TRANSACTION;\n");
        let mut vars = Vars::new();
        for (idx, write) in txn.chunk_writes.iter().enumerate() {
            let chunk_id = ibis_db::ids::format_record_id("commit_chunk", &write.chunk_key);
            let content_var = format!("content{idx}");
            let hash_var = format!("hash{idx}");
            vars.insert(content_var.clone(), Value::from_t(write.content.as_str()));
            vars.insert(hash_var.clone(), Value::from_t(write.hash.as_str()));
            let date_set = match &write.date {
                Some(rfc3339) => {
                    let var = format!("date{idx}");
                    // `commit_date` is the adapter-supplied verbatim RFC3339
                    // string (schema: option<string>, display field); the
                    // read path converts it with `type::datetime`.
                    if chrono::DateTime::parse_from_rfc3339(rfc3339).is_ok() {
                        vars.insert(var.clone(), Value::from_t(rfc3339.as_str()));
                        format!("commit_date = ${var}, ")
                    } else {
                        String::new()
                    }
                }
                None => String::new(),
            };
            if ibis_db::ids::is_safe_record_id(&write.commit_record) {
                sql.push_str(&format!(
                    "UPSERT {chunk_id} SET content = ${content_var}, batch_status = 'pending', commit_hash = ${hash_var}, {date_set}updated_at = time::now();\n"
                ));
                sql.push_str(&format!(
                    "RELATE {}->has_commit_chunk->{chunk_id};\n",
                    write.commit_record
                ));
            }
        }
        for id in &txn.completed_commits {
            if !ibis_db::ids::is_safe_record_id(id) {
                continue;
            }
            sql.push_str(&format!("UPDATE {id} SET batch_status = 'completed';\n"));
        }
        for (id, code) in &txn.failed_commits {
            if !ibis_db::ids::is_safe_record_id(id) {
                continue;
            }
            let var = format!("err{}", vars.len());
            vars.insert(var.clone(), Value::from_t(code.as_str()));
            sql.push_str(&format!(
                "UPDATE {id} SET batch_status = 'failed', batch_error = ${var};\n"
            ));
        }
        sql.push_str("COMMIT TRANSACTION;");
        // F07/C1: the transaction carries `$content{n}`/`$hash{n}`/`$date{n}`
        // /`$err{n}` placeholders — it MUST go through `smart_query` so the
        // bound variables reach the server. The old `execute` discarded every
        // variable and SurrealDB 4 evaluates unbound params as NONE, silently
        // writing empty content/hash/date/error values.
        self.0
            .smart_query(&sql, vars)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(())
    }
}

/// Basename of a path as an owned String (module-level fn: inline closures
/// over `&OsStr` are inferred at a concrete lifetime inside Send futures —
/// rustc limitation; the loop/match form avoids the inference trap).
pub(crate) fn basename_string(path: &str) -> String {
    match std::path::Path::new(path).file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => String::new(),
    }
}

/// Repo name for a repository path ( fell back to `filepath.Base`; the
/// discovery root is not consulted at day-1 parity).
fn repo_name_for(_root: &str, path: &str) -> String {
    let name = basename_string(path);
    if name.is_empty() {
        path.to_string()
    } else {
        name
    }
}

/// Runs the background indexing pass over the active repos (
/// ): one git-ingest task + one code-ingest task per repo.
pub fn trigger_background_indexing(
    state: Arc<AppState>,
    _shutdown: tokio_util::sync::CancellationToken,
) {
    let repos = state.active_repos.clone();
    if repos.is_empty() || state.db().is_none() {
        return;
    }
    tracing::info!(
        "Triggering background indexing for {} repositories...",
        repos.len()
    );
    tokio::spawn(async move {
        let mut tasks = Vec::new();
        for r in repos {
            let state = state.clone();
            tasks.push(tokio::spawn(async move {
                let repo_name = repo_name_for(&state.config().discovery_root, &r);
                // 1. Git ingestion.
                tracing::info!("Background: Indexing Git history for {repo_name}...");
                let Some(db) = state.db() else {
                    return;
                };
                let patterns = state.ticket.reference_patterns_for_project(&repo_name);
                let mut refs: Vec<&str> = Vec::with_capacity(patterns.len());
                for p in &patterns {
                    refs.push(p.as_str());
                }
                if let Err(err) = ibis_ingest_git::ingest_repo(
                    db.clone(),
                    Some(state.ticket.clone()),
                    &r,
                    &repo_name,
                    state.config().redmine_concurrency.max(1) as usize,
                    &refs,
                )
                .await
                {
                    tracing::error!("Background: Git ingestion error for {repo_name}: {err}");
                }
                // 2. Code ingestion.
                tracing::info!("Background: Vectorizing codebase for {repo_name}...");
                let cfg = state.config();
                match ibis_ingest_code::ingest_codebase(
                    db.clone(),
                    Arc::new(ibis_ingest_code::AiClientAdapter(state.ai.clone())),
                    &r,
                    &repo_name,
                    Some(&cfg),
                    None,
                )
                .await
                {
                    Ok(()) => {
                        let search = state.search();
                        let live_db = state.db().expect("checked above");
                        refresh_stale_after_ingest(&live_db, &search, &r, &repo_name).await;
                    }
                    Err(err) => {
                        tracing::error!("Background: Code ingestion error for {repo_name}: {err}");
                    }
                }
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
        tracing::info!("Background: Initial indexing complete.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ibis_db::value::Object;
    use surrealdb::types::{RecordIdKey, ToSql};

    /// Regression: the BatchManager pending-row projections decode the `id`
    /// column as a driver `RecordId` (SurrealDB returns record ids natively),
    /// not as a string — a string field made every poll fail with
    /// "Failed to convert to none | string: Expected string, got record".
    fn row_object(pairs: Vec<(&str, Value)>) -> Value {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(k.to_string(), v);
        }
        Value::Object(obj)
    }

    #[test]
    fn pending_chunk_row_decodes_record_ids() {
        let raw = row_object(vec![
            (
                "id",
                Value::RecordId(surrealdb::types::RecordId::new("file_chunk", "chunk-1")),
            ),
            ("content", Value::String("chunk body".into())),
        ]);
        let sql = "SELECT id, content FROM file_chunk WHERE batch_status = 'pending' ORDER BY id LIMIT 100;";
        let rows = ibis_db::value::decode_row_list::<IdContentRow>(&raw, sql).expect("decode");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.content.as_deref(), Some("chunk body"));
        let id = row
            .id
            .as_ref()
            .map(ibis_db::ids::record_id_to_plain)
            .expect("record id decoded");
        assert_eq!(id, "file_chunk:chunk-1");
        // The normalized form must round-trip as a valid record id for the
        // interpolated `UPDATE {id} SET batch_status = 'completed'` writes.
        assert!(
            ibis_db::ids::is_safe_record_id(&id),
            "normalized id must pass the write guard: {id}"
        );
    }

    #[test]
    fn pending_commit_row_decodes_record_ids() {
        let raw = row_object(vec![
            (
                "id",
                Value::RecordId(surrealdb::types::RecordId::new("commit", "abc123")),
            ),
            ("hash", Value::String("abc123".into())),
            ("date", Value::String("2026-10-30T00:00:00Z".into())),
            ("repo_name", Value::String("ibis-assistant".into())),
            ("message", Value::String("feat: x".into())),
        ]);
        let sql = "SELECT id, hash, date, repo.name as repo_name, message FROM commit \
                   WHERE batch_status = 'pending' ORDER BY id LIMIT 100;";
        let rows = ibis_db::value::decode_row_list::<CommitRow>(&raw, sql).expect("decode");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.hash.as_deref(), Some("abc123"));
        assert_eq!(row.repo_name.as_deref(), Some("ibis-assistant"));
        let id = row
            .id
            .as_ref()
            .map(ibis_db::ids::record_id_to_plain)
            .expect("record id decoded");
        assert_eq!(id, "commit:abc123");
    }

    /// Ingest-carried keys (hashes/sanitized ids) keep the unwrapped plain
    /// form valid as a record id. Driver `to_sql` renders string keys with
    /// backticks — the project-canonical guillemet form comes from
    /// [`ibis_db::ids::format_record`], not from the driver.
    #[test]
    fn record_id_to_plain_keys_stay_valid_record_ids() {
        let record = surrealdb::types::RecordId {
            table: "file_chunk".into(),
            key: RecordIdKey::String("aB3_x-9".into()),
        };
        let plain = ibis_db::ids::record_id_to_plain(&record);
        assert_eq!(plain, "file_chunk:aB3_x-9");
        assert!(ibis_db::ids::is_safe_record_id(&plain), "{plain}");
        assert_eq!(
            Value::RecordId(record.clone()).to_sql(),
            "file_chunk:`aB3_x-9`"
        );
        assert_eq!(ibis_db::ids::format_record(&record), "file_chunk:⟨aB3_x-9⟩");
    }

    /// F07/C1: `apply_commit_txn` must dispatch through `smart_query` with
    /// EVERY placeholder bound. SurrealDB 4.0 evaluates an unbound param as
    /// `NONE` instead of erroring, so the test checks the values carried in
    /// `vars` — not just that the query "succeeded".
    #[tokio::test]
    async fn apply_commit_txn_binds_every_variable() {
        use ibis_ai::{ChunkStore as _, CommitChunkWrite, CommitTxn};

        struct CapturingDb {
            sqls: std::sync::Mutex<Vec<String>>,
            varss: std::sync::Mutex<Vec<Vars>>,
        }
        impl CapturingDb {
            fn new() -> Self {
                CapturingDb {
                    sqls: std::sync::Mutex::new(Vec::new()),
                    varss: std::sync::Mutex::new(Vec::new()),
                }
            }
        }
        #[async_trait::async_trait]
        impl Executor for CapturingDb {
            async fn execute(
                &self,
                sql: &str,
            ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError>
            {
                self.sqls.lock().unwrap().push(sql.to_string());
                Ok(ibis_db::executor::QueryOutcome::Single(Value::None))
            }
            async fn smart_query(
                &self,
                sql: &str,
                v: Vars,
            ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError>
            {
                self.sqls.lock().unwrap().push(sql.to_string());
                self.varss.lock().unwrap().push(v);
                Ok(ibis_db::executor::QueryOutcome::Single(Value::None))
            }
        }

        let var_as_text = |v: &Vars, key: &str| -> Option<String> {
            v.get(key).and_then(|val| match val {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
        };

        // Single chunk with date.
        {
            let db = Arc::new(CapturingDb::new());
            let store = DbChunkStore(db.clone() as Arc<dyn Executor>);
            let txn = CommitTxn {
                chunk_writes: vec![CommitChunkWrite {
                    commit_record: "commit:⟨abc123⟩".into(),
                    chunk_key: "abc123_chunk0".into(),
                    content: "meaning body".into(),
                    hash: "abc123".into(),
                    date: Some("2026-11-05T10:00:00Z".into()),
                }],
                completed_commits: vec!["commit:⟨abc123⟩".into()],
                failed_commits: vec![],
            };
            store.apply_commit_txn(&txn).await.expect("apply");
            let sqls = db.sqls.lock().unwrap();
            assert!(sqls.len() == 1, "one smart_query, got {sqls:?}");
            assert!(sqls[0].starts_with("BEGIN TRANSACTION;"));
            assert!(sqls[0].contains("UPSERT commit_chunk:⟨abc123_chunk0⟩"));
            assert!(sqls[0].contains("updated_at = time::now()"));
            assert!(sqls[0].contains("->has_commit_chunk->commit_chunk:⟨abc123_chunk0⟩"));
            assert!(sqls[0].contains("batch_status = 'completed'"));
            assert!(sqls[0].contains("COMMIT TRANSACTION;"));
            let varss = db.varss.lock().unwrap();
            let v = &varss[0];
            assert_eq!(var_as_text(v, "content0").as_deref(), Some("meaning body"));
            assert_eq!(var_as_text(v, "hash0").as_deref(), Some("abc123"));
            assert_eq!(
                var_as_text(v, "date0").as_deref(),
                Some("2026-11-05T10:00:00Z"),
                "date0 must be bound as the verbatim RFC3339 string"
            );
        }

        // Multiple chunks, one without date, plus a failed commit — every
        // placeholder has its variable, including the error code.
        {
            let db = Arc::new(CapturingDb::new());
            let store = DbChunkStore(db.clone() as Arc<dyn Executor>);
            let txn = CommitTxn {
                chunk_writes: vec![
                    CommitChunkWrite {
                        commit_record: "commit:⟨aaa⟩".into(),
                        chunk_key: "aaa_chunk0".into(),
                        content: "first".into(),
                        hash: "aaa".into(),
                        date: None,
                    },
                    CommitChunkWrite {
                        commit_record: "commit:⟨bbb⟩".into(),
                        chunk_key: "bbb_chunk1".into(),
                        content: "second".into(),
                        hash: "bbb".into(),
                        date: Some("2026-01-01T00:00:00Z".into()),
                    },
                ],
                completed_commits: vec!["commit:⟨aaa⟩".into()],
                failed_commits: vec![("commit:⟨bbb⟩".into(), "embed_failed".into())],
            };
            store.apply_commit_txn(&txn).await.expect("apply");
            let sqls = db.sqls.lock().unwrap();
            let sql = &sqls[0];
            // Every placeholder in the SQL has a matching bound variable.
            for name in ["content0", "hash0", "content1", "hash1", "date1"] {
                assert!(
                    sql.contains(&format!("${name}")),
                    "SQL references {name}: {sql}"
                );
            }
            let varss = db.varss.lock().unwrap();
            let v = &varss[0];
            assert_eq!(var_as_text(v, "content0").as_deref(), Some("first"));
            assert_eq!(var_as_text(v, "hash0").as_deref(), Some("aaa"));
            assert_eq!(var_as_text(v, "content1").as_deref(), Some("second"));
            assert_eq!(var_as_text(v, "hash1").as_deref(), Some("bbb"));
            assert_eq!(
                var_as_text(v, "date1").as_deref(),
                Some("2026-01-01T00:00:00Z")
            );
            // The failed-commit error code is bound under its dynamic name
            // (every placeholder must have a variable).
            let err_name = sql
                .split("batch_error = $")
                .nth(1)
                .and_then(|rest| {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if name.is_empty() {
                        None
                    } else {
                        Some(name)
                    }
                })
                .expect("failed commit binds batch_error");
            assert_eq!(var_as_text(v, &err_name).as_deref(), Some("embed_failed"));
            // The no-date chunk carries no commit_date placeholder.
            assert!(
                !sql.contains("commit_date = $date0"),
                "dateless chunk must omit commit_date: {sql}"
            );
        }

        // Invalid chunk/commit ids are skipped from the SQL without
        // misaligning the variables of the valid ones.
        {
            let db = Arc::new(CapturingDb::new());
            let store = DbChunkStore(db.clone() as Arc<dyn Executor>);
            let txn = CommitTxn {
                chunk_writes: vec![
                    CommitChunkWrite {
                        commit_record: "commit:x; DROP".into(),
                        chunk_key: "evil_chunk0".into(),
                        content: "evil".into(),
                        hash: "evil".into(),
                        date: None,
                    },
                    CommitChunkWrite {
                        commit_record: "commit:⟨ccc⟩".into(),
                        chunk_key: "ccc_chunk1".into(),
                        content: "good".into(),
                        hash: "ccc".into(),
                        date: None,
                    },
                ],
                completed_commits: vec![],
                failed_commits: vec![],
            };
            store.apply_commit_txn(&txn).await.expect("apply");
            let sqls = db.sqls.lock().unwrap();
            let sql = &sqls[0];
            assert!(
                !sql.contains("evil_chunk0"),
                "unsafe id never interpolated: {sql}"
            );
            assert!(
                sql.contains("commit_chunk:⟨ccc_chunk1⟩"),
                "valid id written: {sql}"
            );
            let varss = db.varss.lock().unwrap();
            assert_eq!(var_as_text(&varss[0], "content1").as_deref(), Some("good"));
            assert_eq!(var_as_text(&varss[0], "hash1").as_deref(), Some("ccc"));
        }
    }
}
