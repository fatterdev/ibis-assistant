//! Ephemeral settings server.
//!
//! Serves the Settings UI + API on a **loopback-only** listener with a
//! one-shot URL access token exchanged for a same-site session cookie. Used
//! when runtime_mode=plugin (stdio has no HTTP of its own).
//!
//! Lifecycle semantics (parity):
//! - process-wide singleton: `ensure_ephemeral*` reuses a live listener,
//!   swaps the memory service, refreshes the idle timer and mints a **fresh
//!   one-shot token** ;
//! - the idle timer shuts the listener down after 20 minutes without
//!   requests (222-229);
//! - a successful `POST /api/v1/settings` schedules the shutdown 2s later
//!   ( — any POST status, as originally);
//! - `stop_ephemeral` is idempotent
//!
//! Rust hardening (finding K-08): timer/stop tasks verify with
//! `Arc::ptr_eq` that the server they are stopping is still the current
//! global, so a stale timer can never kill a freshly created listener.

use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use threski_core::config::Config;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::http::{open_url, ApiState, MemoryService};

/// Idle shutdown delay (`ephemeralIdleTimeout).
pub const EPHEMERAL_IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Grace before the post-apply shutdown.
pub const EPHEMERAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Session-token header (`tokenHeader).
pub const TOKEN_HEADER: &str = "X-Threski-Settings-Token";
/// Session cookie name (`sessionCookieName).
pub const SESSION_COOKIE_NAME: &str = "threski_settings_sess";

/// Errors surfaced by the ephemeral lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("ephemeral settings listen: {0}")]
    Listen(std::io::Error),
}

struct EphemeralInner {
    /// One-shot URL token; cleared after the first successful exchange
    /// (`urlToken).
    url_token: String,
    /// Browser session token after the exchange.
    session_token: String,
    base_url: String,
    shutdown: CancellationToken,
    /// Current idle-timer token — replaced on every request.
    idle_token: CancellationToken,
    serve_task: Option<tokio::task::JoinHandle<()>>,
    live: bool,
    stopping: bool,
}

impl Default for EphemeralInner {
    fn default() -> Self {
        EphemeralInner {
            url_token: String::new(),
            session_token: String::new(),
            base_url: String::new(),
            shutdown: CancellationToken::new(),
            idle_token: CancellationToken::new(),
            serve_task: None,
            live: false,
            stopping: false,
        }
    }
}

/// The ephemeral loopback settings server.
pub struct EphemeralServer {
    inner: StdMutex<EphemeralInner>,
    /// Shared config (`cfg *config.Config — POST writes through it).
    cfg: Arc<RwLock<Config>>,
    /// Memory service, swappable by reuse ( read `es.memory` per request).
    memory_cell: Arc<StdMutex<Option<Arc<dyn MemoryService>>>>,
}

/// Process-wide singleton ( package var `ephemeral` + `ephemeralMu`).
static EPHEMERAL: tokio::sync::Mutex<Option<Arc<EphemeralServer>>> =
    tokio::sync::Mutex::const_new(None);

/// Starts (or reuses) a loopback Settings listener and returns the browser
/// URL including a fresh one-shot `?token=`.
pub async fn ensure_ephemeral(cfg: Arc<RwLock<Config>>) -> Result<String, SettingsError> {
    ensure_ephemeral_with_memory(cfg, None).await
}

/// [`ensure_ephemeral`] with optional memory-browser access
///
pub async fn ensure_ephemeral_with_memory(
    cfg: Arc<RwLock<Config>>,
    memory: Option<Arc<dyn MemoryService>>,
) -> Result<String, SettingsError> {
    let mut guard = EPHEMERAL.lock().await;
    if let Some(es) = guard.as_ref() {
        if es.alive() {
            es.set_memory(memory);
            es.touch_idle();
            es.mint_url_token();
            return Ok(es.settings_url());
        }
    }
    let es = Arc::new(EphemeralServer::new(cfg, memory));
    es.start().await?;
    *guard = Some(es.clone());
    Ok(es.settings_url())
}

/// Shuts down the process-wide ephemeral listener if running
/// Idempotent.
pub async fn stop_ephemeral() {
    let es = { EPHEMERAL.lock().await.take() };
    if let Some(es) = es {
        es.stop().await;
    }
}

/// Stops `es` only while it is still the current global listener (Rust
/// hardening for the stale-timer race described in the module docs).
async fn stop_if_current(es: Arc<EphemeralServer>) {
    {
        let mut guard = EPHEMERAL.lock().await;
        match guard.as_ref() {
            Some(current) if Arc::ptr_eq(current, &es) => {
                guard.take();
            }
            _ => return,
        }
    }
    es.stop().await;
}

/// 32 hex chars from a CSPRNG — the uuid crate v4 path (getrandom), matching
/// the `crypto/rand` + `hex.EncodeToString` of 16 bytes
fn random_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

