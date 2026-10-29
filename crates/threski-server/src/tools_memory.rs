//! Memory / knowledge-graph / temporal tool handlers — the
//! AddTool closures for the 26 memory tools plus the 3 code-search
//! tools (`ask_project`, `analyze_blast_radius`, `find_dead_code`).

use std::sync::Arc;

use serde_json::{json, Value};

use crate::args::bool_arg;
#[allow(unused_imports)]
use crate::args::get_string_arg;
use crate::state::AppState;
use crate::tools::{error_result, json_result, text_result, ToolResult};

use threski_search::memory::add::AddMemoryParams;
use threski_search::memory::capture::CaptureObservationParams;
use threski_search::memory::export::MemoryExportParams;
use threski_search::memory::insights::MemoryInsightsParams;
use threski_search::memory::{MemorySearchParams, MemoryUpdateParams};
use threski_search::ActorContext;

/// Memory-tool dispatch (called from [`crate::tools::dispatch`] for the
/// names below).
pub async fn dispatch(
    name: &str,
    args: serde_json::Map<String, Value>,
    state: &Arc<AppState>,
    actor: &ActorContext,
) -> ToolResult {
    match name {
        "provide_collaborative_memory" => provide_collaborative_memory(state, &args, actor).await,
        "save_reasoning_outcome" => save_reasoning_outcome(state, &args, actor).await,
        "memory_add" => memory_add(state, &args, actor).await,
        "memory_bootstrap_git_store" => memory_bootstrap_git_store(state, &args, actor).await,
        "memory_rebuild_git_notes" => memory_rebuild_git_notes(state, &args).await,
        "as_of_project" => as_of_project(state, &args).await,
        "memory_search" => memory_search(state, &args, actor).await,
        "memory_expand" => memory_expand(state, &args, actor).await,
        "memory_update" => memory_update(state, &args, actor).await,
        "memory_delete" => memory_delete(state, &args, actor).await,
        "memory_history" => memory_history(state, &args, actor).await,
        "store_rule" => store_rule(state, &args, actor).await,
        "get_rules" => get_rules(state, &args).await,
        "set_working_state" => set_working_state(state, &args, actor).await,
        "get_working_state" => get_working_state(state, &args, actor).await,
        "recent_failures" => recent_failures(state, &args).await,
        "why_changed" => why_changed(state, &args).await,
        "load_session_context" => load_session_context(state, &args, actor).await,
        "capture_observation" => capture_observation(state, &args, actor).await,
        "memory_consolidate" => memory_consolidate(state, &args).await,
        "memory_share" => memory_share(state, &args, actor).await,
        "memory_export" => memory_export(state, &args, actor).await,
        "memory_import" => memory_import(state, &args, actor).await,
        "ingest_design_note" => ingest_design_note(state, &args, actor).await,
        "memory_insights" => memory_insights(state, &args, actor).await,
        "knowledge_diff" => knowledge_diff(state, &args).await,
        "ask_project" => ask_project(state, &args, actor).await,
        "analyze_blast_radius" => analyze_blast_radius(state, &args).await,
        "find_dead_code" => find_dead_code(state, &args).await,
        _ => error_result(format!("Unknown tool: {name}")),
    }
}

/// resolution folded into AppState.
fn resolve_project(state: &Arc<AppState>, explicit: &str) -> Result<String, ToolResult> {
    match state.resolve_memory_write_project(explicit) {
        Ok(name) => Ok(name),
        Err(e) => Err(error_result(e)),
    }
}

/// F0.4 — resolves a memory's kind/title for the live-update resource URI
/// (single indexed lookup; degraded mode without a DB resolves to `None`).
async fn memory_identity(state: &Arc<AppState>, id: &str) -> Option<(String, String)> {
    match state
        .search()
        .expand_memory(&threski_search::ActorContext::LOCAL, &[id.to_string()], "")
        .await
    {
        Ok(mut rows) => rows.pop().map(|m| (m.kind, m.title)),
        Err(e) => {
            tracing::debug!("resources/updated resolve skipped for {id}: {e}");
            None
        }
    }
}

