//! Redmine ticketing provider.
//!
//! Reuses the legacy [`threski_ingest_redmine::Client`] for search/get/update/users
//! (the original provider did the same through `internal/ingest/redmine`) and its own
//! raw HTTP calls for create/list-statuses/list-projects. The API-key
//! precedence is per-request auth → configured key header contract,
//! E-A13).

use async_trait::async_trait;
use serde_json::json;

use threski_ingest_redmine::{Issue, SearchIssuesParams, SearchIssuesResult, UpdateIssueParams};

use crate::config::RedmineConfig;
use crate::providers::net;
use crate::types::{
    AuthContext, CreateParams, Project, ProviderName, SearchParams, SearchResult, Status, Ticket,
    TicketProvider, TicketingError, UpdateParams, User, PROVIDER_REDMINE,
};

/// Redmine backend.
pub struct Provider {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    legacy: threski_ingest_redmine::Client,
}

impl Provider {
    /// Builds the provider; trailing slashes are trimmed
    /// from the base URL.
    pub fn new(cfg: &RedmineConfig) -> Self {
        let legacy = threski_ingest_redmine::Client::new(&cfg.base_url, &cfg.api_key);
        Provider {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            http: net::http_client(),
            legacy,
        }
    }

    /// Per-request key wins over the configured key.
    fn effective_api_key(&self, auth: &AuthContext) -> String {
        let key = auth.redmine_api_key.trim();
        if key.is_empty() {
            self.api_key.trim().to_string()
        } else {
            key.to_string()
        }
    }

    /// Shared raw-call helper: always sends `X-Redmine-API-Key`,
    /// `Content-Type` only when a body is present.
    async fn request(
        &self,
        auth: &AuthContext,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, TicketingError> {
        let mut request = self.http.request(method, endpoint);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .header("X-Redmine-API-Key", self.effective_api_key(auth))
            .send()
            .await
            .map_err(TicketingError::Request)?;
        Ok(response)
    }

    /// Reads the body and maps a non-2xx status to the original message shape.
    async fn api_error(response: reqwest::Response, op: &'static str) -> TicketingError {
        let status = response.status().as_u16();
        let body = match response.bytes().await {
            Ok(bytes) => net::error_body(&bytes),
            Err(e) => return TicketingError::Request(e),
        };
        TicketingError::Api {
            provider: PROVIDER_REDMINE,
            op,
            status,
            body,
        }
    }

    async fn create_via_raw_call(
        &self,
        auth: &AuthContext,
        params: &CreateParams,
    ) -> Result<Ticket, TicketingError> {
        let mut issue = serde_json::Map::new();
        issue.insert("subject".into(), json!(params.title));
        issue.insert("description".into(), json!(params.description));
        if !params.project_key.is_empty() {
            issue.insert("project_id".into(), json!(params.project_key));
        }
        // Numeric strings become ids ( strconv.Atoi attempts).
        if !params.issue_type.is_empty() {
            issue.insert("tracker_id".into(), int_or_string(&params.issue_type));
        }
        if !params.assignee.is_empty() {
            if let Some(n) = parse_int(&params.assignee) {
                issue.insert("assigned_to_id".into(), json!(n));
            }
        }
        if !params.priority.is_empty() {
            if let Some(n) = parse_int(&params.priority) {
                issue.insert("priority_id".into(), json!(n));
            }
        }
        for (k, v) in params.provider_fields.iter().flatten() {
            issue.insert(k.clone(), v.clone());
        }

        let endpoint = format!("{}/issues.json", self.base_url);
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(json!({ "issue": issue })),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "create").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            issue: CreatedIssue,
        }
        #[derive(serde::Deserialize, Default)]
        struct CreatedIssue {
            #[serde(default)]
            id: i64,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        if out.issue.id == 0 {
            // Redmine answered without an id (parity fallback ticket).
            return Ok(Ticket {
                provider: ProviderName::from(PROVIDER_REDMINE),
                title: params.title.clone(),
                project_key: params.project_key.clone(),
                ..Ticket::default()
            });
        }
        self.get_issue(auth, &out.issue.id.to_string(), &params.project_key)
            .await?
            .ok_or_else(|| TicketingError::Other("created issue disappeared".to_string()))
    }

