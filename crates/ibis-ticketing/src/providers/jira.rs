//! Jira ticketing provider.
//!
//! Wire contract: Jira Cloud REST v3 — Basic auth `email:token`
//! (`Authorization: Bearer`-style `Basic` header,, ADF document
//! bodies for descriptions/comments, `POST /rest/api/3/search` for queries.
//!
//! Deltas applied:
//! - **TI-03 (BUG S2)**: `escapeJQL` quoted quote as `\\"` (escaped backslash +
//!   quote terminator) — any query containing `"` broke the JQL; this crate
//!   escapes as `\"` like every JQL client.
//! - **TI-03b (BUG S2)**: an empty filter set produced a doubled
//!   `ORDER BY updated DESC` (invalid JQL → every unfiltered search failed).
//! - **TI-12 (WART S2)**: the original client had no timeout; 30s now.

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use crate::config::JiraConfig;
use crate::providers::net;
use crate::types::{
    AuthContext, CreateParams, Project, ProviderName, SearchParams, SearchResult, Status, Ticket,
    TicketProvider, TicketingError, UpdateParams, User, PROVIDER_JIRA,
};

/// Jira backend.
pub struct Provider {
    base_url: String,
    email: String,
    token: String,
    http: reqwest::Client,
}

impl Provider {
    /// Builds the provider; trims the config values.
    pub fn new(cfg: &JiraConfig) -> Self {
        Provider {
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            email: cfg.email.trim().to_string(),
            token: cfg.api_token.trim().to_string(),
            http: net::http_client(),
        }
    }

    /// Basic token `email:token` with per-request override.
    fn basic_token(&self, auth: &AuthContext) -> String {
        let mut email = auth.jira_email.trim().to_string();
        let mut token = auth.jira_api_token.trim().to_string();
        if email.is_empty() {
            email = self.email.clone();
        }
        if token.is_empty() {
            token = self.token.clone();
        }
        base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
    }