/// F0.4 — fires `notifications/resources/updated` for one mutated memory:
/// the exact `threski://memory/{kind}/{id}` URI, plus the
/// `threski://rules/{name}` twin for rules.
async fn fire_memory_changed(state: &Arc<AppState>, id: &str, kind: &str, title: &str) {
    if kind.is_empty() {
        return;
    }
    state
        .resource_subs
        .fire(&crate::resources::memory_uri(kind, id))
        .await;
    if kind == "rule" && !title.trim().is_empty() {
        state
            .resource_subs
            .fire(&crate::resources::rule_uri(title))
            .await;
    }
}

/// F0.4 — resolves-then-fires for mutations that carry the memory id but
/// not its kind/title (update/delete/share). Best-effort: never fails the
/// tool result.
async fn notify_memory_changed(state: &Arc<AppState>, id: &str) {
    if let Some((kind, title)) = memory_identity(state, id).await {
        fire_memory_changed(state, id, &kind, &title).await;
    }
}

/// Scenario: provide_collaborative_memory
async fn provide_collaborative_memory(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let memory_text = get_string_arg(args, "memory_text");
    let mut embedding: Vec<f64> = Vec::new();
    if let Some(emb) = args.get("embedding").and_then(Value::as_str) {
        if !emb.is_empty() {
            match serde_json::from_str::<Vec<f64>>(emb) {
                Ok(v) => embedding = v,
                Err(err) => return error_result(format!("invalid embedding JSON: {err}")),
            }
        }
    }
    let service = state.search();
    match service
        .add_collaborative_memory(actor, &project_name, &memory_text, embedding)
        .await
    {
        Ok(()) => {
            text_result("Collaborative memory successfully integrated into the knowledge graph.")
        }
        Err(e) => error_result(format!("Failed to add memory: {e}")),
    }
}

/// Scenario: save_reasoning_outcome
async fn save_reasoning_outcome(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let question = get_string_arg(args, "question");
    let outcome_text = get_string_arg(args, "outcome_text");
    let mut sources = Vec::new();
    if let Some(src) = args.get("useful_sources").and_then(Value::as_str) {
        if !src.is_empty() {
            for p in src.split(',') {
                let p = p.trim();
                if !p.is_empty() {
                    sources.push(p.to_string());
                }
            }
        }
    }
    let service = state.search();
    match service
        .save_reasoning_outcome(actor, &project_name, &question, &outcome_text, &sources)
        .await
    {
        Ok(()) => text_result("Reasoning outcome saved and structural reinforcement applied."),
        Err(e) => error_result(format!("Failed to save reasoning outcome: {e}")),
    }
}

/// Scenario: memory_add
async fn memory_add(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let content = get_string_arg(args, "content");
    if content.is_empty() {
        return error_result("Argument 'content' is required");
    }
    let mut tags = Vec::new();
    if !get_string_arg(args, "tags").is_empty() {
        for t in get_string_arg(args, "tags").split(',') {
            let t = t.trim();
            if !t.is_empty() {
                tags.push(t.to_string());
            }
        }
    }
    let mut source_refs = Vec::new();
    if !get_string_arg(args, "source_refs").is_empty() {
        for r in get_string_arg(args, "source_refs").split(',') {
            let r = r.trim();
            if !r.is_empty() {
                source_refs.push(r.to_string());
            }
        }
    }
    let mut embedding: Vec<f64> = Vec::new();
    if !get_string_arg(args, "embedding").is_empty() {
        match serde_json::from_str::<Vec<f64>>(&get_string_arg(args, "embedding")) {
            Ok(v) => embedding = v,
            Err(err) => return error_result(format!("invalid embedding JSON: {err}")),
        }
    }
    let confidence = args.get("confidence").and_then(Value::as_f64);
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Layer 3 git binding: resolve the live workspace so file-anchored
    // claims can follow the dirty-tree policy; empty on failure.
    let cfg = state.config();
    let workspace_root = threski_core::workspace::resolve(&cfg, &project_name)
        .map(|r| r.path)
        .unwrap_or_default();
    let mut anchor_mode = get_string_arg(args, "anchor_mode");
    if anchor_mode.is_empty() {
        anchor_mode = if cfg.runtime_mode == "server" {
            "require_clean".to_string()
        } else {
            "worktree".to_string()
        };
    }

    let mut params = AddMemoryParams {
        project_name,
        kind: get_string_arg(args, "kind"),
        title: get_string_arg(args, "title"),
        content,
        tags,
        status: get_string_arg(args, "status"),
        confidence,
        visibility: get_string_arg(args, "visibility"),
        owner: get_string_arg(args, "actor"),
        branch: get_string_arg(args, "branch"),
        valid_from: get_string_arg(args, "valid_from"),
        valid_until: get_string_arg(args, "valid_until"),
        derived_from: source_refs,
        supersedes: get_string_arg(args, "supersedes"),
        embedding,
        provenance_tool: "memory_add".to_string(),
        provenance_source_hash: get_string_arg(args, "source_hash"),
        provenance_source_path: get_string_arg(args, "source_path"),
        workspace_root,
        anchor_mode,
        introduced_in_oid: get_string_arg(args, "introduced_in"),
        ..Default::default()
    };
    let service = state.search();
    let id = match service.add_memory(actor, &mut params).await {
        Ok(id) => id,
        Err(e) => return error_result(format!("memory_add failed: {e}")),
    };
    state.maybe_project_memory(&params.project_name).await;
    let kind = params.kind.trim();
    let kind = if kind.is_empty() { "fact" } else { kind };
    fire_memory_changed(state, &id, kind, &params.title).await;
    text_result(json!({ "id": id, "status": "created" }).to_string())
}