    async fn list_statuses_via_raw_call(
        &self,
        auth: &AuthContext,
    ) -> Result<Vec<Status>, TicketingError> {
        let endpoint = format!("{}/issue_statuses.json", self.base_url);
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list statuses").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            issue_statuses: Vec<NamedId>,
        }
        #[derive(serde::Deserialize)]
        struct NamedId {
            #[serde(default)]
            id: i64,
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .issue_statuses
            .into_iter()
            .map(|s| Status {
                id: s.id.to_string(),
                name: s.name,
            })
            .collect())
    }

    async fn list_projects_via_raw_call(
        &self,
        auth: &AuthContext,
    ) -> Result<Vec<Project>, TicketingError> {
        let endpoint = format!("{}/projects.json?limit=100", self.base_url);
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list projects").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            projects: Vec<RawProject>,
        }
        #[derive(serde::Deserialize)]
        struct RawProject {
            #[serde(default)]
            id: i64,
            #[serde(default)]
            identifier: String,
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .projects
            .into_iter()
            .map(|p| Project {
                id: p.id.to_string(),
                key: p.identifier,
                name: p.name,
            })
            .collect())
    }
}

/// Numeric strings bind as Redmine ids, anything else stays a string
/// (`strconv.Atoi attempt).
fn int_or_string(value: &str) -> serde_json::Value {
    match parse_int(value) {
        Some(n) => json!(n),
        None => json!(value),
    }
}

