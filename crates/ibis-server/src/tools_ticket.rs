//! Ticketing + repo-PR tool handlers — the closures for
//! the 15 `ticket_*` tools and the 2 `repo_pr_*` tools.

use std::sync::Arc;

use serde_json::Value;

use crate::args::{
    build_ticket_search_params_from_args, derive_repository_from_origin,
    format_ticket_search_response, get_bool_arg, get_int_arg, get_string_arg, get_string_slice_arg,
};
use crate::state::AppState;
use crate::tools::{error_result, json_result, text_result, ToolResult};

use ibis_ticketing::types::{AuthContext, CreateParams, UpdateParams};

/// Scenario: ticket_get_capabilities
pub async fn ticket_get_capabilities(state: &Arc<AppState>) -> ToolResult {
    json_result(&state.ticket.capabilities())
}

/// Scenario: ticket_search
pub async fn ticket_search(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let params = match build_ticket_search_params_from_args(args) {
        Ok(p) => p,
        Err(err) => return error_result(format!("Invalid arguments: {err}")),
    };
    match state.ticket.search(auth, params.clone()).await {
        Ok(result) => {
            match format_ticket_search_response(&result, &params, "Ticket search results") {
                Ok(out) => text_result(out),
                Err(err) => error_result(format!("JSON marshal failed: {err}")),
            }
        }
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_search_my
pub async fn ticket_search_my(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let params = match build_ticket_search_params_from_args(args) {
        Ok(p) => p,
        Err(err) => return error_result(format!("Invalid arguments: {err}")),
    };
    match state.ticket.search_my(auth, params.clone()).await {
        Ok(result) => {
            match format_ticket_search_response(&result, &params, "Ticket my-issues search results")
            {
                Ok(out) => text_result(out),
                Err(err) => error_result(format!("JSON marshal failed: {err}")),
            }
        }
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_get
pub async fn ticket_get(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Invalid arguments: id is required");
    }
    match state
        .ticket
        .get(
            auth,
            &get_string_arg(args, "provider"),
            &id,
            &get_string_arg(args, "project_key"),
        )
        .await
    {
        Ok(Some(issue)) => json_result(&issue),
        Ok(None) => error_result("Ticket not found"),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_create
pub async fn ticket_create(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let params = CreateParams {
        provider: get_string_arg(args, "provider"),
        project_key: get_string_arg(args, "project_key"),
        title: get_string_arg(args, "title"),
        description: get_string_arg(args, "description"),
        issue_type: get_string_arg(args, "type"),
        assignee: get_string_arg(args, "assignee"),
        priority: get_string_arg(args, "priority"),
        provider_fields_json: get_string_arg(args, "provider_fields_json"),
        ..Default::default()
    };
    match state.ticket.create(auth, params).await {
        Ok(issue) => json_result(&issue),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_update
pub async fn ticket_update(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Invalid arguments: id is required");
    }
    let params = UpdateParams {
        provider: get_string_arg(args, "provider"),
        notes: get_string_arg(args, "notes"),
        status: get_string_arg(args, "status"),
        issue_type: get_string_arg(args, "type"),
        assignee: get_string_arg(args, "assignee"),
        priority: get_string_arg(args, "priority"),
        workflow_action: get_string_arg(args, "workflow_action"),
        fixed_version: get_string_arg(args, "fixed_version"),
        provider_fields_json: get_string_arg(args, "provider_fields_json"),
        ..Default::default()
    };
    match state
        .ticket
        .update(auth, &id, &get_string_arg(args, "project_key"), params)
        .await
    {
        Ok(issue) => json_result(&issue),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_add_comment
pub async fn ticket_add_comment(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    let comment = get_string_arg(args, "comment");
    if id.is_empty() || comment.is_empty() {
        return error_result("Invalid arguments: id and comment are required");
    }
    match state
        .ticket
        .add_comment(
            auth,
            &get_string_arg(args, "provider"),
            &id,
            &get_string_arg(args, "project_key"),
            &comment,
        )
        .await
    {
        Ok(()) => text_result(format!("Comment added to ticket {id}.")),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_assign
pub async fn ticket_assign(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    let assignee = get_string_arg(args, "assignee");
    if id.is_empty() || assignee.is_empty() {
        return error_result("Invalid arguments: id and assignee are required");
    }
    match state
        .ticket
        .assign(
            auth,
            &get_string_arg(args, "provider"),
            &id,
            &get_string_arg(args, "project_key"),
            &assignee,
        )
        .await
    {
        Ok(()) => text_result(format!("Ticket {id} assigned to {assignee}.")),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_transition
pub async fn ticket_transition(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    let transition = get_string_arg(args, "transition");
    if id.is_empty() || transition.is_empty() {
        return error_result("Invalid arguments: id and transition are required");
    }
    match state
        .ticket
        .transition(
            auth,
            &get_string_arg(args, "provider"),
            &id,
            &get_string_arg(args, "project_key"),
            &transition,
        )
        .await
    {
        Ok(()) => text_result(format!("Ticket {id} transitioned to {transition}.")),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_list_statuses
pub async fn ticket_list_statuses(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    match state
        .ticket
        .list_statuses(
            auth,
            &get_string_arg(args, "provider"),
            &get_string_arg(args, "project_key"),
            &get_string_arg(args, "issue_type"),
        )
        .await
    {
        Ok(statuses) => json_result(&statuses),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_search_users
pub async fn ticket_search_users(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    let mut limit = get_int_arg(args, "limit").unwrap_or(0);
    if limit <= 0 {
        limit = 20;
    }
    match state
        .ticket
        .search_users(
            auth,
            &get_string_arg(args, "provider"),
            &get_string_arg(args, "project_key"),
            &get_string_arg(args, "query"),
            limit,
        )
        .await
    {
        Ok(users) => json_result(&users),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Scenario: ticket_list_projects
pub async fn ticket_list_projects(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    match state
        .ticket
        .list_projects(auth, &get_string_arg(args, "provider"))
        .await
    {
        Ok(projects) => json_result(&projects),
        Err(e) => error_result(format!("Ticketing error: {e}")),
    }
}

/// Workflow mark helpers ( ticket_mark_resolved/closed/reopen,
/// ): transition then best-effort comment append.
enum MarkAction {
    Resolve,
    Close,
    Reopen,
}

async fn ticket_mark(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
    action: MarkAction,
) -> ToolResult {
    let id = get_string_arg(args, "id");
    if id.is_empty() {
        return error_result("Invalid arguments: id is required");
    }
    let provider = get_string_arg(args, "provider");
    let project_key = get_string_arg(args, "project_key");
    let issue_type = get_string_arg(args, "issue_type");
    let res = match action {
        MarkAction::Resolve => {
            state
                .ticket
                .mark_resolved(auth, &provider, &id, &project_key, &issue_type)
                .await
        }
        MarkAction::Close => {
            state
                .ticket
                .mark_closed(auth, &provider, &id, &project_key, &issue_type)
                .await
        }
        MarkAction::Reopen => {
            state
                .ticket
                .reopen(auth, &provider, &id, &project_key, &issue_type)
                .await
        }
    };
    let res = match res {
        Ok(res) => res,
        Err(e) => return error_result(format!("Ticketing error: {e}")),
    };
    let notes = get_string_arg(args, "notes");
    if !notes.is_empty() {
        if let Err(e) = state
            .ticket
            .add_comment(auth, &provider, &id, &project_key, &notes)
            .await
        {
            tracing::warn!("ticket comment append after transition failed: {e}");
        }
    }
    json_result(&res)
}

/// Scenario: ticket_mark_resolved
pub async fn ticket_mark_resolved(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    ticket_mark(state, args, auth, MarkAction::Resolve).await
}

/// Scenario: ticket_mark_closed
pub async fn ticket_mark_closed(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    ticket_mark(state, args, auth, MarkAction::Close).await
}

/// Scenario: ticket_reopen
pub async fn ticket_reopen(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &AuthContext,
) -> ToolResult {
    ticket_mark(state, args, auth, MarkAction::Reopen).await
}

/// Scenario: repo_pr_create
pub async fn repo_pr_create(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &ibis_repopr::AuthContext,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    let mut origin_url = get_string_arg(args, "origin_url");
    let mut source_branch = get_string_arg(args, "source_branch");
    let mut repository = get_string_arg(args, "repository");
    if !project_name.is_empty() {
        if let Some(sess) = state.get_session(&project_name) {
            if origin_url.is_empty() {
                origin_url = sess.origin_url;
            }
            if source_branch.is_empty() {
                source_branch = sess.branch;
            }
        }
    }
    if repository.is_empty() {
        repository = derive_repository_from_origin(&origin_url);
    }
    if source_branch.is_empty() {
        return error_result(
            "Invalid arguments: source_branch is required (or initialize project session via init_project).",
        );
    }
    let params = ibis_repopr::CreateParams {
        provider: get_string_arg(args, "provider"),
        project_name,
        origin_url,
        repository,
        source_branch,
        target_branch: get_string_arg(args, "target_branch"),
        title: get_string_arg(args, "title"),
        description: get_string_arg(args, "description"),
        ticket_ids: get_string_slice_arg(args, "ticket_ids"),
        reviewer_ids: get_string_slice_arg(args, "reviewer_ids"),
        auto_complete: get_bool_arg(args, "auto_complete"),
    };
    match state.repopr.create_pr(auth, params).await {
        Ok(res) => json_result(&res),
        Err(e) => error_result(format!("PR error: {e}")),
    }
}

/// Scenario: repo_pr_complete
pub async fn repo_pr_complete(
    state: &Arc<AppState>,
    args: &serde_json::Map<String, Value>,
    auth: &ibis_repopr::AuthContext,
) -> ToolResult {
    let project_name = get_string_arg(args, "project_name");
    let mut repository = get_string_arg(args, "repository");
    if !project_name.is_empty() && repository.is_empty() {
        if let Some(sess) = state.get_session(&project_name) {
            repository = derive_repository_from_origin(&sess.origin_url);
        }
    }
    let params = ibis_repopr::CompleteParams {
        provider: get_string_arg(args, "provider"),
        project_name,
        repository,
        pr_id: get_string_arg(args, "pr_id"),
        delete_source_branch: get_bool_arg(args, "delete_source_branch"),
        squash: get_bool_arg(args, "squash"),
    };
    if params.pr_id.is_empty() {
        return error_result("Invalid arguments: pr_id is required");
    }
    match state.repopr.complete_pr(auth, params).await {
        Ok(res) => json_result(&res),
        Err(e) => error_result(format!("PR error: {e}")),
    }
}