    /// Shared request helper: Basic auth + `Accept: application/json`.
    async fn request(
        &self,
        auth: &AuthContext,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, TicketingError> {
        let mut request = self.http.request(method, endpoint);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .header("Authorization", format!("Basic {}", self.basic_token(auth)))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(TicketingError::Request)?;
        Ok(response)
    }

    async fn api_error(response: reqwest::Response, op: &'static str) -> TicketingError {
        let status = response.status().as_u16();
        let body = match response.bytes().await {
            Ok(bytes) => net::error_body(&bytes),
            Err(e) => return TicketingError::Request(e),
        };
        TicketingError::Api {
            provider: PROVIDER_JIRA,
            op,
            status,
            body,
        }
    }

    async fn list_transitions(
        &self,
        auth: &AuthContext,
        id: &str,
    ) -> Result<Vec<(String, String)>, TicketingError> {
        let endpoint = format!(
            "{}/rest/api/3/issue/{}/transitions",
            self.base_url,
            net::path_escape(id)
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list transitions").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            transitions: Vec<RawTransition>,
        }
        #[derive(serde::Deserialize)]
        struct RawTransition {
            #[serde(default)]
            id: String,
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .transitions
            .into_iter()
            .map(|t| (t.id, t.name))
            .collect())
    }
}

/// A Jira issue in REST v3 shape.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JiraIssue {
    #[serde(default)]
    id: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    fields: JiraIssueFields,
    #[serde(default)]
    self_url: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JiraIssueFields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: Value,
    #[serde(default)]
    status: NamedValue,
    #[serde(rename = "issuetype", default)]
    issue_type: NamedValue,
    #[serde(default)]
    assignee: AssigneeValue,
    #[serde(default)]
    project: ProjectValue,
    #[serde(default)]
    created: String,
    #[serde(default)]
    updated: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct NamedValue {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AssigneeValue {
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ProjectValue {
    #[serde(default)]
    key: String,
}

fn map_issue(issue: &JiraIssue) -> Ticket {
    // ADF documents are re-serialized verbatim ( json.Marshal of the
    // decoded interface{}).
    let description = if issue.fields.description.is_null() {
        String::new()
    } else {
        serde_json::to_string(&issue.fields.description).unwrap_or_default()
    };
    Ticket {
        provider: ProviderName::from(PROVIDER_JIRA),
        id: issue.key.clone(),
        external_id: issue.id.clone(),
        external_key: issue.key.clone(),
        project_key: issue.fields.project.key.clone(),
        title: issue.fields.summary.clone(),
        description,
        status: issue.fields.status.name.clone(),
        issue_type: issue.fields.issue_type.name.clone(),
        assignee: issue.fields.assignee.display_name.clone(),
        url: issue.self_url.clone(),
        created_on: issue.fields.created.clone(),
        updated_on: issue.fields.updated.clone(),
        ..Ticket::default()
    }
}

/// Builds the JQL query string.
fn build_jql(params: &SearchParams, include_current_user: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !params.project_key.is_empty() {
        parts.push(format!("project = \"{}\"", escape_jql(&params.project_key)));
    }
    if !params.query.is_empty() {
        parts.push(format!("text ~ \"{}\"", escape_jql(&params.query)));
    }
    if !params.status.is_empty() {
        parts.push(format!("status = \"{}\"", escape_jql(&params.status)));
    }
    if !params.issue_type.is_empty() {
        parts.push(format!(
            "issuetype = \"{}\"",
            escape_jql(&params.issue_type)
        ));
    }
    if !params.assignee.is_empty() {
        if params.assignee == "current_user" || include_current_user {
            parts.push("assignee = currentUser()".to_string());
        } else {
            parts.push(format!("assignee = \"{}\"", escape_jql(&params.assignee)));
        }
    }
    if !params.updated_from.is_empty() {
        parts.push(format!(
            "updated >= \"{}\"",
            escape_jql(&params.updated_from)
        ));
    }
    if !params.updated_to.is_empty() {
        parts.push(format!("updated <= \"{}\"", escape_jql(&params.updated_to)));
    }

    let mut query = parts.join(" AND ");
    if query.trim().is_empty() {
        // Bare ORDER BY for unfiltered searches (TI-03b: never doubled).
        return "ORDER BY updated DESC".to_string();
    }
    if !params.sort.is_empty() {
        query.push_str(" ORDER BY ");
        query.push_str(&params.sort);
    }
    query
}

/// Escapes double quotes for a JQL string literal (TI-03: `\"`, not `\\"`).
fn escape_jql(value: &str) -> String {
    value.replace('"', "\\\"")
}

/// ADF document wrapper for one text paragraph.
fn to_adf(text: &str) -> Value {
    json!({
        "type": "doc",
        "version": 1,
        "content": [
            {
                "type": "paragraph",
                "content": [{"type": "text", "text": text}]
            }
        ]
    })
}

/// Search-my marker: the assignee marker makes `buildJQL`
/// emit `assignee = currentUser`.
fn with_my_issues(mut params: SearchParams) -> SearchParams {
    params.query = params.query.trim().to_string();
    if params.sort.is_empty() {
        params.sort = "updated DESC".to_string();
    }
    params.assignee = "current_user".to_string();
    params
}

#[async_trait]
impl TicketProvider for Provider {
    fn name(&self) -> ProviderName {
        ProviderName::from(PROVIDER_JIRA)
    }

    async fn search_issues(
        &self,
        auth: &AuthContext,
        mut params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        // Defaults before JQL build (TI-03c: set the sort default after
        // buildJQL, so the `updated DESC` default silently never applied to
        // filtered searches — inconsistent with SearchMyIssues).
        if params.limit == 0 {
            params.limit = 20;
        }
        if params.sort.is_empty() {
            params.sort = "updated DESC".to_string();
        }
        let jql = build_jql(&params, false);
        let body = json!({
            "jql": jql,
            "maxResults": params.limit,
            "startAt": params.offset,
            "fields": [
                "summary", "description", "status", "issuetype",
                "assignee", "project", "created", "updated"
            ],
        });
        let endpoint = format!("{}/rest/api/3/search", self.base_url);
        let response = self
            .request(auth, reqwest::Method::POST, &endpoint, Some(body))
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "search").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            total: i64,
            #[serde(rename = "startAt", default)]
            start_at: i64,
            #[serde(rename = "maxResults", default)]
            max_results: i64,
            #[serde(default)]
            issues: Vec<JiraIssue>,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(SearchResult {
            issues: out.issues.iter().map(map_issue).collect(),
            total_count: out.total,
            offset: out.start_at,
            limit: out.max_results,
        })
    }

    async fn search_my_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        self.search_issues(auth, with_my_issues(params)).await
    }

    async fn get_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
    ) -> Result<Option<Ticket>, TicketingError> {
        let endpoint = format!(
            "{}/rest/api/3/issue/{}",
            self.base_url,
            net::path_escape(id)
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::api_error(response, "get issue").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        let issue: JiraIssue = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        let mut t = map_issue(&issue);
        if t.project_key.is_empty() {
            t.project_key = project_key.to_string();
        }
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
        let issue_type = if params.issue_type.is_empty() {
            "Task".to_string()
        } else {
            params.issue_type.clone()
        };
        let mut fields = serde_json::Map::new();
        fields.insert("project".into(), json!({"key": params.project_key}));
        fields.insert("summary".into(), json!(params.title));
        fields.insert("issuetype".into(), json!({"name": issue_type}));
        if !params.description.is_empty() {
            fields.insert("description".into(), to_adf(&params.description));
        }
        if !params.assignee.is_empty() {
            fields.insert("assignee".into(), json!({"accountId": params.assignee}));
        }
        for (k, v) in params.provider_fields.iter().flatten() {
            fields.insert(k.clone(), v.clone());
        }

        let endpoint = format!("{}/rest/api/3/issue", self.base_url);
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(json!({"fields": fields})),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "create issue").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            id: String,
            #[serde(default)]
            key: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        let key = if out.key.is_empty() { out.id } else { out.key };
        self.get_issue(auth, &key, &params.project_key)
            .await?
            .ok_or_else(|| TicketingError::Other("created issue disappeared".to_string()))
    }

