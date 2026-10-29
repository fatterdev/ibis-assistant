//! The Redmine HTTP client.
//!
//! Precedence for the API key is `override → client config` ( read an
//! `auth.RedmineKeyContextKey` context value; the override is an explicit
//! parameter here — E-A13). Error response bodies are capped at
//! [`crate::error::MAX_ERROR_BODY`] (E-A11).

use std::time::Duration;

use serde_json::json;

use crate::error::{error_body, RedmineError};
use crate::types::{
    Issue, IssuesResponse, SearchIssuesParams, SearchIssuesResult, UpdateIssueParams, UsersResponse,
};

/// `http.Client{Timeout: 30 * time.Second}.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Client {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    timeout: Duration,
}

const JSON_KEY_HEADER: &str = "X-Redmine-API-Key";

impl Client {
    pub fn new(url: &str, key: &str) -> Self {
        Client {
            base_url: url.trim_end_matches('/').to_string(),
            api_key: key.to_string(),
            http: reqwest::Client::builder()
                .timeout(DEFAULT_HTTP_TIMEOUT)
                .build()
                .expect("reqwest client"),
            timeout: DEFAULT_HTTP_TIMEOUT,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The configured HTTP timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn key(&self, api_key_override: Option<&str>) -> String {
        api_key_override
            .filter(|k| !k.is_empty())
            .unwrap_or(&self.api_key)
            .to_string()
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        api_key_override: Option<&str>,
    ) -> Result<reqwest::Response, RedmineError> {
        request
            .header(JSON_KEY_HEADER, self.key(api_key_override))
            .send()
            .await
            .map_err(RedmineError::Request)
    }

    /// Fetches a raw issue (with journals included); `Ok(None)` = not found
    /// (404), mirroring the original nil return.
    pub async fn get_issue(
        &self,
        api_key_override: Option<&str>,
        id: &str,
    ) -> Result<Option<Issue>, RedmineError> {
        let endpoint = format!("{}/issues/{id}.json?include=journals", self.base_url);
        tracing::debug!("Redmine: GET {endpoint}");
        let request = self
            .http
            .get(&endpoint)
            .header("Content-Type", "application/json");
        let response = self.send(request, api_key_override).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(RedmineError::Request)?;
        if status == 404 {
            return Ok(None);
        }
        if status != 200 {
            return Err(RedmineError::Api {
                status,
                body: error_body(&bytes),
            });
        }
        #[derive(serde::Deserialize)]
        struct SingleIssueResponse {
            #[serde(default)]
            issue: Issue,
        }
        let result: SingleIssueResponse = serde_json::from_slice(&bytes)?;
        Ok(Some(result.issue))
    }

    /// Searches users by name; `limit == 0` defaults to 100.
    pub async fn get_users(
        &self,
        api_key_override: Option<&str>,
        name: &str,
        limit: i64,
    ) -> Result<UsersResponse, RedmineError> {
        let limit = if limit == 0 { 100 } else { limit };
        let mut request = self.http.get(format!("{}/users.json", self.base_url));
        if !name.is_empty() {
            request = request.query(&[("name", name)]);
        }
        request = request.query(&[("limit", limit.to_string())]);
        request = request.header("Content-Type", "application/json");
        tracing::debug!("Redmine: GET users.json");
        let response = self.send(request, api_key_override).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(RedmineError::Request)?;
        if status != 200 {
            return Err(RedmineError::Api {
                status,
                body: error_body(&bytes),
            });
        }
        serde_json::from_slice(&bytes).map_err(Into::into)
    }

    /// Finds issues whose subject contains the query (top 10).
    pub async fn search_issues(
        &self,
        api_key_override: Option<&str>,
        query: &str,
    ) -> Result<Vec<Issue>, RedmineError> {
        let result = self
            .search_issues_advanced(
                api_key_override,
                SearchIssuesParams {
                    query: query.to_string(),
                    limit: 10,
                    ..SearchIssuesParams::default()
                },
            )
            .await?;
        Ok(result.issues)
    }

    /// Forces `assigned_to_id = "me"` plus the default sort/limit
    ///
    pub async fn search_my_issues(
        &self,
        api_key_override: Option<&str>,
        mut params: SearchIssuesParams,
    ) -> Result<SearchIssuesResult, RedmineError> {
        params.assigned_to_id = "me".to_string();
        if params.sort.is_empty() {
            params.sort = "updated_on:desc".to_string();
        }
        if params.limit == 0 {
            params.limit = 20;
        }
        self.search_issues_advanced(api_key_override, params).await
    }

    /// Full search: validates params, defaults the
    /// limit to 20, and encodes the Redmine query operators
    /// (`subject=~q`, `updated_on=>=from`, `updated_on_to=to`).
    pub async fn search_issues_advanced(
        &self,
        api_key_override: Option<&str>,
        mut params: SearchIssuesParams,
    ) -> Result<SearchIssuesResult, RedmineError> {
        crate::types::validate_search_params(&params).map_err(|e| RedmineError::Invalid(e.0))?;

        if params.limit == 0 {
            params.limit = 20;
        }

        let mut pairs: Vec<(String, String)> = Vec::new();
        if !params.query.is_empty() {
            pairs.push(("subject".to_string(), format!("~{}", params.query)));
        }
        if !params.project_id.is_empty() {
            pairs.push(("project_id".to_string(), params.project_id.clone()));
        }
        if !params.status_id.is_empty() {
            pairs.push(("status_id".to_string(), params.status_id.clone()));
        }
        if !params.tracker_id.is_empty() {
            pairs.push(("tracker_id".to_string(), params.tracker_id.clone()));
        }
        if !params.assigned_to_id.is_empty() {
            pairs.push(("assigned_to_id".to_string(), params.assigned_to_id.clone()));
        }
        if !params.author_id.is_empty() {
            pairs.push(("author_id".to_string(), params.author_id.clone()));
        }
        if !params.priority_id.is_empty() {
            pairs.push(("priority_id".to_string(), params.priority_id.clone()));
        }
        if !params.updated_from.is_empty() {
            pairs.push((
                "updated_on".to_string(),
                format!(">={}", params.updated_from),
            ));
        }
        if !params.updated_to.is_empty() {
            pairs.push(("updated_on_to".to_string(), params.updated_to.clone()));
        }
        pairs.push(("limit".to_string(), params.limit.to_string()));
        pairs.push(("offset".to_string(), params.offset.to_string()));
        if !params.sort.is_empty() {
            pairs.push(("sort".to_string(), params.sort.clone()));
        }

        let request = self
            .http
            .get(format!("{}/issues.json", self.base_url))
            .query(&pairs);
        tracing::debug!("Redmine: GET issues.json");
        let response = self.send(request, api_key_override).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(RedmineError::Request)?;
        if status != 200 {
            return Err(RedmineError::SearchApi {
                status,
                body: error_body(&bytes),
            });
        }
        let result: IssuesResponse = serde_json::from_slice(&bytes)?;
        Ok(SearchIssuesResult {
            issues: result.issues,
            total_count: result.total_count,
            offset: result.offset,
            limit: result.limit,
        })
    }

    /// PUTs an issue update; fails before sending when no
    /// field would change.
    pub async fn update_issue(
        &self,
        api_key_override: Option<&str>,
        id: &str,
        params: UpdateIssueParams,
    ) -> Result<(), RedmineError> {
        let endpoint = format!("{}/issues/{id}.json", self.base_url);

        let mut issue_payload = serde_json::Map::new();
        if !params.notes.is_empty() {
            issue_payload.insert("notes".into(), json!(params.notes));
        }
        if params.status_id > 0 {
            issue_payload.insert("status_id".into(), json!(params.status_id));
        }
        if params.priority_id > 0 {
            issue_payload.insert("priority_id".into(), json!(params.priority_id));
        }
        if params.assigned_to_id > 0 {
            issue_payload.insert("assigned_to_id".into(), json!(params.assigned_to_id));
        }
        if params.fixed_version_id > 0 {
            issue_payload.insert("fixed_version_id".into(), json!(params.fixed_version_id));
        }
        if issue_payload.is_empty() {
            return Err(RedmineError::NoUpdateFields);
        }

        let request = self
            .http
            .put(&endpoint)
            .header("Content-Type", "application/json")
            .json(&json!({ "issue": issue_payload }));
        let response = self.send(request, api_key_override).await?;
        let status = response.status().as_u16();
        if status >= 300 {
            let bytes = response.bytes().await.map_err(RedmineError::Request)?;
            return Err(RedmineError::UpdateApi {
                status,
                body: error_body(&bytes),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::extract::Query as AxumQuery;
    use axum::http::HeaderMap;
    use axum::routing::{get, put};
    use std::collections::HashMap;
    use std::sync::{Arc as StdArc, Mutex};

    /// Records the API key of the last request ( TestContextKeyOverride).
    #[derive(Clone, Default)]
    struct KeyProbe {
        seen: StdArc<Mutex<String>>,
    }

    fn probe_key(headers: &HeaderMap, probe: &KeyProbe) {
        let key = headers
            .get("X-Redmine-API-Key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *probe.seen.lock().unwrap() = key;
    }

    // Scenario: TestGetIssue.
    #[tokio::test]
    async fn get_issue_decodes_payload() {
        let app = axum::Router::new().route(
            "/issues/123.json",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers.get("X-Redmine-API-Key").unwrap(),
                    "fake-key",
                    "missing or wrong API key"
                );
                axum::Json(serde_json::json!({
                    "issue": {
                        "id": 123,
                        "subject": "Test Issue",
                        "status": {"id": 1, "name": "New"},
                        "tracker": {"id": 2, "name": "Bug"},
                        "author": {"id": 5, "name": "John Doe"},
                        "description": "Desc",
                        "created_on": "2023-01-01T12:00:00Z",
                        "updated_on": "2023-01-02T12:00:00Z",
                    }
                }))
            }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "fake-key");

        let issue = client.get_issue(None, "123").await.unwrap().expect("issue");
        assert_eq!(issue.id, 123);
        assert_eq!(issue.subject, "Test Issue");
        assert_eq!(issue.author.name, "John Doe");
    }

    #[tokio::test]
    async fn get_issue_404_is_none() {
        let app = axum::Router::new().route(
            "/issues/404.json",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "missing") }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");
        assert!(client.get_issue(None, "404").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_issue_error_body_included() {
        let app = axum::Router::new().route(
            "/issues/1.json",
            get(|| async { (axum::http::StatusCode::FORBIDDEN, "forbidden body") }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");
        let err = client.get_issue(None, "1").await.unwrap_err();
        assert_eq!(err.to_string(), "redmine error 403: forbidden body");
    }

    // Scenario: TestSearchIssues.
    #[tokio::test]
    async fn search_issues_sends_subject_operator() {
        let app = axum::Router::new().route(
            "/issues.json",
            get(
                |AxumQuery(q): AxumQuery<HashMap<String, String>>| async move {
                    assert_eq!(q.get("subject").map(String::as_str), Some("~login"));
                    assert_eq!(q.get("limit").map(String::as_str), Some("10"));
                    assert_eq!(q.get("offset").map(String::as_str), Some("0"));
                    axum::Json(serde_json::json!({
                        "issues": [
                            {"id": 10, "subject": "Login Fix", "status": {"name": "Closed"}},
                            {"id": 11, "subject": "Login UI", "status": {"name": "New"}}
                        ]
                    }))
                },
            ),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");

        let issues = client.search_issues(None, "login").await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].id, 10);
    }

    // Scenario: TestSearchIssuesAdvanced.
    #[tokio::test]
    async fn search_issues_advanced_encodes_all_filters() {
        let app = axum::Router::new().route(
            "/issues.json",
            get(|AxumQuery(q): AxumQuery<HashMap<String, String>>| async move {
                assert_eq!(q.get("subject").map(String::as_str), Some("~timeout"));
                assert_eq!(q.get("project_id").map(String::as_str), Some("core"));
                assert_eq!(q.get("status_id").map(String::as_str), Some("open"));
                assert_eq!(q.get("assigned_to_id").map(String::as_str), Some("me"));
                assert_eq!(q.get("limit").map(String::as_str), Some("25"));
                assert_eq!(q.get("offset").map(String::as_str), Some("50"));
                assert_eq!(q.get("sort").map(String::as_str), Some("updated_on:desc"));
                axum::Json(serde_json::json!({
                    "total_count": 101,
                    "offset": 50,
                    "limit": 25,
                    "issues": [
                        {"id": 123, "subject": "DB timeout on startup", "status": {"name": "In Progress"}}
                    ]
                }))
            }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");

        let res = client
            .search_issues_advanced(
                None,
                SearchIssuesParams {
                    query: "timeout".into(),
                    project_id: "core".into(),
                    status_id: "open".into(),
                    assigned_to_id: "me".into(),
                    limit: 25,
                    offset: 50,
                    sort: "updated_on:desc".into(),
                    ..SearchIssuesParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total_count, 101);
        assert_eq!(res.issues.len(), 1);
        assert_eq!(res.issues[0].id, 123);
    }

    // Scenario: TestSearchMyIssuesForcesAssignedToMe.
    #[tokio::test]
    async fn search_my_issues_forces_assigned_to_me() {
        let app = axum::Router::new().route(
            "/issues.json",
            get(
                |AxumQuery(q): AxumQuery<HashMap<String, String>>| async move {
                    assert_eq!(q.get("assigned_to_id").map(String::as_str), Some("me"));
                    axum::Json(serde_json::json!({"issues": []}))
                },
            ),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");

        client
            .search_my_issues(
                None,
                SearchIssuesParams {
                    assigned_to_id: "42".into(),
                    ..SearchIssuesParams::default()
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn search_issues_advanced_validates_before_request() {
        let app = axum::Router::new().route(
            "/issues.json",
            get(|| async { axum::Json(serde_json::json!({"issues": []})) }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");
        let err = client
            .search_issues_advanced(
                None,
                SearchIssuesParams {
                    sort: "bad:desc".into(),
                    ..SearchIssuesParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "invalid sort 'bad:desc'");
    }

    // Scenario: TestUpdateIssueWithExtendedFields.
    #[tokio::test]
    async fn update_issue_sends_extended_fields() {
        let app = axum::Router::new().route(
            "/issues/77.json",
            put(|body: String| async move {
                for needle in [
                    r#""notes":"working on fix""#,
                    r#""status_id":2"#,
                    r#""priority_id":3"#,
                    r#""assigned_to_id":15"#,
                    r#""fixed_version_id":8"#,
                ] {
                    assert!(body.contains(needle), "payload missing {needle}: {body}");
                }
                axum::http::StatusCode::NO_CONTENT
            }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");

        client
            .update_issue(
                None,
                "77",
                UpdateIssueParams {
                    notes: "working on fix".into(),
                    status_id: 2,
                    priority_id: 3,
                    assigned_to_id: 15,
                    fixed_version_id: 8,
                },
            )
            .await
            .unwrap();
    }

    // Scenario: TestUpdateIssueRequiresAtLeastOneField.
    #[tokio::test]
    async fn update_issue_requires_at_least_one_field() {
        let client = Client::new("http://example.com", "key");
        let err = client
            .update_issue(None, "1", UpdateIssueParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no update fields provided");
    }

    // Scenario: TestContextKeyOverride: the override replaces the client
    // key ( context-value semantics → explicit parameter, E-A13).
    #[tokio::test]
    async fn api_key_override_wins_over_client_key() {
        let probe = KeyProbe::default();
        let state = probe.clone();
        let app = axum::Router::new().route(
            "/issues/999.json",
            get(move |headers: HeaderMap| async move {
                probe_key(&headers, &state);
                (axum::http::StatusCode::OK, r#"{"issue": {"id": 999}}"#)
            }),
        );
        let base = serve(app).await;

        let client = Client::new(&base, "system-key");
        client
            .get_issue(Some("user-secret-key"), "999")
            .await
            .unwrap();
        assert_eq!(probe.seen.lock().unwrap().as_str(), "user-secret-key");

        // Without the override the client config key is used.
        client.get_issue(None, "999").await.unwrap();
        assert_eq!(probe.seen.lock().unwrap().as_str(), "system-key");
    }

    #[tokio::test]
    async fn override_empty_falls_back_to_client_key() {
        let probe = KeyProbe::default();
        let state = probe.clone();
        let app = axum::Router::new().route(
            "/issues/1.json",
            get(move |headers: HeaderMap| async move {
                probe_key(&headers, &state);
                (axum::http::StatusCode::OK, r#"{"issue": {"id": 1}}"#)
            }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "system-key");
        client.get_issue(Some(""), "1").await.unwrap();
        assert_eq!(probe.seen.lock().unwrap().as_str(), "system-key");
    }

    // Scenario: TestClientTimeout + `TestClientURLTrimming`.
    #[test]
    fn client_defaults_match_reference() {
        let client = Client::new("http://example.com/", "key");
        assert_eq!(client.timeout(), DEFAULT_HTTP_TIMEOUT);
        assert_eq!(client.base_url(), "http://example.com");
    }
}
