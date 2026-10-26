//! Settings HTTP API.
//!
//! Contract:
//! - `GET /api/v1/settings` → `{probe, summary, fields, config}` with all
//!   secrets masked;
//! - `POST /api/v1/settings` → applies choices, writes `config.json`, returns
//!   `{ok, path, diff}`;
//! - `GET /api/v1/settings/memory` → `{available, memories[]}` with
//!   kind/status/visibility/limit filters; `available:false` when the DB is
//!   down;
//! - `GET /api/v1/settings/memory/export` → `ibis-memories.json` attachment,
//!   default limit 1000;
//! - `POST /api/v1/settings/memory/share` → `{id, visibility: personal|team}`,
//!   owner-only promotion.
//!
//! The API [`Router`] uses **absolute** paths — `ibis-httpserver` merges it
//! (`mux.Handle("/api/v1/settings", …) without StripPrefix). The UI
//! router ([`ui::settings_ui_router`]) exposes **relative** routes (`/` =
//! index.html) and is nested with the `/settings/` prefix stripped.
//!
//! Actor semantics: `X-Ibis-Actor` is
//! trusted only from loopback peers; remote peers carry the
//! remote-untrusted marker so personal memories stay protected. When the mux
//! auth middleware already stamped [`Credentials`] the handler reuses it;
//! standalone mounts (ephemeral server) derive it from the peer address.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use ibis_core::auth::Credentials;
use ibis_core::config::Config;
use ibis_search::memory::{Memory, MemoryListParams, MemoryUpdateParams};
use ibis_search::ActorContext;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::engine::{self, mask_secret, Recommendations};

/// POST body cap for the apply endpoint (SEC fix: read the body unbounded
/// — `io.ReadAll` at ; the settings JSON is small, 1 MiB is
/// generous).
pub const MAX_APPLY_BODY_BYTES: usize = 1024 * 1024;

/// `OpenURL : `base` without trailing slashes + `/settings/`.
pub fn open_url(base: &str) -> String {
    format!("{}/settings/", base.trim_end_matches('/'))
}

/// Errors surfaced by the memory endpoints. The handler matched the
/// "database not connected" substring ; the Rust port is typed.
#[derive(Debug, thiserror::Error)]
pub enum MemoryApiError {
    #[error("database not connected")]
    DatabaseNotConnected,
    #[error("{0}")]
    Other(String),
}

impl MemoryApiError {
    fn from_search(err: ibis_search::SearchError) -> Self {
        let text = err.to_string();
        if matches!(err, ibis_search::SearchError::DatabaseNotConnected)
            || text.to_lowercase().contains("database not connected")
        {
            MemoryApiError::DatabaseNotConnected
        } else {
            MemoryApiError::Other(text)
        }
    }
}

/// The subset of the search service used by the Settings memory browser
/// (`MemoryService interface). `actor` carries the request
/// identity passed through `context.Context`.
#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn list_memories(
        &self,
        actor: &ActorContext,
        params: MemoryListParams,
    ) -> Result<Vec<Memory>, MemoryApiError>;

    /// Variadic actor tail mirrors `MemoryShare(ctx, id, actors ...string).
    async fn memory_share(
        &self,
        actor: &ActorContext,
        id: &str,
        actors: &[String],
    ) -> Result<(), MemoryApiError>;

    async fn memory_update(
        &self,
        actor: &ActorContext,
        params: MemoryUpdateParams,
    ) -> Result<(), MemoryApiError>;
}

/// Adapter over the live search service ( passed a `*search.Service` that
/// satisfies the interface directly; Rust bridges through this wrapper).
#[derive(Clone)]
pub struct SearchMemoryAdapter(pub Arc<ibis_search::SearchService>);

#[async_trait]
impl MemoryService for SearchMemoryAdapter {
    async fn list_memories(
        &self,
        actor: &ActorContext,
        params: MemoryListParams,
    ) -> Result<Vec<Memory>, MemoryApiError> {
        self.0
            .list_memories(actor, params)
            .await
            .map_err(MemoryApiError::from_search)
    }

    async fn memory_share(
        &self,
        actor: &ActorContext,
        id: &str,
        actors: &[String],
    ) -> Result<(), MemoryApiError> {
        self.0
            .memory_share(actor, id, actors)
            .await
            .map_err(MemoryApiError::from_search)
    }