    async fn update_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        params: UpdateParams,
    ) -> Result<Ticket, TicketingError> {
        let mut fields = serde_json::Map::new();
        if !params.status.is_empty() {
            self.transition_issue(auth, id, project_key, &params.status)
                .await?;
        }
        if !params.assignee.is_empty() {
            fields.insert("assignee".into(), json!({"accountId": params.assignee}));
        }
        if !params.issue_type.is_empty() {
            fields.insert("issuetype".into(), json!({"name": params.issue_type}));
        }
        for (k, v) in params.provider_fields.iter().flatten() {
            fields.insert(k.clone(), v.clone());
        }
        if !fields.is_empty() {
            let endpoint = format!(
                "{}/rest/api/3/issue/{}",
                self.base_url,
                net::path_escape(id)
            );
            let response = self
                .request(
                    auth,
                    reqwest::Method::PUT,
                    &endpoint,
                    Some(json!({"fields": fields})),
                )
                .await?;
            if !response.status().is_success() {
                return Err(Self::api_error(response, "update issue").await);
            }
        }
        if !params.notes.is_empty() {
            self.add_comment(auth, id, project_key, &params.notes)
                .await?;
        }
        self.get_issue(auth, id, project_key)
            .await?
            .ok_or_else(|| TicketingError::Other(format!("issue {id} vanished after update")))
    }

    async fn add_comment(
        &self,
        auth: &AuthContext,
        id: &str,
        _project_key: &str,
        comment: &str,
    ) -> Result<(), TicketingError> {
        let endpoint = format!(
            "{}/rest/api/3/issue/{}/comment",
            self.base_url,
            net::path_escape(id)
        );
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(json!({"body": to_adf(comment)})),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "add comment").await);
        }
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
        _project_key: &str,
        transition: &str,
    ) -> Result<(), TicketingError> {
        let mut transition_id = transition.trim().to_string();
        if transition_id.parse::<i64>().is_err() {
            let wanted = transition.trim().to_lowercase();
            let list = self.list_transitions(auth, id).await?;
            transition_id = list
                .into_iter()
                .find(|(_, name)| name.trim().to_lowercase() == wanted)
                .map(|(id, _)| id)
                .ok_or_else(|| {
                    TicketingError::Invalid(format!("jira transition '{transition}' not found"))
                })?;
        }
        let endpoint = format!(
            "{}/rest/api/3/issue/{}/transitions",
            self.base_url,
            net::path_escape(id)
        );
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(json!({"transition": {"id": transition_id}})),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "transition").await);
        }
        Ok(())
    }

    async fn list_statuses(
        &self,
        auth: &AuthContext,
        _project_key: &str,
        _issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError> {
        let endpoint = format!("{}/rest/api/3/status", self.base_url);
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list statuses").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct RawStatus {
            #[serde(default)]
            id: String,
            #[serde(default)]
            name: String,
        }
        let out: Vec<RawStatus> = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .into_iter()
            .map(|s| Status {
                id: s.id,
                name: s.name,
            })
            .collect())
    }

    async fn search_users(
        &self,
        auth: &AuthContext,
        query: &str,
        limit: i64,
    ) -> Result<Vec<User>, TicketingError> {
        let limit = if limit <= 0 { 20 } else { limit };
        let endpoint = format!(
            "{}/rest/api/3/user/search?query={}&maxResults={}",
            self.base_url,
            net::path_escape(query),
            limit
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None)
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "search users").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct RawUser {
            #[serde(rename = "accountId", default)]
            account_id: String,
            #[serde(rename = "displayName", default)]
            display_name: String,
            #[serde(rename = "emailAddress", default)]
            email: String,
        }
        let out: Vec<RawUser> = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .into_iter()
            .map(|u| User {
                id: u.account_id,
                name: u.display_name,
                email: u.email,
            })
            .collect())
    }

    async fn list_projects(&self, auth: &AuthContext) -> Result<Vec<Project>, TicketingError> {
        let endpoint = format!("{}/rest/api/3/project/search", self.base_url);
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
            values: Vec<RawProject>,
        }
        #[derive(serde::Deserialize)]
        struct RawProject {
            #[serde(default)]
            id: String,
            #[serde(default)]
            key: String,
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .values
            .into_iter()
            .map(|p| Project {
                id: p.id,
                key: p.key,
                name: p.name,
            })
            .collect())
    }
}

