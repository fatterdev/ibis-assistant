//! Tool dispatch + project/ingest/credential/settings/log handlers — the
//! AddTool closures (init_project through
//! analyze_logs). Memory/search tools live in [`crate::tools_memory`];
//! ticketing/repo-PR tools in [`crate::tools_ticket`].

use std::sync::Arc;

use ibis_db::value::Value as DbValue;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::RoleServer;
use serde_json::{json, Value};

use crate::args::{bool_arg, get_string_arg, str_arg};
use crate::progress::{report, ProgressReporter};
use crate::state::AppState;

/// Tool-handler outcome: text or error
/// (`NewToolResultError — isError=true, caller-visible).
pub type ToolResult = Result<CallToolResult, rmcp::ErrorData>;

pub fn text_result(text: impl Into<String>) -> ToolResult {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        text.into(),
    )]))
}

pub fn error_result(message: impl Into<String>) -> ToolResult {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        message.into(),
    )]))
}

pub fn json_result<T: serde::Serialize>(value: &T) -> ToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => text_result(text),
        Err(e) => error_result(format!("JSON marshal failed: {e}")),
    }
}

/// The request identity (`httpserver.ResolveActor(ctx, …) plumbing):
/// credentials ride the HTTP request extensions; stdio callers are local.
pub fn actor_context(ctx: &RequestContext<RoleServer>) -> ibis_search::ActorContext {
    let creds = ctx
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<ibis_core::auth::Credentials>());
    ibis_search::ActorContext {
        actor: creds.and_then(|c| c.actor.clone()),
        remote_untrusted: creds.map(|c| c.remote_untrusted).unwrap_or(false),
    }
}

/// The HTTP-injected ticketing auth ( context values from the auth
/// middleware headers; explicit fallbacks stay empty).
pub fn ticket_auth(ctx: &RequestContext<RoleServer>) -> ibis_ticketing::AuthContext {
    let creds = ctx
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<ibis_core::auth::Credentials>());
    ibis_ticketing::AuthContext {
        redmine_api_key: creds
            .and_then(|c| c.redmine_key.clone())
            .unwrap_or_default(),
        jira_email: creds.and_then(|c| c.jira_email.clone()).unwrap_or_default(),
        jira_api_token: creds
            .and_then(|c| c.jira_api_token.clone())
            .unwrap_or_default(),
        azure_devops_pat: creds
            .and_then(|c| c.azure_devops_pat.clone())
            .unwrap_or_default(),
    }
}

/// The HTTP-injected repopr auth.
pub fn repopr_auth(ctx: &RequestContext<RoleServer>) -> ibis_repopr::AuthContext {
    let creds = ctx
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<ibis_core::auth::Credentials>());
    ibis_repopr::AuthContext {
        azure_devops_pat: creds
            .and_then(|c| c.azure_devops_pat.clone())
            .unwrap_or_default(),
    }
}

/// The request git token (`auth.GitTokenContextKey — E-A15 made it an
/// explicit `sync_workspace` parameter).
pub fn request_git_token(ctx: &RequestContext<RoleServer>) -> Option<String> {
    ctx.extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<ibis_core::auth::Credentials>())
        .and_then(|c| c.git_token.clone())
        .filter(|t| !t.trim().is_empty())
}

