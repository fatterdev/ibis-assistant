//! Settings API GET/POST contract tests: covered these endpoints
//! only through the SPA; the shapes are pinned here so regressions surface at
//! test time (masked secrets, {ok, path, diff} apply response).

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use threski_core::config::{default_config, Config, GeminiKeyConfig};
use threski_settings::settings_api_router;
use tempfile::TempDir;
use tower::ServiceExt;

fn cfg_with_secrets(config_path: &str) -> Config {
    let mut cfg = default_config();
    cfg.config_path = config_path.to_string();
    cfg.ai.reasoning.clouds.gemini.keys = vec![GeminiKeyConfig {
        key: "raw-secret-key".into(),
        rpm: 100,
        ..Default::default()
    }];
    cfg.ai.reasoning.keys = cfg.ai.reasoning.clouds.gemini.keys.clone();
    cfg.gemini_keys = cfg.ai.reasoning.clouds.gemini.keys.clone();
    cfg.ai.reasoning.clouds.claude.api_key = "sk-ant-1234567890".into();
    cfg.ai.reasoning.clouds.openai_compat.api_key = "sk-openai-abcdefgh".into();
    cfg.log_ingestion.http.api_key = "upload-secret-key".into();
    cfg.smtp.password = "smtp-password".into();
    cfg.db_password = "db-password".into();
    cfg
}

async fn get_settings(router: axum::Router) -> (StatusCode, serde_json::Value) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// GET /api/v1/settings → {probe, summary, fields, config} with every secret
/// masked (`handleGet + finding K-09: the legacy root
/// `gemini_keys`, log-ingestion API key, SMTP password and DB password are
/// masked too — leaked them).
#[tokio::test]
async fn get_settings_returns_masked_config() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let cfg = cfg_with_secrets(&path.display().to_string());
    let lock = Arc::new(RwLock::new(cfg));
    let router = settings_api_router(lock, None);
    let (status, body) = get_settings(router).await;
    assert_eq!(status, StatusCode::OK);

    for key in ["probe", "summary", "fields", "config"] {
        assert!(body.get(key).is_some(), "missing {key}");
    }
    assert!(!body["summary"].as_str().unwrap().is_empty());
    assert!(!body["fields"].as_array().unwrap().is_empty());
    assert!(body["probe"]["tier"].is_string());

    let config = &body["config"];
    assert_eq!(
        config["ai"]["reasoning"]["clouds"]["gemini"]["keys"][0]["key"], "…-key",
        "raw-secret-key last 4 = -key"
    );
    assert_eq!(
        config["ai"]["reasoning"]["clouds"]["claude"]["api_key"],
        "…7890"
    );
    assert_eq!(
        config["ai"]["reasoning"]["clouds"]["openai_compat"]["api_key"],
        "…efgh"
    );
    // Legacy reasoning keys are nulled out entirely ( view.Keys = nil).
    assert!(
        config["ai"]["reasoning"].get("keys").is_none(),
        "masked reasoning keys must be omitted"
    );
    // K-09 additions.
    assert_eq!(config["gemini_keys"][0]["key"], "…-key");
    assert_eq!(config["log_ingestion"]["http"]["api_key"], "…-key");
    assert_eq!(config["smtp"]["password"], "…word");
    assert_eq!(config["db_password"], "…word");
    // Non-secrets stay readable for the SPA.
    assert_eq!(config["db_user"], "root");
    assert_eq!(config["port"], 3030);
    // Bookkeeping fields are not serialized ( json:"-").
    assert!(config.get("config_path").is_none());
    assert!(config.get("config_loaded").is_none());
}

/// POST /api/v1/settings applies choices, writes config.json and returns
/// {ok, path, diff}.
#[tokio::test]
async fn post_settings_writes_config_and_updates_live_state() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested").join("config.json");
    let cfg = cfg_with_secrets(&path.display().to_string());
    let lock = Arc::new(RwLock::new(cfg));
    let router = settings_api_router(lock.clone(), None);

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/settings")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"ai_mode":"ollama","local_model":"auto","use_cloud_when_no_gpu":false,"memory_git_store_personal_claims":"stub"}"#,
        ))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["path"], path.display().to_string());
    assert_eq!(body["diff"][0]["path"], "ai.reasoning");
    assert_eq!(body["diff"][0]["from"], "(previous)");
    assert_eq!(body["diff"][0]["to"], "(updated)");

    // The nested parent dir was created and the file written ( MkdirAll).
    let written = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(parsed["ai"]["reasoning"]["provider"], "ollama");
    assert_eq!(parsed["ai"]["reasoning"]["model"], "auto");
    assert_eq!(parsed["memory"]["git_store"]["personal_claims"], "stub");
    assert_eq!(
        parsed["ai"]["reasoning"]["routing"]["cloud_provider"],
        "auto"
    );
    // Indented two spaces ( MarshalIndent).
    assert!(written.contains("\n  \""));

    // Live shared config updated.
    let live = lock.read().unwrap();
    assert_eq!(live.ai.reasoning.provider, "ollama");
    assert_eq!(live.memory.git_store.personal_claims, "stub");
    assert_eq!(live.config_path, path.display().to_string());
}

/// The claims-mode enum validation (JSON 400).
#[tokio::test]
async fn post_settings_rejects_invalid_claims_mode() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let lock = Arc::new(RwLock::new(cfg_with_secrets(&path.display().to_string())));
    let router = settings_api_router(lock, None);
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/settings")
        .body(Body::from(
            r#"{"memory_git_store_personal_claims":"bogus"}"#,
        ))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["error"],
        "memory_git_store_personal_claims must be db_only, stub, or in_repo"
    );
}

/// Malformed JSON → the plain-text 400 (`http.Error(w, err.Error, 400)`).
#[tokio::test]
async fn post_settings_bad_json_is_plain_text_400() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let lock = Arc::new(RwLock::new(cfg_with_secrets(&path.display().to_string())));
    let router = settings_api_router(lock, None);
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/settings")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(body.contains('\n'));
    assert!(ctype.starts_with("text/plain"), "ctype={ctype}");
}

/// Other methods → 405 `method not allowed`
#[tokio::test]
async fn settings_root_rejects_other_methods() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let lock = Arc::new(RwLock::new(cfg_with_secrets(&path.display().to_string())));
    let router = settings_api_router(lock, None);
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/settings")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes), "method not allowed\n");
}

/// The apply body is size-capped (K-06: read it unbounded) — a hostile
/// oversized body answers with the plain-text 413 protocol.
#[tokio::test]
async fn post_settings_rejects_oversized_body() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.json");
    let lock = Arc::new(RwLock::new(cfg_with_secrets(&path.display().to_string())));
    let router = settings_api_router(lock, None);
    let oversized = vec![b'a'; threski_settings::MAX_APPLY_BODY_BYTES + 1];
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/settings")
        .body(Body::from(oversized))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains(&format!(
            "Request body exceeds {} byte limit",
            threski_settings::MAX_APPLY_BODY_BYTES
        )),
        "body={body}"
    );
}