    async fn memory_update(
        &self,
        actor: &ActorContext,
        params: MemoryUpdateParams,
    ) -> Result<(), MemoryApiError> {
        self.0
            .update_memory(actor, params)
            .await
            .map_err(MemoryApiError::from_search)
    }
}

/// Shared handler state.
#[derive(Clone)]
pub struct ApiState {
    /// Live config; POST writes both the file and this shared value (
    /// `*cfg = *out` at — a data race originally, locked here).
    pub cfg: Arc<RwLock<Config>>,
    pub memory: Option<Arc<dyn MemoryService>>,
}

/// Builds the settings API router with absolute paths, ready to be merged
/// into `ibis_httpserver::mux::handler`.
pub fn settings_api_router(
    cfg: Arc<RwLock<Config>>,
    memory: Option<Arc<dyn MemoryService>>,
) -> Router {
    settings_api_router_with_state(ApiState { cfg, memory })
}

pub(crate) fn settings_api_router_with_state(state: ApiState) -> Router {
    Router::new()
        .route(
            "/api/v1/settings",
            any(handle_settings).with_state(state.clone()),
        )
        .route(
            "/api/v1/settings/memory",
            any(handle_memory_list).with_state(state.clone()),
        )
        .route(
            "/api/v1/settings/memory/export",
            any(handle_memory_export).with_state(state.clone()),
        )
        .route(
            "/api/v1/settings/memory/share",
            any(handle_memory_share).with_state(state),
        )
}

/// `http.Error: text/plain body with a trailing newline + nosniff.
pub(crate) fn plain_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        format!("{msg}\n"),
    )
        .into_response()
}

/// `writeJSON
pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(err) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Request actor context.
pub(crate) fn actor_context(req: &Request) -> ActorContext {
    if let Some(creds) = req.extensions().get::<Credentials>() {
        return ActorContext {
            actor: creds.actor.clone(),
            remote_untrusted: creds.remote_untrusted,
        };
    }
    // Standalone mounts (ephemeral server): derive from the peer address.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.to_string())
        .unwrap_or_default();
    let actor = req
        .headers()
        .get("x-ibis-actor")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if ibis_core::auth::is_loopback_peer(&peer) {
        ActorContext {
            actor,
            remote_untrusted: false,
        }
    } else {
        ActorContext {
            actor: None,
            remote_untrusted: true,
        }
    }
}

/// GET/POST dispatcher (`handleSettingsAPI). Also the
/// entry point for the ephemeral server, which mounts the same handler.
pub(crate) async fn handle_settings(State(st): State<ApiState>, req: Request) -> Response {
    if req.method() == Method::GET {
        handle_get(&st).await
    } else if req.method() == Method::POST {
        handle_post(&st, req).await
    } else {
        plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
    }
}

/// GET /api/v1/settings (`handleGet).
async fn handle_get(st: &ApiState) -> Response {
    let snapshot = st.cfg.read().expect("config lock").clone();
    let mut view = snapshot.clone();
    ibis_core::reasoning::normalize_reasoning_config(
        &mut view.ai.reasoning,
        view.gemini_default_rpm,
    );
    let probe = engine::probe(&view.ai.embedding.url).await;
    let rec: Recommendations = engine::recommend(&view, &probe);

    // Mask secrets in the response copy . The port also
    // masks the legacy root `gemini_keys`, the log-ingestion API key, the
    // SMTP password and the DB password — leaked those in the view
    // (finding K-09).
    view.ai.reasoning.keys = Vec::new();
    for key in view.ai.reasoning.clouds.gemini.keys.iter_mut() {
        key.key = mask_secret(&key.key);
    }
    view.ai.reasoning.clouds.openai_compat.api_key =
        mask_secret(&view.ai.reasoning.clouds.openai_compat.api_key);
    view.ai.reasoning.clouds.claude.api_key = mask_secret(&view.ai.reasoning.clouds.claude.api_key);
    for key in view.gemini_keys.iter_mut() {
        key.key = mask_secret(&key.key);
    }
    view.log_ingestion.http.api_key = mask_secret(&view.log_ingestion.http.api_key);
    view.smtp.password = mask_secret(&view.smtp.password);
    view.db_password = mask_secret(&view.db_password);

    json_response(
        StatusCode::OK,
        &json!({
            "probe": probe,
            "summary": rec.summary,
            "fields": rec.fields,
            "config": view,
        }),
    )
}