/// Central dispatch (D3): routes a tool name to its handler.
pub async fn dispatch(
    name: &str,
    args: serde_json::Map<String, Value>,
    ctx: rmcp::handler::server::tool::ToolCallContext<'_, crate::mcp_server::IbisServer>,
) -> ToolResult {
    let state = ctx.service.state.clone();
    let progress = if crate::progress::tool_emits_progress(name) {
        ProgressReporter::attach(&ctx.request_context)
    } else {
        None
    };
    let actor = actor_context(&ctx.request_context);
    tracing::info!("MCP Tool Call: {name}");
    match name {
        "init_project" => init_project(&state, &args, progress, ctx).await,
        "update_project_status" => update_project_status(&state, &args).await,
        "sync_local_patch" => sync_local_patch(&state, &args, ctx).await,
        "git_configure_credentials" => git_configure_credentials(&state, &args).await,
        "ingest_code" => ingest_code(&state, &args, progress).await,
        "analyze_logs" => analyze_logs(&state, &args, progress).await,
        "settings_get" => settings_get(&state).await,
        "settings_apply" => settings_apply(&state, &args).await,
        "settings_open_ui" => settings_open_ui(&state).await,
        "optimize_knowledge" => optimize_knowledge(&state, &args).await,
        "ticket_get_capabilities" => crate::tools_ticket::ticket_get_capabilities(&state).await,
        "ticket_search" => {
            crate::tools_ticket::ticket_search(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "ticket_search_my" => {
            crate::tools_ticket::ticket_search_my(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "ticket_get" => {
            crate::tools_ticket::ticket_get(&state, &args, &ticket_auth(&ctx.request_context)).await
        }
        "ticket_create" => {
            crate::tools_ticket::ticket_create(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "ticket_update" => {
            crate::tools_ticket::ticket_update(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "ticket_add_comment" => {
            crate::tools_ticket::ticket_add_comment(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_assign" => {
            crate::tools_ticket::ticket_assign(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "ticket_transition" => {
            crate::tools_ticket::ticket_transition(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_list_statuses" => {
            crate::tools_ticket::ticket_list_statuses(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_search_users" => {
            crate::tools_ticket::ticket_search_users(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_list_projects" => {
            crate::tools_ticket::ticket_list_projects(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_mark_resolved" => {
            crate::tools_ticket::ticket_mark_resolved(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_mark_closed" => {
            crate::tools_ticket::ticket_mark_closed(
                &state,
                &args,
                &ticket_auth(&ctx.request_context),
            )
            .await
        }
        "ticket_reopen" => {
            crate::tools_ticket::ticket_reopen(&state, &args, &ticket_auth(&ctx.request_context))
                .await
        }
        "repo_pr_create" => {
            crate::tools_ticket::repo_pr_create(&state, &args, &repopr_auth(&ctx.request_context))
                .await
        }
        "repo_pr_complete" => {
            crate::tools_ticket::repo_pr_complete(&state, &args, &repopr_auth(&ctx.request_context))
                .await
        }
        _ => crate::tools_memory::dispatch(name, args, &state, &actor).await,
    }
}

/// Scenario: init_project handler with the F-006 2/3 checkpoint.
async fn init_project(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    progress: Option<ProgressReporter>,
    ctx: rmcp::handler::server::tool::ToolCallContext<'_, crate::mcp_server::IbisServer>,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    let job = ibis_ingest_dynamic::IngestionJob {
        project_name: project_name.clone(),
        origin_url: get_string_arg(args, "origin_url"),
        branch: get_string_arg(args, "branch"),
        commit: get_string_arg(args, "commit"),
        on_sync_done: None,
        on_complete: None,
    };
    state.set_session(crate::state::RepoSession {
        project_name: job.project_name.clone(),
        origin_url: job.origin_url.clone(),
        branch: job.branch.clone(),
        commit: job.commit.clone(),
    });

    let db = state.db();
    if let Some(db) = &db {
        if !job.project_name.is_empty() {
            // Scenario: the original built this with EscapeSQL; this crate binds (F-002) and uses
            // the D17 UPSERT for the baseline first-write.
            let project_id = ibis_db::ids::format_record_id(
                "project",
                &ibis_db::ids::sanitize_id(&job.project_name),
            );
            let (source, anchor) = if job.commit.is_empty() && !job.branch.is_empty() {
                ("session_branch", job.branch.as_str())
            } else {
                ("session", "")
            };
            let mut vars = ibis_db::executor::Vars::new();
            vars.insert("name".into(), DbValue::from_t(job.project_name.as_str()));
            vars.insert(
                "baseline_commit".into(),
                DbValue::from_t(job.commit.as_str()),
            );
            vars.insert("baseline_branch".into(), DbValue::from_t(anchor));
            vars.insert("baseline_source".into(), DbValue::from_t(source));
            if let Err(err) = db
                .smart_query(
                    &format!(
                        "UPSERT {project_id} SET name = $name, baseline_commit = $baseline_commit, baseline_branch = $baseline_branch, baseline_source = $baseline_source;"
                    ),
                    vars,
                )
                .await
            {
                tracing::warn!("project baseline persist failed: {err}");
            }
        }
    }

    report(&progress, 0, 3, "Syncing git workspace");
    let cfg = state.config();
    let repo_sync = ibis_ingest_dynamic::workspace::sync_workspace(
        &cfg,
        db.clone(),
        request_git_token(&ctx.request_context).as_deref(),
        &job.project_name,
        &job.origin_url,
        &job.branch,
        &job.commit,
    )
    .await;
    let outcome = match repo_sync {
        Ok(outcome) => outcome,
        Err(err) => {
            if let Some((provider, target)) = err.credentials_required_fields() {
                let payload = json!({
                    "status": "credentials_required",
                    "provider": provider,
                    "target": target,
                    "message": err.to_string(),
                });
                return text_result(serde_json::to_string(&payload).unwrap_or_default());
            }
            return error_result(format!("Failed to sync: {err}"));
        }
    };

    if let Some(db) = &db {
        if !project_name.is_empty() && !outcome.actual_commit.is_empty() {
            let project_id = ibis_db::ids::format_record_id(
                "project",
                &ibis_db::ids::sanitize_id(&project_name),
            );
            let mut vars = ibis_db::executor::Vars::new();
            vars.insert(
                "baseline_commit".into(),
                DbValue::from_t(outcome.actual_commit.as_str()),
            );
            vars.insert(
                "baseline_branch".into(),
                DbValue::from_t(job.branch.as_str()),
            );
            if let Err(err) = db
                .smart_query(
                    &format!(
                        "UPDATE {project_id} SET baseline_commit = $baseline_commit, baseline_branch = $baseline_branch, baseline_source = 'init_project';"
                    ),
                    vars,
                )
                .await
            {
                tracing::warn!("project baseline update failed: {err}");
            }
            state.set_session(crate::state::RepoSession {
                project_name: project_name.clone(),
                origin_url: job.origin_url.clone(),
                branch: job.branch.clone(),
                commit: outcome.actual_commit.clone(),
            });
        }
    }
    if !outcome.is_aligned {
        return text_result(format!(
            "{{\"status\": \"requires_patch\", \"closest_known_commit\": \"{}\", \"message\": \"Commit not found on remote. Please use sync_local_patch for perfect alignment or continue with the closest commit.\"}}",
            outcome.actual_commit
        ));
    }

    // 1/3 → git ingest → 2/3 (F-006) → code ingest → 3/3.
    report(&progress, 1, 3, "Ingesting git history and code");
    let Some(db) = db else {
        return error_result("Ingestion failed: database not connected");
    };
    let repo_path = outcome.repo_path.clone();
    let repo_name = crate::assembly::basename_string(&outcome.repo_path);
    let repo_name = if repo_name.is_empty() {
        outcome.repo_path.clone()
    } else {
        repo_name
    };
    let patterns = state.ticket.reference_patterns_for_project(&repo_name);
    let mut pattern_refs: Vec<&str> = Vec::with_capacity(patterns.len());
    for p in &patterns {
        pattern_refs.push(p.as_str());
    }
    if let Err(err) = ibis_ingest_git::ingest_repo(
        db.clone(),
        Some(state.ticket.clone()),
        &repo_path,
        &repo_name,
        cfg.redmine_concurrency.max(1) as usize,
        &pattern_refs,
    )
    .await
    {
        return error_result(format!(
            "Ingestion failed: git ingestion error for {repo_name}: {err}"
        ));
    }
    report(&progress, 2, 3, "Indexing AST chunks");
    if let Err(err) = ibis_ingest_code::ingest_codebase(
        db.clone(),
        Arc::new(ibis_ingest_code::AiClientAdapter(state.ai.clone())),
        &repo_path,
        &repo_name,
        Some(&cfg),
        None,
    )
    .await
    {
        return error_result(format!(
            "Ingestion failed: code ingestion error for {repo_name}: {err}"
        ));
    }
    crate::assembly::refresh_stale_after_ingest(&db, &state.search(), &repo_path, &repo_name).await;
    report(&progress, 3, 3, "Project ready");
    text_result(format!(
        "{{\"status\": \"aligned\", \"actual_commit\": \"{}\", \"message\": \"Workspace synced and ingestion completed\"}}",
        outcome.actual_commit
    ))
}

/// Scenario: update_project_status : fire-and-forget enqueue.
async fn update_project_status(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let job = ibis_ingest_dynamic::IngestionJob {
        project_name: get_string_arg(args, "project_name"),
        origin_url: get_string_arg(args, "origin_url"),
        branch: get_string_arg(args, "branch"),
        commit: get_string_arg(args, "commit"),
        on_sync_done: None,
        on_complete: None,
    };
    state.set_session(crate::state::RepoSession {
        project_name: job.project_name.clone(),
        origin_url: job.origin_url.clone(),
        branch: job.branch.clone(),
        commit: job.commit.clone(),
    });
    state.ingestion.enqueue(job);
    text_result("Update queued successfully.")
}

/// Scenario: sync_local_patch
async fn sync_local_patch(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    ctx: rmcp::handler::server::tool::ToolCallContext<'_, crate::mcp_server::IbisServer>,
) -> ToolResult {
    let _ = ctx;
    let project_name = get_string_arg(args, "project_name");
    let mut patch_text = get_string_arg(args, "patch");
    let commit = get_string_arg(args, "commit");
    if project_name.is_empty() || patch_text.is_empty() {
        return error_result("project_name and patch are required");
    }
    if project_name.contains("..") || project_name.contains(['/', '\\']) {
        return error_result("invalid project_name");
    }
    patch_text = patch_text.replace("\r\n", "\n");
    if !patch_text.ends_with('\n') {
        patch_text.push('\n');
    }

    let cfg = state.config();
    let resolved = match ibis_core::workspace::resolve(&cfg, &project_name) {
        Ok(r) => r,
        Err(err) => {
            return error_result(format!(
                "workspace not found for project {project_name:?}; run init_project first ({err})"
            ));
        }
    };
    if !resolved.owned {
        return text_result(
            json!({
                "status": "live_tree",
                "project_name": project_name,
                "workspace": resolved.path,
                "message": "Live working tree does not need a patch; dirty files are already visible to ingest.",
            })
            .to_string(),
        );
    }
    let repo_path = resolved.path.clone();
    match std::fs::metadata(&repo_path) {
        Ok(meta) if meta.is_dir() => {}
        _ => {
            return error_result(format!(
                "workspace not found for project {project_name:?}; run init_project first"
            ));
        }
    }

    let patch_path = std::path::Path::new(&repo_path).join("ibis-local.patch");
    if let Err(err) = std::fs::write(&patch_path, patch_text.as_bytes()) {
        return error_result(format!("failed to write patch: {err}"));
    }
    // Windows git compatibility: relative patch path ( used
    // "ibis-local.patch" with cmd.Dir = repoPath).
    let runner = ibis_git::GitRunner::anonymous();
    let apply_outcome = match runner
        .run(
            &repo_path,
            &["apply", "--whitespace=nowarn", "ibis-local.patch"],
        )
        .await
    {
        Ok(out) => Ok(out),
        Err(first_err) => {
            match runner
                .run(
                    &repo_path,
                    &["apply", "--3way", "--whitespace=nowarn", "ibis-local.patch"],
                )
                .await
            {
                Ok(out) => Ok(out),
                Err(second_err) => {
                    // Keep the failed patch for diagnosis.
                    let _ = std::fs::write(
                        std::path::Path::new(&repo_path).join("ibis-local.failed.patch"),
                        patch_text.as_bytes(),
                    );
                    Err(format!(
                        "git apply failed: {first_err}; retry: {second_err}"
                    ))
                }
            }
        }
    };
    if let Err(message) = apply_outcome {
        let _ = std::fs::remove_file(&patch_path);
        return error_result(message);
    }
    let _ = std::fs::remove_file(&patch_path);

    match state.get_session(&project_name) {
        Some(mut sess) => {
            if !commit.is_empty() {
                sess.commit = commit.clone();
            }
            state.set_session(sess);
        }
        None => {
            state.set_session(crate::state::RepoSession {
                project_name: project_name.clone(),
                commit: commit.clone(),
                ..Default::default()
            });
        }
    }

    let db = state.db();
    let reingest = db.is_some() && state.ai.is_embedding_functional();
    if reingest {
        if let Err(err) = ibis_ingest_code::ingest_codebase(
            db.clone().expect("checked above"),
            Arc::new(ibis_ingest_code::AiClientAdapter(state.ai.clone())),
            &repo_path,
            &project_name,
            Some(&cfg),
            None,
        )
        .await
        {
            tracing::warn!("sync_local_patch: ingest after patch failed: {err}");
        } else {
            crate::assembly::refresh_stale_after_ingest(
                db.as_ref().expect("checked above"),
                &state.search(),
                &repo_path,
                &project_name,
            )
            .await;
        }
    }

    let head = runner
        .run(&repo_path, &["rev-parse", "HEAD"])
        .await
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_default();
    text_result(format!(
        "{{\"status\":\"patched\",\"project_name\":\"{project_name}\",\"workspace_commit\":\"{head}\",\"requested_commit\":\"{commit}\",\"message\":\"Local patch applied and workspace re-indexed\"}}"
    ))
}

/// Scenario: git_configure_credentials
async fn git_configure_credentials(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let target = get_string_arg(args, "target");
    let provider = get_string_arg(args, "provider");
    let auth_type_str = get_string_arg(args, "auth_type");
    let token = get_string_arg(args, "token");
    let username = get_string_arg(args, "username");
    let ssh_key = get_string_arg(args, "ssh_private_key");

    if target.is_empty() {
        return error_result("Argument 'target' is required");
    }
    if auth_type_str.is_empty() {
        return error_result("Argument 'auth_type' is required");
    }
    let auth_type = match auth_type_str.to_lowercase().as_str() {
        "token" => {
            if token.is_empty() {
                return error_result("Argument 'token' is required for token auth");
            }
            ibis_git::AuthType::token()
        }
        "basic" => {
            if token.is_empty() {
                return error_result("Argument 'token' is required for basic auth (as password)");
            }
            ibis_git::AuthType::basic()
        }
        "ssh" => {
            if ssh_key.is_empty() {
                return error_result("Argument 'ssh_private_key' is required for ssh auth");
            }
            ibis_git::AuthType::ssh()
        }
        _ => return error_result("Invalid auth_type, must be: token, basic, or ssh"),
    };

    let cred = ibis_git::Credential {
        target,
        provider,
        auth_type,
        token,
        username,
        ssh_private_key: ssh_key,
    };
    let db = state.db();
    if db.is_none() {
        return error_result("Failed to save credentials: database client not initialized");
    }
    let store = credential_store(db.expect("checked above"));
    if let Err(err) = store.save_credential(&cred).await {
        return error_result(format!("Failed to save credentials: {err}"));
    }
    text_result("{\"status\": \"configured\", \"message\": \"Git credentials saved successfully\"}")
}

/// The credential store wired to the executor.
pub(crate) fn credential_store(
    db: Arc<dyn ibis_db::executor::Executor>,
) -> ibis_git::CredentialStore {
    ibis_git::CredentialStore::new(Some(
        Arc::new(ibis_ingest_dynamic::ExecutorCredentialDb(db)) as Arc<dyn ibis_git::CredentialDb>,
    ))
}

/// Scenario: ingest_code
async fn ingest_code(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    progress: Option<ProgressReporter>,
) -> ToolResult {
    let Some(db) = state.db() else {
        return error_result("Database not connected");
    };
    // Finding N-A03 : the gate checked the legacy
    // `cfg.GeminiKeys` slice, refusing cloud-only installs configured via
    // `ai.reasoning.clouds.gemini`; this crate gates on the embedding
    // capability itself.
    if !state.ai.is_embedding_functional() {
        return error_result("No AI Keys configured");
    }

    let cfg = state.config();
    let path = get_string_arg(args, "path");
    let targets: Vec<String> = if !path.is_empty() {
        vec![path]
    } else {
        state.active_repos.clone()
    };

    let mut output = String::new();
    for r in targets {
        let repo_name = crate::assembly::basename_string(&r);
        let repo_name = if repo_name.is_empty() {
            r.clone()
        } else {
            repo_name
        };
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
            Err(err) => output.push_str(&format!("Error scanning {r}: {err}\n")),
            Ok(()) => {
                report(&progress, 1, 1, &format!("Scanned {repo_name}"));
                crate::assembly::refresh_stale_after_ingest(&db, &state.search(), &r, &repo_name)
                    .await;
                output.push_str(&format!("Successfully scanned {r}\n"));
            }
        }
    }
    text_result(output)
}

/// Scenario: analyze_logs . Path components are sanitized at the
/// tool boundary (M-A04-class traversal defense, finding N-A04).
async fn analyze_logs(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    progress: Option<ProgressReporter>,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    let mut log_file = get_string_arg(args, "log_file");
    let log_text = get_string_arg(args, "log_text");
    if project_name.is_empty() {
        return error_result("project_name is required");
    }
    if log_text.is_empty() {
        return error_result("log_text is required");
    }
    if log_file.is_empty() {
        log_file = "mcp_tool.log".to_string();
    }
    let clean_lines: Vec<String> = log_text
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    if clean_lines.is_empty() {
        return text_result("[]");
    }
    let cfg = state.config();
    let path = std::path::Path::new(cfg.logs_root.trim())
        .join(sanitize_component(&project_name))
        .join(sanitize_component(&log_file))
        .display()
        .to_string();
    let mut analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        &path,
        &project_name,
        Some(cfg.clone()),
        state.db(),
        Some(state.ai.clone()),
    )
    .await;
    analyzer.progress =
        progress.map(|p| Arc::new(p) as Arc<dyn ibis_ingest_logs::ProgressReporter>);
    let (anomalies, err) = analyzer.process_batch_sync(&clean_lines).await;
    if let Some(err) = err {
        return error_result(format!("Analysis failed: {err}"));
    }
    if !anomalies.is_empty() {
        let mut max_severity = 0i64;
        let mut sb = String::new();
        sb.push_str(&format!(
            "MCP Synchronous Log Report for Project: {project_name} (File: {log_file})\r\n\r\n"
        ));
        sb.push_str(&format!(
            "Detected {} errors/anomalies:\r\n\r\n",
            anomalies.len()
        ));
        for e in &anomalies {
            if e.severity > max_severity {
                max_severity = e.severity;
            }
            sb.push_str(&format!("Category: {}\r\n", e.category));
            sb.push_str(&format!("Severity: {}/10\r\n", e.severity));
            if !e.file.is_empty() {
                sb.push_str(&format!("File: {}\r\n", e.file));
            }
            if !e.cause.is_empty() {
                sb.push_str(&format!("Cause: {}\r\n", e.cause));
            }
            sb.push_str("Stack Trace:\r\n");
            sb.push_str(&format!("{}\r\n\r\n", e.stack_trace));
        }
        let notifier = ibis_ingest_logs::Notifier::new(cfg, state.db());
        // Scenario: discarded the queue error (`notifier.QueueNotification(...)`);
        // this crate logs it at warn (N-A05).
        if let Err(err) = notifier
            .queue_notification(&project_name, &log_file, anomalies.len(), max_severity, &sb)
            .await
        {
            tracing::warn!("analyze_logs: notification queueing failed: {err}");
        }
    }
    json_result(&anomalies)
}

/// Path-component sanitizer (M-A04 pattern) for MCP-tool-supplied names.
fn sanitize_component(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' => '_',
            other => other,
        })
        .collect();
    if sanitized == ".." || sanitized == "." {
        "_".to_string()
    } else {
        sanitized
    }
}

/// Scenario: settings_get
async fn settings_get(state: &Arc<AppState>) -> ToolResult {
    let cfg = state.config();
    let probe = ibis_settings::probe(&cfg.ai.embedding.url).await;
    let rec = ibis_settings::recommend(&cfg, &probe);
    let out = json!({
        "probe": probe,
        "summary": rec.summary,
        "fields": rec.fields,
        "settings_url": ibis_settings::open_url(&cfg.public_base_url()),
        "provider": cfg.ai.reasoning.provider,
        "cloud_primary": cfg.ai.reasoning.resolve_cloud_primary(),
    });
    json_result(&out)
}

/// Scenario: settings_apply . The applied config swaps the
/// state snapshot (`*cfg = *out — race, N-A01).
async fn settings_apply(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let use_cloud = bool_arg(args, "use_cloud_when_no_gpu");
    let mut choices = ibis_settings::UserChoices {
        ai_mode: str_arg(args, "ai_mode"),
        gemini_key: str_arg(args, "gemini_key"),
        openai_key: str_arg(args, "openai_key"),
        claude_key: str_arg(args, "claude_key"),
        always_smallest_local: bool_arg(args, "always_smallest_local"),
        use_cloud_when_no_gpu: Some(use_cloud),
        ..Default::default()
    };
    if let Some(v) = args.get("use_ide_provided_models").and_then(Value::as_bool) {
        choices.use_ide_provided_models = Some(v);
    }
    let cfg = state.config();
    // Scenario: passed a probe that Apply ignored (K-A07); this crate drops
    // the dead parameter. `apply` is infallible (K-A07); the only fallible
    // step is the config write below.
    let (out, _diff) = ibis_settings::apply(&cfg, &choices);
    let path = if cfg.config_path.is_empty() {
        "config.json".to_string()
    } else {
        cfg.config_path.clone()
    };
    let data = match serde_json::to_string_pretty(&out) {
        Ok(d) => d,
        Err(err) => return error_result(err.to_string()),
    };
    if let Err(err) = std::fs::write(&path, data) {
        return error_result(err.to_string());
    }
    *state.cfg.write().unwrap() = out;
    text_result(format!(
        "Settings saved to {path}. Restart to reload providers."
    ))
}

/// Scenario: settings_open_ui
async fn settings_open_ui(state: &Arc<AppState>) -> ToolResult {
    let cfg = state.config();
    let search = state.search();
    let memory: Option<Arc<dyn ibis_settings::MemoryService>> =
        Some(Arc::new(ibis_settings::SearchMemoryAdapter(search)));
    match ibis_settings::ephemeral::open_settings_url_with_memory(&cfg, memory).await {
        Ok(url) => {
            let mut msg = format!("Open Settings UI: {url}");
            if cfg.is_plugin() && url.contains("token=") {
                msg.push_str("\n(Ephemeral loopback listener for plugin mode; token required. Prefer a personal daemon on :3030 if you want a shared runtime.)");
            }
            text_result(msg)
        }
        Err(err) => error_result(format!("settings UI: {err}")),
    }
}

/// Scenario: optimize_knowledge : detached via tokio::spawn.
async fn optimize_knowledge(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let iter = args
        .get("iterations")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(10);
    // Detach from the request scope: the MCP call returns immediately and
    // the MCP request context would cancel attached work.
    let state = state.clone();
    tokio::spawn(async move {
        let optimizer = ibis_optimization::Optimizer::new(
            state.db(),
            Some(Arc::new(crate::state::ClientAIAdapter(state.ai.clone()))
                as Arc<dyn ibis_ai::AIProvider>),
            Some(state.search()),
        );
        if let Err(err) = optimizer.optimize_loop(iter).await {
            tracing::error!("Optimization failed: {err}");
        } else {
            tracing::info!("Optimization completed for {iter} iterations.");
        }
    });
    text_result(format!("Optimization started for {iter} iterations."))
}