/// Scenario: memory_bootstrap_git_store
async fn memory_bootstrap_git_store(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cfg = state.config();
    let resolved = match threski_core::workspace::resolve(&cfg, &project_name) {
        Ok(r) if !r.path.trim().is_empty() => r,
        _ => {
            return error_result(format!("no repository resolved for project {project_name}"));
        }
    };
    let service = state.search();
    match service
        .bootstrap_claim_store(
            actor,
            &resolved.path,
            &project_name,
            bool_arg(args, "dry_run"),
        )
        .await
    {
        Ok(result) => json_result(&result),
        Err(e) => error_result(format!("memory_bootstrap_git_store failed: {e}")),
    }
}

/// Scenario: memory_rebuild_git_notes
async fn memory_rebuild_git_notes(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cfg = state.config();
    let resolved = match threski_core::workspace::resolve(&cfg, &project_name) {
        Ok(r) if !r.path.trim().is_empty() => r,
        _ => {
            return error_result(format!("no repository resolved for project {project_name}"));
        }
    };
    let service = state.search();
    match service
        .rebuild_git_notes(&resolved.path, &project_name, bool_arg(args, "dry_run"))
        .await
    {
        Ok(result) => json_result(&result),
        Err(e) => error_result(format!("memory_rebuild_git_notes failed: {e}")),
    }
}

/// Scenario: as_of_project with the degraded-claim-read log
/// hook (PR #34).
async fn as_of_project(state: &Arc<AppState>, args: &serde_json::Map<String, Value>) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    if project_name.is_empty() {
        return error_result("Argument 'project_name' is required");
    }
    let reference = get_string_arg(args, "ref");
    if reference.is_empty() {
        return error_result("Argument 'ref' is required");
    }
    let mut projection = get_string_arg(args, "projection");
    if projection.is_empty() {
        projection = "claims".to_string();
    }
    match projection.as_str() {
        "claims" | "code" | "both" => {}
        other => {
            return error_result(format!(
                "projection {other:?} not available (claims | code | both)"
            ));
        }
    }
    let cfg = state.config();
    let resolved = match threski_core::workspace::resolve(&cfg, &project_name) {
        Ok(r) => r,
        Err(err) => {
            return error_result(format!(
                "workspace not resolvable for {project_name}: {err}"
            ));
        }
    };
    let actor = get_string_arg(args, "actor");
    let mut payload = serde_json::Map::new();
    payload.insert("projection".into(), json!(projection));
    let service = state.search();
    if projection == "claims" || projection == "both" {
        let (claims, truncated, as_of_ref) = match service
            .as_of_claims(&project_name, &resolved.path, &reference, &actor)
            .await
        {
            Ok(v) => v,
            Err(e) => return error_result(format!("as_of_project failed: {e}")),
        };
        payload.insert(
            "claims".into(),
            serde_json::to_value(&claims).unwrap_or_default(),
        );
        payload.insert("count".into(), json!(claims.len()));
        payload.insert("truncated".into(), json!(truncated));
        payload.insert("as_of_ref".into(), json!(as_of_ref));
        // Clone-only degraded mode (the claim-store addendum).
        if service.db.is_none() {
            payload.insert("degraded".into(), json!(true));
            log_degraded_claim_read(
                "as_of_project",
                &project_name,
                &format!(", ref={as_of_ref}"),
                claims.len(),
            );
        }
    }
    if projection == "code" || projection == "both" {
        let query = get_string_arg(args, "query");
        let code = match service.as_of_code(&query, &resolved.path, &reference).await {
            Ok(v) => v,
            Err(e) => return error_result(format!("as_of_project code failed: {e}")),
        };
        let as_of_ref = code
            .first()
            .map(|c| c.as_of_ref.clone())
            .unwrap_or_default();
        payload.insert(
            "code".into(),
            serde_json::to_value(&code).unwrap_or_default(),
        );
        if !payload.contains_key("as_of_ref") && !code.is_empty() {
            payload.insert("as_of_ref".into(), json!(as_of_ref));
        }
        if !payload.contains_key("count") {
            payload.insert("count".into(), json!(code.len()));
        }
    }
    let value = Value::Object(payload);
    json_result(&value)
}