/// POST body (`applyBody).
#[derive(Debug, Default, Deserialize)]
struct ApplyBody {
    #[serde(default)]
    ai_mode: String,
    #[serde(default)]
    local_model: String,
    #[serde(default)]
    always_smallest_local: bool,
    #[serde(default)]
    use_cloud_when_no_gpu: bool,
    #[serde(default)]
    gemini_key: String,
    #[serde(default)]
    openai_key: String,
    #[serde(rename = "openai_base_url", default)]
    openai_base_url: String,
    #[serde(default)]
    claude_key: String,
    #[serde(default)]
    discovery_root: String,
    #[serde(default)]
    logs_root: String,
    #[serde(default)]
    use_ide_provided_models: bool,
    #[serde(default)]
    memory_projection_enabled: bool,
    #[serde(rename = "memory_git_store_personal_claims", default)]
    memory_git_store_personal_claims: String,
}

/// POST /api/v1/settings (`handlePost).
async fn handle_post(st: &ApiState, req: Request) -> Response {
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_APPLY_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return plain_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("Request body exceeds {MAX_APPLY_BODY_BYTES} byte limit"),
            )
        }
    };
    let body: ApplyBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(err) => return plain_error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    // Validation : JSON 400 for the enum field.
    let claims_mode = body.memory_git_store_personal_claims.trim().to_lowercase();
    match claims_mode.as_str() {
        "" | "db_only" | "stub" | "in_repo" => {}
        _ => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({
                    "error": "memory_git_store_personal_claims must be db_only, stub, or in_repo"
                }),
            )
        }
    }

    let snapshot = st.cfg.read().expect("config lock").clone();
    let choices = crate::engine::UserChoices {
        ai_mode: body.ai_mode,
        local_model: body.local_model,
        always_smallest_local: body.always_smallest_local,
        use_cloud_when_no_gpu: Some(body.use_cloud_when_no_gpu),
        gemini_key: body.gemini_key,
        openai_key: body.openai_key,
        openai_base_url: body.openai_base_url,
        claude_key: body.claude_key,
        discovery_root: body.discovery_root,
        logs_root: body.logs_root,
        use_ide_provided_models: Some(body.use_ide_provided_models),
        memory_projection_enabled: Some(body.memory_projection_enabled),
        memory_git_store_personal_claims: claims_mode,
        ..Default::default()
    };
    let (out, diff) = crate::engine::apply(&snapshot, &choices);

    let mut path = snapshot.config_path.clone();
    if path.is_empty() {
        path = "config.json".to_string();
    }
    let data = match serde_json::to_string_pretty(&out) {
        Ok(d) => d,
        Err(err) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent); // ignored here too
    }
    let written = match std::fs::write(&path, &data) {
        Ok(()) => path.clone(),
        // Scenario: fallback: write config.json into the process CWD
        Err(_) => match std::fs::write("config.json", &data) {
            Ok(()) => "config.json".to_string(),
            Err(err) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        },
    };
    // Update the live shared config.
    {
        let mut live = st.cfg.write().expect("config lock");
        *live = out;
        live.config_path = written.clone();
    }
    json_response(
        StatusCode::OK,
        &json!({"ok": true, "path": written, "diff": diff}),
    )
}

