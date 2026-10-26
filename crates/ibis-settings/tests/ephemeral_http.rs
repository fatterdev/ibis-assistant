//! Ephemeral lifecycle + F-004 probe
//! order test. The package-global server is shared across tests; this crate
//! serializes the same way with a static guard.

use std::sync::{Arc, RwLock};

use axum::http::header;
use ibis_core::config::default_config;
use ibis_settings::{
    ensure_ephemeral, open_settings_url, probe_personal_settings, stop_ephemeral, TOKEN_HEADER,
};
use reqwest::Client;

/// Scenario: ran the package tests serially against the process-wide listener.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn plugin_cfg() -> Arc<RwLock<ibis_core::config::Config>> {
    let mut cfg = default_config();
    cfg.runtime_mode = "plugin".into();
    Arc::new(RwLock::new(cfg))
}

fn token_of(url: &str) -> &str {
    url.split("token=")
        .nth(1)
        .unwrap_or_default()
        .split('&')
        .next()
        .unwrap_or_default()
}

/// `TestEphemeralTokenRejectAndAccept.
#[tokio::test]
async fn ephemeral_token_reject_and_accept() {
    let _guard = SERIAL.lock().await;

    let cfg = plugin_cfg();
    let open_url = ensure_ephemeral(cfg.clone()).await.unwrap();
    assert!(
        open_url.contains("127.0.0.1"),
        "expected loopback URL: {open_url}"
    );
    assert!(
        open_url.contains("token="),
        "expected token query: {open_url}"
    );

    let base = open_url.split('?').next().unwrap().to_string();
    // Without any token → 401.
    let plain = Client::new();
    let resp = plain.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // With the one-shot URL token → 200; the token is exchanged for a
    // session cookie.
    let jar_client = Client::builder().cookie_store(true).build().unwrap();
    let resp = jar_client.get(&open_url).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        set_cookie.starts_with("ibis_settings_sess="),
        "{set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Strict"), "{set_cookie}");

    // One-shot: replaying the same URL token without a session cookie fails.
    let resp = plain.get(&open_url).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The session cookie authorizes assets and the API without ?token=.
    let resp = jar_client
        .get(format!("{base}style.css"))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let api_url = base
        .trim_end_matches('/')
        .trim_end_matches("/settings")
        .to_string()
        + "/api/v1/settings";
    let resp = jar_client.get(&api_url).send().await.unwrap();
    assert_ne!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Reuse keeps the listener but mints a fresh one-shot URL token.
    let url2 = ensure_ephemeral(cfg.clone()).await.unwrap();
    assert_ne!(url2, open_url, "expected a new one-shot token URL on reuse");
    assert_eq!(
        url2.split('?').next(),
        open_url.split('?').next(),
        "reuse should keep listener host"
    );
    assert_ne!(
        token_of(&url2),
        token_of(&open_url),
        "distinct one-shot tokens"
    );

    // A fresh URL token is consumed even when the browser already holds a
    // session cookie ( atomic exchange).
    let resp = jar_client.get(&url2).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let resp = plain.get(&url2).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    stop_ephemeral().await;
    stop_ephemeral().await; // idempotent

    let _ = ensure_ephemeral(cfg.clone()).await.unwrap();
    stop_ephemeral().await;
}

/// `TestOpenSettingsURLPersonal.
#[tokio::test]
async fn open_settings_url_personal() {
    let mut cfg = default_config();
    cfg.runtime_mode = "personal".into();
    cfg.port = 3030;
    let u = open_settings_url(&cfg).await.unwrap();
    assert!(u.ends_with("/settings/"), "got {u}");
    assert!(
        !u.contains("token="),
        "personal URL should not include token: {u}"
    );
}

/// `TestHeaderSessionToken: the session token from the exchange cookie
/// authorizes via X-Ibis-Settings-Token too.
#[tokio::test]
async fn header_session_token() {
    let _guard = SERIAL.lock().await;
    let cfg = plugin_cfg();
    let open_url = ensure_ephemeral(cfg.clone()).await.unwrap();

    let resp = Client::new().get(&open_url).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let sess = set_cookie
        .split(';')
        .next()
        .and_then(|kv| kv.split_once('='))
        .filter(|(name, _)| name.trim() == "ibis_settings_sess")
        .map(|(_, value)| value.to_string());
    let Some(sess) = sess else {
        panic!("expected session cookie after URL token exchange: {set_cookie}");
    };

    let base = open_url.split('?').next().unwrap().to_string();
    let resp = Client::new()
        .get(&base)
        .header(TOKEN_HEADER, &sess)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    stop_ephemeral().await;
}

/// F-004 applied: a plugin probes the configured port FIRST (then the
/// hard-coded 3030/3031 fallbacks) before starting an ephemeral server.
#[tokio::test]
async fn open_settings_url_plugin_probes_config_port_first() {
    let app = axum::Router::new().route(
        "/settings/",
        axum::routing::get(|| async { "personal settings" }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_clone.cancelled().await;
            })
            .await;
    });

    // The stub responds (probe works standalone).
    let found = probe_personal_settings(&format!("http://127.0.0.1:{port}")).await;
    assert_eq!(found, format!("http://127.0.0.1:{port}/settings/"));

    // Plugin runtime with the personal server on the CONFIGURED port: the
    // probe finds it before any ephemeral server starts.
    let mut cfg = default_config();
    cfg.runtime_mode = "plugin".into();
    cfg.port = port as i64;
    let url = open_settings_url(&cfg).await.unwrap();
    assert_eq!(url, format!("http://127.0.0.1:{port}/settings/"));

    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
}

/// A stopped ephemeral server must be replaceable, and the one-shot tokens
/// of a fresh instance differ (regression for the original stale-timer race —
/// K-08).
#[tokio::test]
async fn ensure_ephemeral_replaces_stopped_server() {
    let _guard = SERIAL.lock().await;
    let cfg = plugin_cfg();
    let url1 = ensure_ephemeral(cfg.clone()).await.unwrap();
    stop_ephemeral().await;
    let url2 = ensure_ephemeral(cfg.clone()).await.unwrap();
    assert_ne!(url1, url2);
    stop_ephemeral().await;
}