/// Scenario: memory_search
async fn memory_search(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let query = get_string_arg(args, "query");
    if query.is_empty() {
        return error_result("Argument 'query' is required");
    }
    let mut kinds = Vec::new();
    if !get_string_arg(args, "kind").is_empty() {
        kinds = get_string_arg(args, "kind")
            .split(',')
            .map(String::from)
            .collect();
    }
    let mut tags = Vec::new();
    if !get_string_arg(args, "tags").is_empty() {
        tags = get_string_arg(args, "tags")
            .split(',')
            .map(String::from)
            .collect();
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let service = state.search();
    match service
        .search_memory(
            actor,
            MemorySearchParams {
                project_name: get_string_arg(args, "project_name"),
                query,
                kinds,
                status: get_string_arg(args, "status"),
                visibility: get_string_arg(args, "visibility"),
                tags,
                limit,
                actor: get_string_arg(args, "actor"),
            },
        )
        .await
    {
        Ok(results) => json_result(&results),
        Err(e) => error_result(format!("memory_search failed: {e}")),
    }
}

/// Scenario: memory_expand
async fn memory_expand(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Argument 'id' is required");
    }
    let depth = args
        .get("depth")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(1);
    let service = state.search();
    match service
        .memory_expand(actor, &id, depth, &get_string_arg(args, "actor"))
        .await
    {
        Ok(expansion) => json_result(&expansion),
        Err(e) => error_result(format!("memory_expand failed: {e}")),
    }
}

/// Scenario: memory_update
async fn memory_update(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Argument 'id' is required");
    }
    let mut params = MemoryUpdateParams {
        id: id.clone(),
        actor: get_string_arg(args, "actor"),
        ..Default::default()
    };
    if !get_string_arg(args, "content").is_empty() {
        params.content = Some(get_string_arg(args, "content"));
    }
    if !get_string_arg(args, "title").is_empty() {
        params.title = Some(get_string_arg(args, "title"));
    }
    if !get_string_arg(args, "kind").is_empty() {
        params.kind = Some(get_string_arg(args, "kind"));
    }
    if !get_string_arg(args, "status").is_empty() {
        params.status = Some(get_string_arg(args, "status"));
    }
    if let Some(v) = args.get("confidence").and_then(Value::as_f64) {
        params.confidence = Some(v);
    }
    if !get_string_arg(args, "tags").is_empty() {
        params.tags = Some(
            get_string_arg(args, "tags")
                .split(',')
                .map(String::from)
                .collect(),
        );
    }
    if !get_string_arg(args, "visibility").is_empty() {
        params.visibility = Some(get_string_arg(args, "visibility"));
    }
    if args.contains_key("valid_until") {
        params.valid_until = Some(get_string_arg(args, "valid_until"));
    }
    let service = state.search();
    if let Err(e) = service.update_memory(actor, params).await {
        return error_result(format!("memory_update failed: {e}"));
    }
    state.maybe_project_memory("").await;
    notify_memory_changed(state, &id).await;
    text_result(json!({ "id": id, "status": "updated" }).to_string())
}