impl EphemeralServer {
    fn new(cfg: Arc<RwLock<Config>>, memory: Option<Arc<dyn MemoryService>>) -> Self {
        EphemeralServer {
            inner: StdMutex::new(EphemeralInner::default()),
            cfg,
            memory_cell: Arc::new(StdMutex::new(memory)),
        }
    }

    fn set_memory(&self, memory: Option<Arc<dyn MemoryService>>) {
        *self.memory_cell.lock().expect("memory cell") = memory;
    }

    fn memory(&self) -> Option<Arc<dyn MemoryService>> {
        self.memory_cell.lock().expect("memory cell").clone()
    }

    fn alive(&self) -> bool {
        let inner = self.inner.lock().expect("ephemeral state");
        inner.live && !inner.stopping
    }

    /// `OpenURL(base) + "?token=" + urlToken`.
    fn settings_url(&self) -> String {
        let inner = self.inner.lock().expect("ephemeral state");
        format!("{}?token={}", open_url(&inner.base_url), inner.url_token)
    }

    fn mint_url_token(&self) {
        self.inner.lock().expect("ephemeral state").url_token = random_token();
    }

    /// Binds `127.0.0.1:0`, builds the mux and serves until shutdown
    /// (`start).
    async fn start(self: &Arc<Self>) -> Result<(), SettingsError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(SettingsError::Listen)?;
        let port = listener.local_addr().map_err(SettingsError::Listen)?.port();
        let base = format!("http://127.0.0.1:{port}");

        let shutdown = CancellationToken::new();
        {
            let mut inner = self.inner.lock().expect("ephemeral state");
            inner.url_token = random_token();
            inner.session_token = random_token();
            inner.base_url = base;
            inner.shutdown = shutdown.clone();
            inner.stopping = false;
        }

        let app = self.build_router();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                })
                .await;
        });
        {
            let mut inner = self.inner.lock().expect("ephemeral state");
            inner.serve_task = Some(handle);
            // `alive required srv != nil && listener != nil && !stopping.
            inner.live = true;
        }
        // `resetIdleLocked after wiring the server
        self.touch_idle();
        Ok(())
    }

    /// `start's mux : stripped SPA, `/settings`
    /// redirect, API on both `/api/v1/settings` and the trailing-slash form —
    /// every route behind `es.auth`.
    fn build_router(self: &Arc<Self>) -> Router {
        let es = self.clone();
        let ui = crate::ui::settings_ui_router();
        Router::new()
            .route("/settings/", any(ui_stripped).with_state(ui.clone()))
            .route("/settings/{*rest}", any(ui_stripped).with_state(ui))
            .route(
                "/settings",
                any(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "/settings/")],
                        "Found.\n",
                    )
                }),
            )
            .route(
                "/api/v1/settings",
                any(ephemeral_settings_api).with_state(es.clone()),
            )
            .route(
                "/api/v1/settings/",
                any(ephemeral_settings_api).with_state(es),
            )
            .layer(middleware::from_fn_with_state(self.clone(), auth_layer))
    }

    /// One-shot exchange result: `Some(Set-Cookie value)` when the URL token
    /// was consumed, `None` for cookie/header sessions, `Err()` when
    /// unauthorized (`authorize).
    fn authorize(&self, req: &Request) -> Result<Option<String>, ()> {
        let q = crate::http::query_value(req.uri().query(), "token")
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut inner = self.inner.lock().expect("ephemeral state");
        if inner.session_token.is_empty() {
            return Err(());
        }
        // Query tokens are consumed atomically before cookie/header fallback
        // so a freshly minted URL cannot remain reusable in a browser that
        // already holds a session cookie
        if !q.is_empty() && !inner.url_token.is_empty() && q == inner.url_token {
            inner.url_token = String::new();
            return Ok(Some(format!(
                "{SESSION_COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict",
                inner.session_token
            )));
        }
        if cookie_token(req).is_some_and(|t| t == inner.session_token) {
            return Ok(None);
        }
        if header_token(req).is_some_and(|t| t == inner.session_token) {
            return Ok(None);
        }
        Err(())
    }

    /// Rearms the idle timer (`touchIdle/`resetIdleLocked`).
    fn touch_idle(self: &Arc<Self>) {
        let token = {
            let mut inner = self.inner.lock().expect("ephemeral state");
            inner.idle_token.cancel();
            let token = CancellationToken::new();
            inner.idle_token = token.clone();
            token
        };
        let es = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(EPHEMERAL_IDLE_TIMEOUT) => {
                    stop_if_current(es).await;
                }
            }
        });
    }

    /// Graceful stop: 3s drain budget, then abort (`stop,
    /// ).
    async fn stop(&self) {
        let (shutdown, mut serve_task) = {
            let mut inner = self.inner.lock().expect("ephemeral state");
            if inner.stopping {
                return;
            }
            inner.stopping = true;
            inner.live = false;
            inner.url_token = String::new();
            inner.session_token = String::new();
            inner.idle_token.cancel();
            (inner.shutdown.clone(), inner.serve_task.take())
        };
        shutdown.cancel();
        if let Some(handle) = serve_task.as_mut() {
            let drained = tokio::time::timeout(Duration::from_secs(3), &mut *handle).await;
            if drained.is_err() {
                handle.abort();
            }
        }
    }
}