fn parse_int(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

fn map_issue(issue: &Issue) -> Ticket {
    let external_id = issue.id.to_string();
    Ticket {
        provider: ProviderName::from(PROVIDER_REDMINE),
        id: external_id.clone(),
        external_id: external_id.clone(),
        external_key: external_id,
        title: issue.subject.clone(),
        description: issue.description.clone(),
        status: issue.status.name.clone(),
        issue_type: issue.tracker.name.clone(),
        author: issue.author.name.clone(),
        created_on: issue.created_on.clone(),
        updated_on: issue.updated_on.clone(),
        ..Ticket::default()
    }
}

fn map_search(result: SearchIssuesResult) -> SearchResult {
    SearchResult {
        issues: result.issues.iter().map(map_issue).collect(),
        total_count: result.total_count,
        offset: result.offset,
        limit: result.limit,
    }
}

#[async_trait]
impl TicketProvider for Provider {
    fn name(&self) -> ProviderName {
        ProviderName::from(PROVIDER_REDMINE)
    }

    async fn search_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        let mut legacy_params = SearchIssuesParams {
            query: params.query,
            project_id: params.project_key,
            status_id: params.status,
            tracker_id: params.issue_type,
            assigned_to_id: params.assignee,
            author_id: params.author,
            priority_id: params.priority,
            updated_from: params.updated_from,
            updated_to: params.updated_to,
            limit: params.limit,
            offset: params.offset,
            sort: params.sort,
        };
        if legacy_params.limit == 0 {
            legacy_params.limit = 20;
        }
        if legacy_params.sort.is_empty() {
            legacy_params.sort = "updated_on:desc".to_string();
        }
        let key = self.effective_api_key(auth);
        let result = self
            .legacy
            .search_issues_advanced(Some(&key), legacy_params)
            .await?;
        Ok(map_search(result))
    }

    async fn search_my_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        let mut legacy_params = SearchIssuesParams {
            query: params.query,
            project_id: params.project_key,
            status_id: params.status,
            tracker_id: params.issue_type,
            priority_id: params.priority,
            updated_from: params.updated_from,
            updated_to: params.updated_to,
            limit: params.limit,
            offset: params.offset,
            sort: params.sort,
            ..SearchIssuesParams::default()
        };
        if legacy_params.limit == 0 {
            legacy_params.limit = 20;
        }
        if legacy_params.sort.is_empty() {
            legacy_params.sort = "updated_on:desc".to_string();
        }
        let key = self.effective_api_key(auth);
        let result = self
            .legacy
            .search_my_issues(Some(&key), legacy_params)
            .await?;
        Ok(map_search(result))
    }

    async fn get_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
    ) -> Result<Option<Ticket>, TicketingError> {
        let key = self.effective_api_key(auth);
        let Some(issue) = self.legacy.get_issue(Some(&key), id).await? else {
            return Ok(None);
        };
        let mut t = map_issue(&issue);
        t.project_key = project_key.to_string();
        Ok(Some(t))
    }

    async fn create_issue(
        &self,
        auth: &AuthContext,
        params: CreateParams,
    ) -> Result<Ticket, TicketingError> {
        if params.title.trim().is_empty() {
            return Err(TicketingError::TitleRequired);
        }
        self.create_via_raw_call(auth, &params).await
    }

    async fn update_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        params: UpdateParams,
    ) -> Result<Ticket, TicketingError> {
        let mut update = UpdateIssueParams {
            notes: params.notes,
            ..UpdateIssueParams::default()
        };
        if !params.status.is_empty() {
            if let Some(n) = parse_int(&params.status) {
                update.status_id = n;
            }
        }
        if !params.priority.is_empty() {
            if let Some(n) = parse_int(&params.priority) {
                update.priority_id = n;
            }
        }
        if !params.assignee.is_empty() {
            if let Some(n) = parse_int(&params.assignee) {
                update.assigned_to_id = n;
            }
        }
        if !params.fixed_version.is_empty() {
            if let Some(n) = parse_int(&params.fixed_version) {
                update.fixed_version_id = n;
            }
        }
        let key = self.effective_api_key(auth);
        self.legacy.update_issue(Some(&key), id, update).await?;
        self.get_issue(auth, id, project_key)
            .await?
            .ok_or_else(|| TicketingError::Other(format!("issue {id} vanished after update")))
    }

    async fn add_comment(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        comment: &str,
    ) -> Result<(), TicketingError> {
        self.update_issue(
            auth,
            id,
            project_key,
            UpdateParams {
                notes: comment.to_string(),
                ..UpdateParams::default()
            },
        )
        .await?;
        Ok(())
    }

    async fn assign_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        assignee: &str,
    ) -> Result<(), TicketingError> {
        self.update_issue(
            auth,
            id,
            project_key,
            UpdateParams {
                assignee: assignee.to_string(),
                ..UpdateParams::default()
            },
        )
        .await?;
        Ok(())
    }

    async fn transition_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        transition: &str,
    ) -> Result<(), TicketingError> {
        self.update_issue(
            auth,
            id,
            project_key,
            UpdateParams {
                status: transition.to_string(),
                ..UpdateParams::default()
            },
        )
        .await?;
        Ok(())
    }

    async fn list_statuses(
        &self,
        auth: &AuthContext,
        _project_key: &str,
        _issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError> {
        self.list_statuses_via_raw_call(auth).await
    }

    async fn search_users(
        &self,
        auth: &AuthContext,
        query: &str,
        limit: i64,
    ) -> Result<Vec<User>, TicketingError> {
        let key = self.effective_api_key(auth);
        let res = self.legacy.get_users(Some(&key), query, limit).await?;
        Ok(res
            .users
            .iter()
            .map(|u| User {
                id: u.id.to_string(),
                name: format!("{} {}", u.firstname, u.lastname).trim().to_string(),
                email: u.mail.clone(),
            })
            .collect())
    }

    async fn list_projects(&self, auth: &AuthContext) -> Result<Vec<Project>, TicketingError> {
        self.list_projects_via_raw_call(auth).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::routing::{get, post};
    use std::sync::{Arc, Mutex};

    /// Captures request headers/body/query for post-call assertions (axum
    /// handler panics would not propagate through the spawned server task).
    #[derive(Default, Clone)]
    struct Capture {
        key: Arc<Mutex<String>>,
        path: Arc<Mutex<String>>,
        query: Arc<Mutex<String>>,
        body: Arc<Mutex<String>>,
        method: Arc<Mutex<String>>,
        content_type: Arc<Mutex<String>>,
    }

    fn record(
        capture: &Capture,
        method: &str,
        path: &str,
        query: &str,
        headers: &axum::http::HeaderMap,
        body: &str,
    ) {
        *capture.method.lock().unwrap() = method.to_string();
        *capture.path.lock().unwrap() = path.to_string();
        *capture.query.lock().unwrap() = query.to_string();
        *capture.key.lock().unwrap() = headers
            .get("X-Redmine-API-Key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *capture.content_type.lock().unwrap() = headers
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *capture.body.lock().unwrap() = body.to_string();
    }

    fn provider(base: &str, key: &str) -> Provider {
        Provider::new(&crate::config::RedmineConfig {
            base_url: base.to_string(),
            api_key: key.to_string(),
        })
    }

    fn empty_auth() -> AuthContext {
        AuthContext::default()
    }

    const SAMPLE_ISSUE: &str = r#"{"issue":{"id":42,"subject":"Fix startup","status":{"id":1,"name":"New"},"tracker":{"id":2,"name":"Bug"},"author":{"id":3,"name":"Dev"},"created_on":"2026-01-01T00:00:00Z","updated_on":"2026-01-02T00:00:00Z"}}"#;

    // Scenario: TestProviderGetIssueUsesHeaderOverride.
    #[tokio::test]
    async fn get_issue_uses_header_override() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/issues/42.json",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    *state.key.lock().unwrap() = headers
                        .get("X-Redmine-API-Key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    (axum::http::StatusCode::OK, SAMPLE_ISSUE)
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base, "system-key");
        let issue = p
            .get_issue(
                &AuthContext {
                    redmine_api_key: "user-key".into(),
                    ..AuthContext::default()
                },
                "42",
                "core",
            )
            .await
            .unwrap()
            .expect("issue");
        assert_eq!(issue.external_id, "42");
        assert_eq!(capture.key.lock().unwrap().as_str(), "user-key");
        assert_eq!(issue.project_key, "core");
        assert_eq!(issue.issue_type, "Bug");
        assert_eq!(issue.status, "New");
    }

    // Scenario: TestProviderSearchIssues.
    #[tokio::test]
    async fn search_issues_maps_result() {
        let app = axum::Router::new().route(
            "/issues.json",
            get(|| async {
                axum::Json(serde_json::json!({
                    "total_count": 1,
                    "offset": 0,
                    "limit": 20,
                    "issues": [
                        {"id": 12, "subject": "Timeout",
                         "status": {"name": "In Progress"},
                         "tracker": {"name": "Bug"},
                         "author": {"name": "Dev"}}
                    ]
                }))
            }),
        );
        let base = serve(app).await;

        let p = provider(&base, "system-key");
        let result = p
            .search_issues(
                &empty_auth(),
                SearchParams {
                    query: "timeout".into(),
                    ..SearchParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.total_count, 1);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].external_id, "12");
        assert_eq!(result.issues[0].title, "Timeout");
        assert_eq!(result.issues[0].provider.as_str(), "redmine");
    }

    #[tokio::test]
    async fn search_my_issues_forces_me_assignment() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/issues.json",
            get(
                move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let state = state.clone();
                    async move {
                        *state.query.lock().unwrap() = format!("{query:?}");
                        axum::Json(serde_json::json!({"total_count": 0, "issues": []}))
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        p.search_my_issues(&empty_auth(), SearchParams::default())
            .await
            .unwrap();
        let q = capture.query.lock().unwrap();
        assert!(q.contains(r#""assigned_to_id": "me""#), "query: {q}");
        assert!(q.contains(r#""sort": "updated_on:desc""#), "query: {q}");
        assert!(q.contains(r#""limit": "20""#), "query: {q}");
    }

    #[tokio::test]
    async fn get_issue_404_is_none() {
        let app = axum::Router::new().route(
            "/issues/404.json",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "missing") }),
        );
        let base = serve(app).await;
        let p = provider(&base, "key");
        assert!(p
            .get_issue(&empty_auth(), "404", "core")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn create_requires_title() {
        let p = provider("http://example.com", "key");
        let err = p
            .create_issue(
                &empty_auth(),
                CreateParams {
                    title: "   ".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .expect_err("empty title must fail");
        assert_eq!(err.to_string(), "title is required");
    }

    #[tokio::test]
    async fn create_posts_numeric_ids_and_provider_fields() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new()
            .route(
                "/issues.json",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        record(&state, "POST", "/issues.json", "", &headers, &body);
                        (axum::http::StatusCode::CREATED, r#"{"issue":{"id":99}}"#)
                    }
                }),
            )
            .route(
                "/issues/99.json",
                get(|| async { (axum::http::StatusCode::OK, SAMPLE_ISSUE) }),
            );
        let base = serve(app).await;

        let p = provider(&base, "key");
        let mut fields = serde_json::Map::new();
        fields.insert("custom_string".into(), serde_json::json!("x"));
        let t = p
            .create_issue(
                &empty_auth(),
                CreateParams {
                    project_key: "core".into(),
                    title: "New bug".into(),
                    description: "Desc".into(),
                    issue_type: "2".into(),
                    assignee: "7".into(),
                    priority: "3".into(),
                    provider_fields: Some(fields),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(t.external_id, "42", "follow-up get returns the issue");
        let cap = capture.clone();
        assert_eq!(cap.method.lock().unwrap().as_str(), "POST");
        assert!(cap
            .content_type
            .lock()
            .unwrap()
            .starts_with("application/json"));
        let body = cap.body.lock().unwrap().clone();
        assert!(body.contains(r#""subject":"New bug""#), "{body}");
        assert!(body.contains(r#""project_id":"core""#), "{body}");
        assert!(body.contains(r#""tracker_id":2"#), "{body}");
        assert!(body.contains(r#""assigned_to_id":7"#), "{body}");
        assert!(body.contains(r#""priority_id":3"#), "{body}");
        assert!(body.contains(r#""custom_string":"x""#), "{body}");
    }

    // Numeric-string fields fall back to strings ( strconv.Atoi attempt).
    #[tokio::test]
    async fn create_non_numeric_tracker_stays_string() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new()
            .route(
                "/issues.json",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        record(&state, "POST", "/issues.json", "", &headers, &body);
                        (axum::http::StatusCode::CREATED, r#"{"issue":{"id":0}}"#)
                    }
                }),
            )
            .route(
                "/issues/0.json",
                get(|| async { (axum::http::StatusCode::OK, SAMPLE_ISSUE) }),
            );
        let base = serve(app).await;

        let p = provider(&base, "key");
        // Non-numeric assignee is silently dropped (parity).
        let t = p
            .create_issue(
                &empty_auth(),
                CreateParams {
                    title: "t".into(),
                    issue_type: "Feature".into(),
                    assignee: "not-a-number".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        // id == 0 → bare fallback ticket (parity).
        assert_eq!(t.title, "t");
        let body = capture.body.lock().unwrap().clone();
        assert!(body.contains(r#""tracker_id":"Feature""#), "{body}");
        assert!(!body.contains("assigned_to_id"), "{body}");
    }

    #[tokio::test]
    async fn create_error_includes_body() {
        let app = axum::Router::new().route(
            "/issues.json",
            post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
        let base = serve(app).await;
        let p = provider(&base, "key");
        let err = p
            .create_issue(
                &empty_auth(),
                CreateParams {
                    title: "t".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .expect_err("500 must fail");
        assert_eq!(err.to_string(), "redmine create error 500: boom");
    }

    #[tokio::test]
    async fn update_and_followup_get_roundtrip() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/issues/5.json",
            axum::routing::any(
                move |method: axum::http::Method, headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        if method == axum::http::Method::PUT {
                            record(&state, "PUT", "/issues/5.json", "", &headers, &body);
                            return (axum::http::StatusCode::NO_CONTENT, String::new());
                        }
                        // GET follow-up: do not overwrite the captured PUT body.
                        (axum::http::StatusCode::OK, SAMPLE_ISSUE.to_string())
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        let t = p
            .update_issue(
                &empty_auth(),
                "5",
                "core",
                UpdateParams {
                    notes: "note".into(),
                    status: "2".into(),
                    priority: "3".into(),
                    assignee: "7".into(),
                    fixed_version: "8".into(),
                    ..UpdateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(t.external_id, "42");

        let body = capture.body.lock().unwrap().clone();
        for needle in [
            r#""notes":"note""#,
            r#""status_id":2"#,
            r#""priority_id":3"#,
            r#""assigned_to_id":7"#,
            r#""fixed_version_id":8"#,
        ] {
            assert!(body.contains(needle), "payload missing {needle}: {body}");
        }
    }

    #[tokio::test]
    async fn add_comment_and_transition_wrap_update() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/issues/5.json",
            axum::routing::any(
                move |method: axum::http::Method, headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        if method == axum::http::Method::PUT {
                            record(&state, "PUT", "/issues/5.json", "", &headers, &body);
                            return (axum::http::StatusCode::NO_CONTENT, String::new());
                        }
                        // GET follow-up: do not overwrite the captured PUT body.
                        (axum::http::StatusCode::OK, SAMPLE_ISSUE.to_string())
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        p.add_comment(&empty_auth(), "5", "core", "a comment")
            .await
            .unwrap();
        assert!(capture
            .body
            .lock()
            .unwrap()
            .contains(r#""notes":"a comment""#));

        p.transition_issue(&empty_auth(), "5", "core", "3")
            .await
            .unwrap();
        assert!(capture.body.lock().unwrap().contains(r#""status_id":3"#));
    }

    #[tokio::test]
    async fn list_statuses_decodes() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/issue_statuses.json",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    record(&state, "GET", "/issue_statuses.json", "", &headers, "");
                    axum::Json(serde_json::json!({
                        "issue_statuses": [
                            {"id": 1, "name": "New"},
                            {"id": 3, "name": "Resolved"}
                        ]
                    }))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        let statuses = p.list_statuses(&empty_auth(), "core", "").await.unwrap();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[1].id, "3");
        assert_eq!(statuses[1].name, "Resolved");
    }

    #[tokio::test]
    async fn list_statuses_error_includes_body() {
        let app = axum::Router::new().route(
            "/issue_statuses.json",
            get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "nope") }),
        );
        let base = serve(app).await;
        let p = provider(&base, "key");
        let err = p
            .list_statuses(&empty_auth(), "core", "")
            .await
            .expect_err("401 must fail");
        assert_eq!(err.to_string(), "redmine list statuses error 401: nope");
    }

    // `ListProjects sends `?limit=100`.
    #[tokio::test]
    async fn list_projects_sends_limit_and_decodes() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/projects.json",
            get(
                move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let state = state.clone();
                    async move {
                        *state.query.lock().unwrap() = format!("{query:?}");
                        axum::Json(serde_json::json!({
                            "projects": [
                                {"id": 1, "identifier": "core", "name": "Core"},
                                {"id": 2, "identifier": "mobile", "name": "Mobile"}
                            ]
                        }))
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        let projects = p.list_projects(&empty_auth()).await.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "1");
        assert_eq!(projects[0].key, "core");
        assert_eq!(projects[1].name, "Mobile");
        assert!(
            capture.query.lock().unwrap().contains(r#""limit": "100""#),
            "limit=100 expected: {:?}",
            capture.query.lock().unwrap()
        );
    }

    // Legacy-client users endpoint; name = "first last" trimmed.
    #[tokio::test]
    async fn search_users_maps_names() {
        let app = axum::Router::new().route(
            "/users.json",
            get(|| async {
                axum::Json(serde_json::json!({
                    "users": [
                        {"id": 7, "firstname": "John", "lastname": "Doe", "mail": "j@x.co"},
                        {"id": 8, "firstname": "", "lastname": "", "mail": ""}
                    ]
                }))
            }),
        );
        let base = serve(app).await;

        let p = provider(&base, "key");
        let users = p.search_users(&empty_auth(), "john", 10).await.unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].id, "7");
        assert_eq!(users[0].name, "John Doe");
        assert_eq!(users[0].email, "j@x.co");
        assert_eq!(users[1].name, "");
    }

    // The per-request key wins over the client key for the raw calls too
    // (`do always used effectiveAPIKey).
    #[tokio::test]
    async fn raw_calls_use_effective_api_key() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/projects.json",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    *state.key.lock().unwrap() = headers
                        .get("X-Redmine-API-Key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    axum::Json(serde_json::json!({"projects": []}))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base, "system-key");
        p.list_projects(&AuthContext {
            redmine_api_key: "user-key".into(),
            ..AuthContext::default()
        })
        .await
        .unwrap();
        assert_eq!(capture.key.lock().unwrap().as_str(), "user-key");

        p.list_projects(&empty_auth()).await.unwrap();
        assert_eq!(capture.key.lock().unwrap().as_str(), "system-key");
    }
}