/// Scenario: memory_delete
async fn memory_delete(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Argument 'id' is required");
    }
    let hard = bool_arg(args, "hard");
    // Resolve the URI identity before the mutation: a hard delete leaves no
    // record to look up afterwards.
    let identity = memory_identity(state, &id).await;
    let service = state.search();
    match service
        .memory_delete(actor, &id, hard, &get_string_arg(args, "actor"))
        .await
    {
        Ok(status) => {
            if let Some((kind, title)) = identity {
                fire_memory_changed(state, &id, &kind, &title).await;
            }
            text_result(json!({ "id": id, "status": status }).to_string())
        }
        Err(e) => error_result(format!("memory_delete failed: {e}")),
    }
}

/// Scenario: memory_history
async fn memory_history(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Argument 'id' is required");
    }
    let depth = args
        .get("depth")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(1);
    let service = state.search();
    match service
        .memory_history(actor, &id, depth, &get_string_arg(args, "actor"))
        .await
    {
        Ok(history) => json_result(&history),
        Err(e) => error_result(format!("memory_history failed: {e}")),
    }
}

/// Scenario: store_rule
async fn store_rule(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let title = get_string_arg(args, "title");
    let content = get_string_arg(args, "content");
    if title.is_empty() || content.is_empty() {
        return error_result("Arguments 'title' and 'content' are required");
    }
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let tags: Vec<String> = if get_string_arg(args, "tags").is_empty() {
        Vec::new()
    } else {
        get_string_arg(args, "tags")
            .split(',')
            .map(String::from)
            .collect()
    };
    let service = state.search();
    match service
        .store_rule(
            actor,
            &project_name,
            &title,
            &content,
            &tags,
            &get_string_arg(args, "branch"),
            &get_string_arg(args, "actor"),
        )
        .await
    {
        Ok(id) => {
            fire_memory_changed(state, &id, "rule", &title).await;
            text_result(json!({ "id": id, "status": "created" }).to_string())
        }
        Err(e) => error_result(format!("store_rule failed: {e}")),
    }
}