#[cfg(test)]
mod jql_tests {
    use super::*;

    // JQL builder matrix ( buildJQL scenarios + TI-03 fixes).
    #[test]
    fn build_jql_matrix() {
        let mut p = SearchParams {
            project_key: "ABC".into(),
            query: "timeout".into(),
            status: "In Progress".into(),
            issue_type: "Bug".into(),
            assignee: "jane".into(),
            updated_from: "2026-01-01".into(),
            updated_to: "2026-02-01".into(),
            sort: "updated DESC".into(),
            ..SearchParams::default()
        };
        let jql = build_jql(&p, false);
        assert_eq!(
            jql,
            "project = \"ABC\" AND text ~ \"timeout\" AND status = \"In Progress\" \
             AND issuetype = \"Bug\" AND assignee = \"jane\" \
             AND updated >= \"2026-01-01\" AND updated <= \"2026-02-01\" ORDER BY updated DESC"
        );

        // current_user marker ( withMyIssues).
        p.assignee = "current_user".into();
        assert!(build_jql(&p, false).contains("assignee = currentUser()"));
        p.assignee = "jane".into();
        assert!(build_jql(&p, true).contains("assignee = currentUser()"));

        // Empty filters → single ORDER BY, never doubled (TI-03b).
        let empty = SearchParams {
            sort: "updated DESC".into(),
            ..SearchParams::default()
        };
        let jql = build_jql(&empty, false);
        assert_eq!(jql.matches("ORDER BY").count(), 1, "{jql}");
        assert_eq!(jql, "ORDER BY updated DESC");

        // Bare params (no sort) → bare ORDER BY default too.
        let bare = SearchParams::default();
        assert_eq!(build_jql(&bare, false), "ORDER BY updated DESC");
    }