/// `splitFilter
pub(crate) fn split_filter(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Minimal `net/url`-style query decoding:
/// `+` decodes to a space, `%XX` to the byte value.
pub(crate) fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(k) == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// GET /api/v1/settings/memory.
async fn handle_memory_list(state: State<ApiState>, req: Request) -> Response {
    memory_list(state, req, false).await
}

/// GET /api/v1/settings/memory/export.
async fn handle_memory_export(state: State<ApiState>, req: Request) -> Response {
    memory_list(state, req, true).await
}

/// Memory list/export body
async fn memory_list(State(st): State<ApiState>, req: Request, export: bool) -> Response {
    if req.method() != Method::GET {
        return plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let unavailable = || {
        json_response(
            StatusCode::OK,
            &json!({
                "available": false,
                "memories": [],
                "message": "Memory database is not available.",
            }),
        )
    };
    let Some(memory) = st.memory.clone() else {
        return unavailable();
    };
    let mut limit = if export { 1000 } else { 100 };
    if let Some(requested) = query_value(req.uri().query(), "limit") {
        if let Ok(n) = requested.trim().parse::<i64>() {
            if n > 0 {
                limit = n as usize;
            }
        }
    }
    let params = MemoryListParams {
        kinds: split_filter(&query_value(req.uri().query(), "kind").unwrap_or_default()),
        status: query_value(req.uri().query(), "status").unwrap_or_default(),
        visibility: query_value(req.uri().query(), "visibility").unwrap_or_default(),
        limit,
        actor: String::new(),
    };
    let actor = actor_context(&req);
    match memory.list_memories(&actor, params).await {
        Err(MemoryApiError::DatabaseNotConnected) => unavailable(),
        Err(MemoryApiError::Other(err)) => {
            json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": err}))
        }
        Ok(memories) => {
            if export {
                let mut resp = json_response(StatusCode::OK, &memories);
                resp.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    header::HeaderValue::from_static("attachment; filename=\"ibis-memories.json\""),
                );
                resp
            } else {
                json_response(
                    StatusCode::OK,
                    &json!({
                        "available": true,
                        "memories": memories,
                    }),
                )
            }
        }
    }
}

/// POST body (`memoryShareBody).
#[derive(Debug, Default, Deserialize)]
struct MemoryShareBody {
    #[serde(default)]
    id: String,
    #[serde(default)]
    visibility: String,
}

/// POST /api/v1/settings/memory/share (`handleMemoryShare).
async fn handle_memory_share(State(st): State<ApiState>, req: Request) -> Response {
    if req.method() != Method::POST {
        return plain_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let actor = actor_context(&req);
    let Some(memory) = st.memory.clone() else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error": "memory database is not available"}),
        );
    };
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_APPLY_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => return plain_error(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    let mut body: MemoryShareBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": "invalid JSON body"}),
            )
        }
    };
    body.id = body.id.trim().to_string();
    body.visibility = body.visibility.trim().to_lowercase();
    if body.id.is_empty() || (body.visibility != "personal" && body.visibility != "team") {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "id and visibility (personal or team) are required"}),
        );
    }
    let result = if body.visibility == "team" {
        memory.memory_share(&actor, &body.id, &[]).await
    } else {
        memory
            .memory_update(
                &actor,
                MemoryUpdateParams {
                    id: body.id.clone(),
                    visibility: Some("personal".to_string()),
                    ..Default::default()
                },
            )
            .await
    };
    match result {
        Err(err) => json_response(StatusCode::CONFLICT, &json!({"error": err.to_string()})),
        Ok(()) => json_response(
            StatusCode::OK,
            &json!({"ok": true, "visibility": body.visibility}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: splitFilter semantics.
    #[test]
    fn split_filter_trims_and_drops_empty() {
        assert_eq!(
            split_filter(" rule , decision ,,  "),
            vec!["rule", "decision"]
        );
        assert!(split_filter("").is_empty());
    }

    /// Query decoding parity with net/url (percent + plus).
    #[test]
    fn query_value_decodes() {
        assert_eq!(
            query_value(Some("kind=working_state&visibility=team"), "visibility"),
            Some("team".to_string())
        );
        assert_eq!(
            query_value(Some("a=b%20c&x=%2B"), "a"),
            Some("b c".to_string())
        );
        assert_eq!(query_value(Some("x=%2B"), "x"), Some("+".to_string()));
        assert_eq!(query_value(Some("kind=rule"), "missing"), None);
    }

    /// Scenario: OpenURL.
    #[test]
    fn open_url_trims_slashes() {
        assert_eq!(
            open_url("http://127.0.0.1:3030"),
            "http://127.0.0.1:3030/settings/"
        );
        assert_eq!(
            open_url("http://127.0.0.1:3030/"),
            "http://127.0.0.1:3030/settings/"
        );
    }
}