/// Scenario: get_rules
async fn get_rules(state: &Arc<AppState>, args: &serde_json::Map<String, Value>) -> ToolResult {
    let tags: Vec<String> = if get_string_arg(args, "tags").is_empty() {
        Vec::new()
    } else {
        get_string_arg(args, "tags")
            .split(',')
            .map(String::from)
            .collect()
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let service = state.search();
    match service
        .get_rules(
            &get_string_arg(args, "project_name"),
            &get_string_arg(args, "query"),
            &tags,
            limit,
        )
        .await
    {
        Ok(rules) => json_result(&rules),
        Err(e) => error_result(format!("get_rules failed: {e}")),
    }
}

/// Scenario: set_working_state
async fn set_working_state(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let content = get_string_arg(args, "content");
    if content.is_empty() {
        return error_result("Argument 'content' is required");
    }
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let service = state.search();
    match service
        .set_working_state(
            actor,
            &project_name,
            &get_string_arg(args, "branch"),
            &content,
            &get_string_arg(args, "actor"),
        )
        .await
    {
        Ok(id) => {
            fire_memory_changed(state, &id, "working_state", "").await;
            text_result(json!({ "id": id, "status": "set" }).to_string())
        }
        Err(e) => error_result(format!("set_working_state failed: {e}")),
    }
}

/// Scenario: get_working_state
async fn get_working_state(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let service = state.search();
    match service
        .get_working_state(
            actor,
            &get_string_arg(args, "project_name"),
            &get_string_arg(args, "branch"),
            &get_string_arg(args, "actor"),
        )
        .await
    {
        Ok(mem) => {
            let found = mem.is_some();
            json_result(&json!({ "found": found, "memory": mem }))
        }
        Err(e) => error_result(format!("get_working_state failed: {e}")),
    }
}

/// Scenario: recent_failures
async fn recent_failures(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let kinds: Vec<String> = if get_string_arg(args, "kind").is_empty() {
        Vec::new()
    } else {
        get_string_arg(args, "kind")
            .split(',')
            .map(String::from)
            .collect()
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let service = state.search();
    match service
        .recent_failures(&threski_search::memory::queries::RecentFailuresParams {
            project_name: get_string_arg(args, "project_name"),
            kinds,
            since: get_string_arg(args, "since"),
            limit,
            ..Default::default()
        })
        .await
    {
        Ok(results) => json_result(&results),
        Err(e) => error_result(format!("recent_failures failed: {e}")),
    }
}

/// Scenario: why_changed
async fn why_changed(state: &Arc<AppState>, args: &serde_json::Map<String, Value>) -> ToolResult {
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let service = state.search();
    match service
        .why_changed(&threski_search::memory::queries::WhyChangedParams {
            project_name: get_string_arg(args, "project_name"),
            file_path: get_string_arg(args, "file_path"),
            symbol_name: get_string_arg(args, "symbol_name"),
            issue_id: get_string_arg(args, "issue_id"),
            limit,
        })
        .await
    {
        Ok(res) => json_result(&res),
        Err(e) => error_result(format!("why_changed failed: {e}")),
    }
}

/// Scenario: load_session_context
async fn load_session_context(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let service = state.search();
    match service
        .load_session_context(
            actor,
            &get_string_arg(args, "project_name"),
            &get_string_arg(args, "branch"),
            &get_string_arg(args, "actor"),
        )
        .await
    {
        Ok(sess) => json_result(&sess),
        Err(e) => error_result(format!("load_session_context failed: {e}")),
    }
}

/// Scenario: capture_observation
async fn capture_observation(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let text = get_string_arg(args, "text");
    if text.is_empty() {
        return error_result("Argument 'text' is required");
    }
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let service = state.search();
    match service
        .capture_observation(
            actor,
            &CaptureObservationParams {
                project_name,
                text,
                source: get_string_arg(args, "source"),
                session_id: get_string_arg(args, "session_id"),
                owner: get_string_arg(args, "actor"),
            },
        )
        .await
    {
        Ok(id) => {
            fire_memory_changed(state, &id, "observation", "").await;
            text_result(json!({ "id": id, "status": "captured" }).to_string())
        }
        Err(e) => error_result(format!("capture_observation failed: {e}")),
    }
}

/// Scenario: memory_consolidate : returns immediately, runs
/// detached via tokio::spawn (NEVER tied to the MCP request scope).
async fn memory_consolidate(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    let max_items = args
        .get("max_items")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .map(|v| v as usize)
        .unwrap_or(20);
    let state = state.clone();
    let project = project_name.clone();
    tokio::spawn(async move {
        let optimizer = threski_optimization::Optimizer::new(
            state.db(),
            Some(Arc::new(crate::state::ClientAIAdapter(state.ai.clone()))
                as Arc<dyn threski_ai::AIProvider>),
            Some(state.search()),
        );
        match optimizer.consolidate_loop(&project, max_items).await {
            Err(err) => tracing::error!("Memory consolidation failed: {err}"),
            Ok(()) => {
                tracing::info!(
                    "Memory consolidation completed for up to {max_items} observations."
                );
                state.maybe_project_memory(&project).await;
            }
        }
    });
    text_result(format!(
        "Consolidation started for up to {max_items} candidate observations."
    ))
}

/// Scenario: memory_share
async fn memory_share(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    let visibility = get_string_arg(args, "visibility");
    if id.is_empty() || visibility.is_empty() {
        return error_result("Arguments 'id' and 'visibility' are required");
    }
    let service = state.search();
    match service
        .memory_share(
            actor,
            &id,
            &[visibility.clone(), get_string_arg(args, "actor")],
        )
        .await
    {
        Ok(()) => {
            notify_memory_changed(state, &id).await;
            text_result(
                json!({ "id": id, "visibility": visibility, "status": "updated" }).to_string(),
            )
        }
        Err(e) => error_result(format!("memory_share failed: {e}")),
    }
}

/// Scenario: memory_export
async fn memory_export(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let format = get_string_arg(args, "format");
    let service = state.search();
    match service
        .memory_export(
            actor,
            &MemoryExportParams {
                project_name: get_string_arg(args, "project_name"),
                kind: get_string_arg(args, "kind"),
                status: get_string_arg(args, "status"),
                visibility: get_string_arg(args, "visibility"),
                format: format.clone(),
                actor: get_string_arg(args, "actor"),
            },
        )
        .await
    {
        Ok((items, md)) => {
            if format.to_lowercase() == "markdown" {
                text_result(md)
            } else {
                json_result(&json!({ "count": items.len(), "items": items }))
            }
        }
        Err(e) => error_result(format!("memory_export failed: {e}")),
    }
}

/// Scenario: memory_import
async fn memory_import(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let items_json = get_string_arg(args, "items_json");
    if items_json.is_empty() {
        return error_result("Argument 'items_json' is required");
    }
    let items: Vec<threski_search::memory::Memory> = match serde_json::from_str(&items_json) {
        Ok(items) => items,
        Err(err) => return error_result(format!("invalid items_json: {err}")),
    };
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let service = state.search();
    match service
        .memory_import(
            actor,
            &project_name,
            &items,
            &get_string_arg(args, "on_conflict"),
            &get_string_arg(args, "actor"),
        )
        .await
    {
        Ok(result) => json_result(&result),
        Err(e) => error_result(format!("memory_import failed: {e}")),
    }
}

/// Scenario: ingest_design_note
async fn ingest_design_note(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let mut tags = Vec::new();
    if !get_string_arg(args, "tags").is_empty() {
        for t in get_string_arg(args, "tags").split(',') {
            let t = t.trim();
            if !t.is_empty() {
                tags.push(t.to_string());
            }
        }
    }
    let mut source_refs = Vec::new();
    if !get_string_arg(args, "source_refs").is_empty() {
        for r in get_string_arg(args, "source_refs").split(',') {
            let r = r.trim();
            if !r.is_empty() {
                source_refs.push(r.to_string());
            }
        }
    }
    let project_name = match resolve_project(state, &get_string_arg(args, "project_name")) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let service = state.search();
    let id = match service
        .ingest_design_note(
            actor,
            &threski_search::memory::design::IngestDesignNoteParams {
                project_name,
                title: get_string_arg(args, "title"),
                content: get_string_arg(args, "content"),
                path: get_string_arg(args, "path"),
                source_refs,
                tags,
                visibility: get_string_arg(args, "visibility"),
                actor: get_string_arg(args, "actor"),
            },
        )
        .await
    {
        Ok(id) => id,
        Err(e) => return error_result(format!("ingest_design_note failed: {e}")),
    };
    state
        .maybe_project_memory(&get_string_arg(args, "project_name"))
        .await;
    fire_memory_changed(state, &id, "design", &get_string_arg(args, "title")).await;
    text_result(json!({ "id": id, "kind": "design", "status": "created" }).to_string())
}

/// Scenario: memory_insights
async fn memory_insights(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let kinds: Vec<String> = if get_string_arg(args, "kind").is_empty() {
        Vec::new()
    } else {
        get_string_arg(args, "kind")
            .split(',')
            .map(String::from)
            .collect()
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let service = state.search();
    match service
        .memory_insights(
            actor,
            &MemoryInsightsParams {
                project_name: get_string_arg(args, "project_name"),
                kinds,
                status: get_string_arg(args, "status"),
                limit,
                actor: get_string_arg(args, "actor"),
                use_embeddings: bool_arg(args, "use_embeddings"),
                label_with_ai: bool_arg(args, "label_with_ai"),
            },
        )
        .await
    {
        Ok(result) => json_result(&result),
        Err(e) => error_result(format!("memory_insights failed: {e}")),
    }
}

/// Scenario: knowledge_diff with the degraded log hook.
async fn knowledge_diff(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    if project_name.is_empty() {
        return error_result("Argument 'project_name' is required");
    }
    let from_ref = get_string_arg(args, "from_ref");
    let to_ref = get_string_arg(args, "to_ref");
    if from_ref.is_empty() || to_ref.is_empty() {
        return error_result("Arguments 'from_ref' and 'to_ref' are required");
    }
    let max_claims = args
        .get("max_claims")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .map(|v| v as usize)
        .unwrap_or(0);
    let cfg = state.config();
    let resolved = match threski_core::workspace::resolve(&cfg, &project_name) {
        Ok(r) => r,
        Err(err) => {
            return error_result(format!(
                "workspace not resolvable for {project_name}: {err}"
            ));
        }
    };
    let diff_mode = match get_string_arg(args, "diff_mode").as_str() {
        "since_divergence" => threski_search::memory::kdiff::DiffMode::SinceDivergence,
        _ => threski_search::memory::kdiff::DiffMode::Explicit,
    };
    let service = state.search();
    match service
        .knowledge_diff(
            &project_name,
            &resolved.path,
            &from_ref,
            &to_ref,
            &get_string_arg(args, "actor"),
            diff_mode,
            max_claims,
        )
        .await
    {
        Ok(diff) => {
            if diff.degraded {
                log_degraded_claim_read(
                    "knowledge_diff",
                    &project_name,
                    &format!(", from={} to={}", diff.from_ref, diff.to_ref),
                    diff.added_claims.len() + diff.removed_claims.len(),
                );
            }
            json_result(&diff)
        }
        Err(e) => error_result(format!("knowledge_diff failed: {e}")),
    }
}

/// Scenario: ask_project with the degraded log hook.
async fn ask_project(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    actor: &ActorContext,
) -> ToolResult {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let branch_context = args
        .get("branch_or_commit")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let at_ref = get_string_arg(args, "at_ref");
    let service = state.search();

    if !at_ref.is_empty() {
        let project_name = get_string_arg(args, "project_name");
        if project_name.is_empty() {
            return error_result("project_name is required with at_ref");
        }
        let cfg = state.config();
        let resolved = match threski_core::workspace::resolve(&cfg, &project_name) {
            Ok(r) => r,
            Err(err) => {
                return error_result(format!(
                    "workspace not resolvable for {project_name}: {err}"
                ));
            }
        };
        let results = match service
            .ask_project_at_ref(
                actor,
                &query,
                &project_name,
                &resolved.path,
                &at_ref,
                &get_string_arg(args, "actor"),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return error_result(format!("as_of search failed: {e}")),
        };
        // Clone-only degraded mode (the claim-store addendum).
        if service.db.is_none() {
            let served = results
                .iter()
                .filter(|r| r.source_kind == "claim_file")
                .count();
            let payload = json!({ "degraded": true, "results": results });
            log_degraded_claim_read(
                "ask_project",
                &project_name,
                &format!(", ref={at_ref}"),
                served,
            );
            return json_result(&payload);
        }
        return json_result(&results);
    }

    match service.ask_project_agentic(&query, &branch_context).await {
        Ok(result) => json_result(&result),
        Err(e) => error_result(format!("Search failed: {e}")),
    }
}

/// Scenario: analyze_blast_radius
async fn analyze_blast_radius(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let repo_name = get_string_arg(args, "repo_name");
    let file_path = get_string_arg(args, "file_path");
    let symbol_name = get_string_arg(args, "symbol_name");
    if repo_name.is_empty() || file_path.is_empty() || symbol_name.is_empty() {
        return error_result("Missing required arguments");
    }
    let Some(db) = state.db() else {
        return error_result("Blast radius analysis failed: database not connected");
    };
    match threski_search::graph::analyze_blast_radius(&db, &repo_name, &file_path, &symbol_name)
        .await
    {
        Ok(res) => json_result(&res),
        Err(e) => error_result(format!("Blast radius analysis failed: {e}")),
    }
}

/// Scenario: find_dead_code
async fn find_dead_code(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
) -> ToolResult {
    let repo_name = get_string_arg(args, "repo_name");
    if repo_name.is_empty() {
        return error_result("Missing required argument: repo_name");
    }
    let Some(db) = state.db() else {
        return error_result("Dead code analysis failed: database not connected");
    };
    match threski_search::graph::find_dead_code(&db, &repo_name).await {
        Ok(res) if res.is_empty() => text_result("No dead code found."),
        Ok(res) => text_result(format!(
            "Found {} potentially dead symbols:\n- {}",
            res.len(),
            res.join("\n- ")
        )),
        Err(e) => error_result(format!("Dead code analysis failed: {e}")),
    }
}

/// `logDegradedClaimRead (PR #34): the server-side
/// observability line for clone-only degraded reads that served claims.
/// Zero-claim reads stay silent.
pub fn log_degraded_claim_read(tool: &str, project: &str, detail: &str, claims_served: usize) {
    if claims_served == 0 {
        return;
    }
    tracing::info!(
        "[degraded] {tool} served {claims_served} claim(s) from .threski/claims/ without SurrealDB (project={project}{detail})"
    );
}