    // TI-03: quotes escape as `\"` ( emitted `\\"`).
    #[test]
    fn escape_jql_single_backslash() {
        assert_eq!(escape_jql("say \"hi\""), "say \\\"hi\\\"");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::routing::{get, post};
    use std::sync::{Arc, Mutex};

    /// Captures requests for post-call assertions.
    #[derive(Default, Clone)]
    struct Capture {
        auth: Arc<Mutex<String>>,
        accept: Arc<Mutex<String>>,
        path: Arc<Mutex<String>>,
        query: Arc<Mutex<String>>,
        body: Arc<Mutex<String>>,
        method: Arc<Mutex<String>>,
        content_type: Arc<Mutex<String>>,
        /// `"<method> <path> <body>"` per request, in call order.
        calls: Arc<Mutex<Vec<String>>>,
    }

    fn record(
        capture: &Capture,
        method: &str,
        path: &str,
        headers: &axum::http::HeaderMap,
        body: &str,
    ) {
        *capture.method.lock().unwrap() = method.to_string();
        *capture.path.lock().unwrap() = path.to_string();
        *capture.auth.lock().unwrap() = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *capture.accept.lock().unwrap() = headers
            .get("Accept")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *capture.content_type.lock().unwrap() = headers
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        *capture.body.lock().unwrap() = body.to_string();
        capture
            .calls
            .lock()
            .unwrap()
            .push(format!("{method} {path} {body}"));
    }

    fn provider(base: &str) -> Provider {
        Provider::new(&crate::config::JiraConfig {
            base_url: base.to_string(),
            email: "system@example.com".into(),
            api_token: "system-token".into(),
        })
    }

    fn user_auth() -> AuthContext {
        AuthContext {
            jira_email: "user@example.com".into(),
            jira_api_token: "token".into(),
            ..AuthContext::default()
        }
    }

    fn basic(email: &str, token: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"))
        )
    }

    const SAMPLE_ISSUE: &str = r#"{"id":"100","key":"ABC-100","self":"http://jira/ABC-100","fields":{"summary":"Fix login","description":{"type":"doc"},"status":{"name":"In Progress"},"issuetype":{"name":"Bug"},"project":{"key":"ABC"},"created":"2026-01-01T00:00:00.000+0000","updated":"2026-01-02T00:00:00.000+0000"}}"#;

