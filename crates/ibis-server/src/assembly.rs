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
use ibis_db::value::{SurrealValue, Value};
use ibis_ingest_dynamic::queue::{IngestionJob, ProcessJobFn, ProjectIngestionManager};
use ibis_repopr::service::Service as RepoprService;
use ibis_search::search::SearchService;

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
    let mut service = ibis_ticketing::service::Service::new(cfg.clone(), db.clone());
    if !cfg.providers.redmine.base_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: redmine ({})",
            cfg.providers.redmine.base_url
        );
        service.register_provider(Arc::new(ibis_ticketing::RedmineProvider::new(
            &cfg.providers.redmine,
        )));
    }
    if !cfg.providers.jira.base_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: jira ({})",
            cfg.providers.jira.base_url
        );
        service.register_provider(Arc::new(ibis_ticketing::JiraProvider::new(
            &cfg.providers.jira,
        )));
    }
    if !cfg.providers.azure_devops.organization_url.is_empty() {
        tracing::info!(
            "Initializing ticket provider: azure_devops ({})",
            cfg.providers.azure_devops.organization_url
        );
        service.register_provider(Arc::new(ibis_ticketing::AzureDevOpsProvider::new(
            &cfg.providers.azure_devops,
        )));
    }
    if db.is_some() {
        if let Err(err) = ibis_ticketing::migrate_legacy_issues(db.as_ref()).await {
            tracing::warn!("Legacy issue migration warning: {err}");
        }
    }
    (Arc::new(service), err)
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

/// DB-backed chunk store for the batch manager (lane-C trait adapter over
/// the executor; bound variables + UPSERT per D17/F-002).
pub struct DbChunkStore(pub Arc<dyn Executor>);

#[derive(Debug, SurrealValue)]
struct IdContentRow {
    id: Option<String>,
    content: Option<String>,
}

#[async_trait::async_trait]
impl ibis_ai::ChunkStore for DbChunkStore {
    async fn pending_chunks(
        &self,
        table: &str,
        limit: usize,
    ) -> Result<Vec<ibis_ai::PendingChunkRow>, ibis_ai::AiError> {
        let sql = format!(
            "SELECT id, content FROM {table} WHERE batch_status = 'pending' LIMIT {limit};"
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
                id: r
                    .id
                    .map(|id| ibis_db::ids::coerce_record_id(&Value::from_t(id))),
                content: r.content,
            })
            .collect())
    }

    async fn pending_commits(
        &self,
        limit: usize,
    ) -> Result<Vec<ibis_ai::PendingCommitRow>, ibis_ai::AiError> {
        let sql = format!(
            "SELECT id, hash, date, repo.name as repo_name, message FROM commit WHERE batch_status = 'pending' LIMIT {limit};"
        );
        let outcome = self
            .0
            .execute(&sql)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        #[derive(Debug, SurrealValue)]
        struct CommitRow {
            id: Option<String>,
            hash: Option<String>,
            date: Option<String>,
            repo_name: Option<String>,
            message: Option<String>,
        }
        let rows = ibis_db::value::decode_row_list::<CommitRow>(
            outcome.first().unwrap_or(&Value::None),
            &sql,
        )
        .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|r| ibis_ai::PendingCommitRow {
                id: r
                    .id
                    .map(|id| ibis_db::ids::coerce_record_id(&Value::from_t(id))),
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
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339) {
                        vars.insert(
                            var.clone(),
                            Value::Datetime(surrealdb::types::Datetime::from(
                                dt.with_timezone(&chrono::Utc),
                            )),
                        );
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
        self.0
            .execute(&sql)
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
