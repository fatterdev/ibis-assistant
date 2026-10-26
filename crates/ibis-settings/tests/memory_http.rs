//! The memory browser
//! endpoints over the settings API router (fake MemoryService, axum oneshot).

use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use ibis_core::config::default_config;
use ibis_search::memory::{Memory, MemoryListParams, MemoryUpdateParams};
use ibis_search::ActorContext;
use ibis_settings::{settings_api_router, MemoryApiError, MemoryService};
use tower::ServiceExt;

#[derive(Default)]
struct FakeMemoryState {
    list_params: Option<MemoryListParams>,
    shared_id: Option<String>,
    updated: Option<MemoryUpdateParams>,
}

struct FakeMemoryService {
    memories: Vec<Memory>,
    state: Mutex<FakeMemoryState>,
}

impl FakeMemoryService {
    fn new(memories: Vec<Memory>) -> Arc<Self> {
        Arc::new(FakeMemoryService {
            memories,
            state: Mutex::new(FakeMemoryState::default()),
        })
    }
}

#[async_trait::async_trait]
impl MemoryService for FakeMemoryService {
    async fn list_memories(
        &self,
        _actor: &ActorContext,
        params: MemoryListParams,
    ) -> Result<Vec<Memory>, MemoryApiError> {
        self.state.lock().unwrap().list_params = Some(params);
        Ok(self.memories.clone())
    }

    async fn memory_share(
        &self,
        _actor: &ActorContext,
        id: &str,
        _actors: &[String],
    ) -> Result<(), MemoryApiError> {
        self.state.lock().unwrap().shared_id = Some(id.to_string());
        Ok(())
    }

    async fn memory_update(
        &self,
        _actor: &ActorContext,
        params: MemoryUpdateParams,
    ) -> Result<(), MemoryApiError> {
        self.state.lock().unwrap().updated = Some(params);
        Ok(())
    }
}

fn router(memory: Option<Arc<dyn MemoryService>>) -> axum::Router {
    settings_api_router(Arc::new(RwLock::new(default_config())), memory)
}

async fn get(router: axum::Router, uri: &str) -> (StatusCode, String, Vec<(String, String)>) {
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string()))
        .collect();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

/// `TestMemoryListEndpointFiltersAndReturnsMemories.
#[tokio::test]
async fn memory_list_endpoint_filters_and_returns_memories() {
    let service = FakeMemoryService::new(vec![Memory {
        id: "memory:one".into(),
        kind: "rule".into(),
        content: "Keep imports at the top".into(),
        status: "stale".into(),
        visibility: "team".into(),
        ..Default::default()
    }]);
    let router = router(Some(service.clone()));
    let (status, body, _) = get(
        router,
        "/api/v1/settings/memory?kind=rule&status=stale&visibility=team",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(value["available"].as_bool().unwrap());
    assert_eq!(value["memories"].as_array().unwrap().len(), 1);
    assert_eq!(value["memories"][0]["id"], "memory:one");
    assert_eq!(value["memories"][0]["kind"], "rule");
    let params = service.state.lock().unwrap().list_params.clone().unwrap();
    assert_eq!(params.kinds, vec!["rule".to_string()]);
    assert_eq!(params.status, "stale");
    assert_eq!(params.visibility, "team");
}

/// `TestMemoryEndpointDegradesWithoutDatabase.
#[tokio::test]
async fn memory_endpoint_degrades_without_database() {
    let router = router(None);
    let (status, body, _) = get(router, "/api/v1/settings/memory").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""available":false"#), "body={body}");
}

/// `TestMemoryVisibilityEndpointSharesAndMakesPersonal.
#[tokio::test]
async fn memory_visibility_endpoint_shares_and_makes_personal() {
    for (visibility, want_shared) in [("team", true), ("personal", false)] {
        let service = FakeMemoryService::new(vec![]);
        let router = router(Some(service.clone()));
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/settings/memory/share")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"id":"memory:one","visibility":"{visibility}"}}"#
            )))
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        assert_eq!(status, StatusCode::OK, "{visibility}: body={body}");
        if want_shared {
            assert_eq!(
                service.state.lock().unwrap().shared_id.as_deref(),
                Some("memory:one"),
                "expected share call"
            );
        } else {
            let updated = service.state.lock().unwrap().updated.clone().unwrap();
            assert_eq!(
                updated.visibility.as_deref(),
                Some("personal"),
                "expected personal update"
            );
            assert_eq!(updated.id, "memory:one");
        }
    }
}

/// `TestMemoryExportEndpointSetsDownloadHeaders.
#[tokio::test]
async fn memory_export_endpoint_sets_download_headers() {
    let service = FakeMemoryService::new(vec![Memory {
        id: "memory:one".into(),
        ..Default::default()
    }]);
    let router = router(Some(service.clone()));
    let (status, body, headers) =
        get(router, "/api/v1/settings/memory/export?status=current").await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let disposition = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-disposition"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(disposition.contains("ibis-memories.json"));
    // Export returns the bare memories array ( marshals the slice directly).
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(value.is_array());
    assert_eq!(value[0]["id"], "memory:one");
    let params = service.state.lock().unwrap().list_params.clone().unwrap();
    assert_eq!(params.status, "current", "export forwarded filters");
    assert_eq!(params.limit, 1000, "export default limit");
}

/// Database-not-connected errors degrade to available:false instead of 500
/// ( matched the error string; the Rust port is typed).
#[tokio::test]
async fn memory_list_degrades_on_database_not_connected() {
    struct DownService;
    #[async_trait::async_trait]
    impl MemoryService for DownService {
        async fn list_memories(
            &self,
            _actor: &ActorContext,
            _params: MemoryListParams,
        ) -> Result<Vec<Memory>, MemoryApiError> {
            Err(MemoryApiError::DatabaseNotConnected)
        }
        async fn memory_share(
            &self,
            _actor: &ActorContext,
            _id: &str,
            _actors: &[String],
        ) -> Result<(), MemoryApiError> {
            unimplemented!()
        }
        async fn memory_update(
            &self,
            _actor: &ActorContext,
            _params: MemoryUpdateParams,
        ) -> Result<(), MemoryApiError> {
            unimplemented!()
        }
    }
    let router = router(Some(Arc::new(DownService)));
    let (status, body, _) = get(router, "/api/v1/settings/memory").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#""available":false"#));
}

/// Wrong method → the exact `method not allowed` 405 text.
#[tokio::test]
async fn memory_share_rejects_get() {
    let router = router(None);
    let (status, body, headers) = get(router, "/api/v1/settings/memory/share").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body, "method not allowed\n");
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.starts_with("text/plain")),
        "text/plain error body expected"
    );
}