    // Scenario: TestGetIssueUsesBasicAuth.
    #[tokio::test]
    async fn get_issue_uses_basic_auth() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/issue/ABC-100",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    record(&state, "GET", "/rest/api/3/issue/ABC-100", &headers, "");
                    (axum::http::StatusCode::OK, SAMPLE_ISSUE)
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let issue = p
            .get_issue(&user_auth(), "ABC-100", "ABC")
            .await
            .unwrap()
            .expect("issue");
        assert_eq!(issue.external_key, "ABC-100");
        assert_eq!(
            capture.auth.lock().unwrap().as_str(),
            &basic("user@example.com", "token")
        );
        assert_eq!(capture.accept.lock().unwrap().as_str(), "application/json");
        // Config credentials are the fallback.
        let issue = p
            .get_issue(&AuthContext::default(), "ABC-100", "ABC")
            .await
            .unwrap();
        assert_eq!(issue.unwrap().external_key, "ABC-100");
        assert_eq!(
            capture.auth.lock().unwrap().as_str(),
            &basic("system@example.com", "system-token")
        );
    }

    // Scenario: TestSearchIssues.
    #[tokio::test]
    async fn search_issues_sends_jql_and_maps() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/search",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let state = state.clone();
                async move {
                    record(&state, "POST", "/rest/api/3/search", &headers, &body);
                    axum::Json(serde_json::json!({
                        "total": 1, "startAt": 0, "maxResults": 20,
                        "issues": [{
                            "id": "101", "key": "XYZ-101",
                            "fields": {
                                "summary": "Timeout",
                                "description": {"type": "doc"},
                                "status": {"name": "Open"},
                                "issuetype": {"name": "Task"},
                                "project": {"key": "XYZ"}
                            }
                        }]
                    }))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let result = p
            .search_issues(
                &AuthContext::default(),
                SearchParams {
                    project_key: "XYZ".into(),
                    query: "timeout".into(),
                    ..SearchParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.total_count, 1);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].external_key, "XYZ-101");
        assert_eq!(result.issues[0].provider.as_str(), "jira");

        let body = capture.body.lock().unwrap().clone();
        assert!(body.contains(r#""jql":"#), "{body}");
        assert!(body.contains(r#"project = \"XYZ\""#), "{body}");
        assert!(body.contains(r#"text ~ \"timeout\""#), "{body}");
        // Sort default applies to filtered searches (TI-03c).
        assert!(body.contains("ORDER BY updated DESC"), "{body}");
        assert!(body.contains(r#""maxResults":20"#), "{body}");
        assert!(body.contains(r#""startAt":0"#), "{body}");
        assert!(body.contains(r#""fields":["summary""#), "{body}");
    }

    // Search-my marker routes through currentUser ( withMyIssues).
    #[tokio::test]
    async fn search_my_issues_uses_current_user() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/search",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let state = state.clone();
                async move {
                    record(&state, "POST", "/rest/api/3/search", &headers, &body);
                    axum::Json(serde_json::json!({"total": 0, "issues": []}))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        p.search_my_issues(&AuthContext::default(), SearchParams::default())
            .await
            .unwrap();
        let body = capture.body.lock().unwrap().clone();
        assert!(body.contains("assignee = currentUser()"), "{body}");
        assert!(body.contains("ORDER BY updated DESC"), "{body}");
    }

    #[tokio::test]
    async fn get_issue_404_is_none() {
        let app = axum::Router::new().route(
            "/rest/api/3/issue/ABC-404",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "missing") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        assert!(p
            .get_issue(&AuthContext::default(), "ABC-404", "ABC")
            .await
            .unwrap()
            .is_none());
    }

    // Path segments are percent-encoded (TI-13).
    #[tokio::test]
    async fn get_issue_escapes_path_segment() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/issue/ABC%2F100",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    record(&state, "GET", "/rest/api/3/issue/ABC%2F100", &headers, "");
                    (axum::http::StatusCode::OK, SAMPLE_ISSUE)
                }
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        p.get_issue(&AuthContext::default(), "ABC/100", "ABC")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.path.lock().unwrap().as_str(),
            "/rest/api/3/issue/ABC%2F100"
        );
    }

    #[tokio::test]
    async fn create_requires_title() {
        let p = provider("http://example.com");
        let err = p
            .create_issue(
                &AuthContext::default(),
                CreateParams {
                    title: "".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .expect_err("empty title must fail");
        assert_eq!(err.to_string(), "title is required");
    }

    #[tokio::test]
    async fn create_posts_adf_and_default_task() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new()
            .route(
                "/rest/api/3/issue",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        record(&state, "POST", "/rest/api/3/issue", &headers, &body);
                        (
                            axum::http::StatusCode::CREATED,
                            r#"{"id":"100","key":"ABC-100"}"#,
                        )
                    }
                }),
            )
            .route(
                "/rest/api/3/issue/ABC-100",
                get(|| async { (axum::http::StatusCode::OK, SAMPLE_ISSUE) }),
            );
        let base = serve(app).await;

        let p = provider(&base);
        let t = p
            .create_issue(
                &AuthContext::default(),
                CreateParams {
                    project_key: "ABC".into(),
                    title: "New bug".into(),
                    description: "Long description".into(),
                    assignee: "acct-1".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(t.external_key, "ABC-100");

        let body = capture.body.lock().unwrap().clone();
        assert!(body.contains(r#""project":{"key":"ABC"}"#), "{body}");
        assert!(body.contains(r#""summary":"New bug""#), "{body}");
        // Default type Task (parity).
        assert!(body.contains(r#""issuetype":{"name":"Task"}"#), "{body}");
        // ADF description document.
        assert!(
            body.contains(r#""type":"doc""#) && body.contains(r#""type":"paragraph""#),
            "{body}"
        );
        assert!(body.contains(r#""text":"Long description""#), "{body}");
        assert!(
            body.contains(r#""assignee":{"accountId":"acct-1"}"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn create_error_includes_body() {
        let app = axum::Router::new().route(
            "/rest/api/3/issue",
            post(|| async { (axum::http::StatusCode::BAD_REQUEST, "bad fields") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let err = p
            .create_issue(
                &AuthContext::default(),
                CreateParams {
                    title: "t".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .expect_err("400 must fail");
        assert_eq!(err.to_string(), "jira create issue error 400: bad fields");
    }

    // Transitions: numeric ids skip lookup; names resolve via the
    // transitions endpoint ( listTransitions + EqualFold).
    #[tokio::test]
    async fn transition_by_name_and_numeric_id() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new()
            .route(
                "/rest/api/3/issue/ABC-1/transitions",
                axum::routing::any(move |method: axum::http::Method,
                                         headers: axum::http::HeaderMap,
                                         body: String| {
                    let state = state.clone();
                    async move {
                        if method == axum::http::Method::POST {
                            record(&state, "POST", "/rest/api/3/issue/ABC-1/transitions", &headers, &body);
                            return (axum::http::StatusCode::NO_CONTENT, String::new());
                        }
                        (axum::http::StatusCode::OK, r#"{"transitions":[{"id":"31","name":"Done"},{"id":"41","name":"  ReOpen "}]}"#.to_string())
                    }
                }),
            )
            .route(
                "/rest/api/3/issue/ABC-1",
                get(|| async { (axum::http::StatusCode::OK, SAMPLE_ISSUE) }),
            );
        let base = serve(app).await;

        let p = provider(&base);
        // Numeric → posted verbatim, no lookup.
        p.transition_issue(&AuthContext::default(), "ABC-1", "ABC", " 21 ")
            .await
            .unwrap();
        assert_eq!(
            capture.calls.lock().unwrap().len(),
            1,
            "numeric transition must skip the lookup"
        );
        assert!(capture
            .body
            .lock()
            .unwrap()
            .contains(r#""transition":{"id":"21"}"#));

        // Name (case-insensitive, trimmed) → resolved id.
        p.transition_issue(&AuthContext::default(), "ABC-1", "ABC", "done")
            .await
            .unwrap();
        assert!(capture
            .body
            .lock()
            .unwrap()
            .contains(r#""transition":{"id":"31"}"#));

        p.transition_issue(&AuthContext::default(), "ABC-1", "ABC", "ReOpen")
            .await
            .unwrap();
        assert!(capture
            .body
            .lock()
            .unwrap()
            .contains(r#""transition":{"id":"41"}"#));

        // Unknown name → "jira transition 'x' not found".
        let err = p
            .transition_issue(&AuthContext::default(), "ABC-1", "ABC", "Nope")
            .await
            .expect_err("unknown transition must fail");
        assert_eq!(err.to_string(), "jira transition 'Nope' not found");
    }

    #[tokio::test]
    async fn add_comment_posts_adf() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/issue/ABC-1/comment",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let state = state.clone();
                async move {
                    record(
                        &state,
                        "POST",
                        "/rest/api/3/issue/ABC-1/comment",
                        &headers,
                        &body,
                    );
                    (axum::http::StatusCode::CREATED, r#"{"id":"1"}"#)
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        p.add_comment(&AuthContext::default(), "ABC-1", "ABC", "hello \"world\"")
            .await
            .unwrap();
        let body = capture.body.lock().unwrap().clone();
        assert!(
            body.contains(r#""body":{"type":"doc""#) || body.contains(r#""type":"doc""#),
            "{body}"
        );
        assert!(body.contains(r#""type":"paragraph""#), "{body}");
        assert!(body.contains(r#"hello \"world\""#), "{body}");
    }

    // Update flow ( UpdateIssue): status → transition, fields → PUT,
    // notes → comment, then a final GET.
    #[tokio::test]
    async fn update_flows_transition_fields_comment() {
        let capture = Capture::default();
        let state_put = capture.clone();
        let state_transition = capture.clone();
        let state_comment = capture.clone();
        let app = axum::Router::new()
            .route(
                "/rest/api/3/issue/ABC-1",
                axum::routing::any(
                    move |method: axum::http::Method,
                          headers: axum::http::HeaderMap,
                          body: String| {
                        let state = state_put.clone();
                        async move {
                            if method == axum::http::Method::PUT {
                                record(&state, "PUT", "/rest/api/3/issue/ABC-1", &headers, &body);
                                return (axum::http::StatusCode::NO_CONTENT, String::new());
                            }
                            (axum::http::StatusCode::OK, SAMPLE_ISSUE.to_string())
                        }
                    },
                ),
            )
            .route(
                "/rest/api/3/issue/ABC-1/transitions",
                axum::routing::any(
                    move |method: axum::http::Method,
                          headers: axum::http::HeaderMap,
                          body: String| {
                        let state = state_transition.clone();
                        async move {
                            if method == axum::http::Method::POST {
                                record(
                                    &state,
                                    "POST",
                                    "/rest/api/3/issue/ABC-1/transitions",
                                    &headers,
                                    &body,
                                );
                                return (axum::http::StatusCode::NO_CONTENT, String::new());
                            }
                            (
                                axum::http::StatusCode::OK,
                                r#"{"transitions":[{"id":"31","name":"Done"}]}"#.to_string(),
                            )
                        }
                    },
                ),
            )
            .route(
                "/rest/api/3/issue/ABC-1/comment",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state_comment.clone();
                    async move {
                        record(
                            &state,
                            "POST",
                            "/rest/api/3/issue/ABC-1/comment",
                            &headers,
                            &body,
                        );
                        (axum::http::StatusCode::CREATED, r#"{"id":"9"}"#)
                    }
                }),
            );
        let base = serve(app).await;

        let p = provider(&base);
        let t = p
            .update_issue(
                &AuthContext::default(),
                "ABC-1",
                "ABC",
                UpdateParams {
                    status: "Done".into(),
                    assignee: "acct-2".into(),
                    issue_type: "Story".into(),
                    notes: "update note".into(),
                    ..UpdateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(t.external_key, "ABC-100");

        let calls = capture.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        let put = calls
            .iter()
            .find(|c| c.starts_with("PUT "))
            .expect("PUT fields call");
        assert!(
            put.contains(r#""assignee":{"accountId":"acct-2"}"#),
            "{put}"
        );
        assert!(put.contains(r#""issuetype":{"name":"Story"}"#), "{put}");
        let transition = calls
            .iter()
            .find(|c| c.starts_with("POST /rest/api/3/issue/ABC-1/transitions"))
            .expect("transition call");
        assert!(
            transition.contains(r#""transition":{"id":"31"}"#),
            "{transition}"
        );
        let comment = calls
            .iter()
            .find(|c| c.contains("/comment"))
            .expect("comment call");
        assert!(comment.contains(r#""text":"update note""#), "{comment}");
    }

    #[tokio::test]
    async fn list_statuses_decodes_array() {
        let app = axum::Router::new().route(
            "/rest/api/3/status",
            get(|| async {
                axum::Json(serde_json::json!([
                    {"id": "1", "name": "Open"},
                    {"id": "10001", "name": "Done"}
                ]))
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let statuses = p
            .list_statuses(&AuthContext::default(), "ABC", "")
            .await
            .unwrap();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].id, "1");
        assert_eq!(statuses[1].name, "Done");
    }

    #[tokio::test]
    async fn search_users_sends_query_and_limit() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/rest/api/3/user/search",
            get(
                move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let state = state.clone();
                    async move {
                        *state.query.lock().unwrap() = format!("{query:?}");
                        axum::Json(serde_json::json!([
                            {"accountId": "a1", "displayName": "Jane", "emailAddress": "j@x.co"}
                        ]))
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let users = p
            .search_users(&AuthContext::default(), "jane", 5)
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].id, "a1");
        assert_eq!(users[0].name, "Jane");
        let q = capture.query.lock().unwrap().clone();
        assert!(
            q.contains(r#""query": "jane""#) && q.contains(r#""maxResults": "5""#),
            "{q}"
        );

        // limit <= 0 → default 20 (parity).
        p.search_users(&AuthContext::default(), "j", 0)
            .await
            .unwrap();
        assert!(capture
            .query
            .lock()
            .unwrap()
            .contains(r#""maxResults": "20""#));
    }

    #[tokio::test]
    async fn list_projects_decodes_values() {
        let app = axum::Router::new().route(
            "/rest/api/3/project/search",
            get(|| async {
                axum::Json(serde_json::json!({
                    "values": [
                        {"id": "10000", "key": "ABC", "name": "Alpha"},
                        {"id": "10001", "key": "XYZ", "name": "Xyz"}
                    ]
                }))
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let projects = p.list_projects(&AuthContext::default()).await.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].key, "ABC");
        assert_eq!(projects[1].name, "Xyz");
    }

    // Error bodies surface in the original message shapes.
    #[tokio::test]
    async fn search_error_includes_body() {
        let app = axum::Router::new().route(
            "/rest/api/3/search",
            post(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "jql broken") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let err = p
            .search_issues(&AuthContext::default(), SearchParams::default())
            .await
            .expect_err("500 must fail");
        assert_eq!(err.to_string(), "jira search error 500: jql broken");
    }
}