/// Cookie header scan for [`SESSION_COOKIE_NAME`].
fn cookie_token(req: &Request) -> Option<String> {
    req.headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_cookie_token)
}

fn parse_cookie_token(raw: &str) -> Option<String> {
    raw.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case(SESSION_COOKIE_NAME)
            .then(|| value.trim().to_string())
    })
}

fn header_token(req: &Request) -> Option<String> {
    req.headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The auth wrapper (`EphemeralServer.auth):
/// unauthorized → 401; on success the idle timer rearm fires and an
/// apply-POST schedules the 2s delayed shutdown.
async fn auth_layer(State(es): State<Arc<EphemeralServer>>, req: Request, next: Next) -> Response {
    let is_apply_post = req.method() == Method::POST && req.uri().path() == "/api/v1/settings";
    let cookie = match es.authorize(&req) {
        Err(()) => return crate::http::plain_error(StatusCode::UNAUTHORIZED, "unauthorized"),
        Ok(cookie) => cookie,
    };
    es.touch_idle();
    let mut resp = next.run(req).await;
    if let Some(cookie) = cookie {
        if let Ok(value) = header::HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    if is_apply_post {
        // Scenario: spawns `time.Sleep(grace); StopEphemeral` — any POST status.
        let es = es.clone();
        tokio::spawn(async move {
            tokio::time::sleep(EPHEMERAL_SHUTDOWN_GRACE).await;
            stop_if_current(es).await;
        });
    }
    resp
}

/// The settings API handler wired to the server's shared config + swappable
/// memory service
async fn ephemeral_settings_api(State(es): State<Arc<EphemeralServer>>, req: Request) -> Response {
    let st = ApiState {
        cfg: es.cfg.clone(),
        memory: es.memory(),
    };
    crate::http::handle_settings(State(st), req).await
}

/// `http.StripPrefix("/settings/", UIHandler)` equivalent: rewrites
/// `/settings[/x]` to `/[/x]` and serves the embedded SPA.
async fn ui_stripped(State(ui): State<Router>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let rest = path
        .strip_prefix("/settings")
        .unwrap_or(&path)
        .trim_start_matches('/')
        .to_string();
    let new_path = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{rest}")
    };
    let (mut parts, body) = req.into_parts();
    let new_uri = match query {
        Some(q) if !q.is_empty() => Uri::builder()
            .path_and_query(format!("{new_path}?{q}"))
            .build(),
        _ => Uri::builder().path_and_query(new_path).build(),
    };
    match new_uri {
        Ok(uri) => parts.uri = uri,
        Err(_) => {
            return crate::http::plain_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid request path",
            )
        }
    }
    let req = Request::from_parts(parts, body);
    match ui.clone().oneshot(req).await {
        Ok(resp) => resp,
        Err(_) => crate::http::plain_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "settings ui handler error",
        ),
    }
}

/// Returns `base + "/settings/"` when a personal HTTP settings endpoint
/// already responds on `base` (2xx/3xx within 800ms); otherwise `""`
/// (`ProbePersonalSettings).
pub async fn probe_personal_settings(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    let url = open_url(base);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..400).contains(&status) {
                url
            } else {
                String::new()
            }
        }
        Err(_) => String::new(),
    }
}

/// Chooses personal HTTP when reachable, else the ephemeral plugin server
/// (`OpenSettingsURL).
///
/// F-004: the configured port is probed first
/// (`cfg.public_base_url`), then the hard-coded fallbacks 3030 **and 3031**
/// ( only tried 3030; lists 3031 as the settings probe fallback).
pub async fn open_settings_url(cfg: &Config) -> Result<String, SettingsError> {
    open_settings_url_with_memory(cfg, None).await
}

/// [`open_settings_url`] with optional memory-browser access
///
pub async fn open_settings_url_with_memory(
    cfg: &Config,
    memory: Option<Arc<dyn MemoryService>>,
) -> Result<String, SettingsError> {
    if !cfg.is_plugin() {
        return Ok(open_url(&cfg.public_base_url()));
    }
    let candidates = [
        cfg.public_base_url(),
        "http://127.0.0.1:3030".to_string(),
        "http://127.0.0.1:3031".to_string(),
    ];
    for candidate in candidates {
        let found = probe_personal_settings(&candidate).await;
        if !found.is_empty() {
            return Ok(found);
        }
    }
    ensure_ephemeral_with_memory(Arc::new(RwLock::new(cfg.clone())), memory).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cookie header parsing for the session exchange.
    #[test]
    fn parse_session_cookie_header() {
        assert_eq!(
            parse_cookie_token("other=1; threski_settings_sess=abc"),
            Some("abc".to_string())
        );
        assert_eq!(
            parse_cookie_token("threski_settings_sess=xyz"),
            Some("xyz".to_string())
        );
        assert_eq!(parse_cookie_token("other=1"), None);
        assert_eq!(parse_cookie_token(""), None);
    }
}
