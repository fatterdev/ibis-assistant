//! Azure DevOps ticketing provider
//!
//!
//! Wire contract: Azure DevOps REST 7.1 — Basic auth with an **empty user +
//! PAT** (`Basic base64(":pat")`,, WIQL search then batched work-item
//! fetch, JSON-Patch bodies for create/update (`application/json-patch+json`).
//!
//! Deltas applied:
//! - **TI-08 (PERF S2)**: the WIQL query sends `$top={limit}` — fetched the
//!   full result set (huge `ids=` URLs blow ADO's URL-length limit).
//! - **TI-10 (BUG S2)**: `ListStatuses` fabricated sequential ids
//!   (`idx+1`) that later flowed into `System.State` patches — the
//!   resolve/close/reopen heuristic could never work for ADO. The port
//!   returns the state **name as id** (states in that API have no ids; the
//!   name is the transition token).
//! - **TI-09 (WART S3)**: `provider_fields` values keep their JSON type
//!   ( stringified everything via `fmt.Sprintf("%v")`).
//! - **TI-11 (WART S3)**: `toString` returns `""` for object values instead of
//!   the `map[...]` syntax garbage (unreachable after the assignee map branch).
//! - **TI-12 (WART S2)**: 30s HTTP timeout ( client had none).

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Map, Value};

use crate::config::AzureDevOpsConfig;
use crate::providers::net;
use crate::types::{
    AuthContext, CreateParams, Project, ProviderName, SearchParams, SearchResult, Status, Ticket,
    TicketProvider, TicketingError, UpdateParams, User, PROVIDER_AZURE_DEVOPS,
};

/// Azure DevOps backend. The configured
/// `repository` is carried by the config only for the repo-PR provider
/// ( stored it on this struct but never read it — TI-14).
pub struct Provider {
    org_url: String,
    project: String,
    pat: String,
    http: reqwest::Client,
}

impl Provider {
    /// Builds the provider; trims the config values.
    pub fn new(cfg: &AzureDevOpsConfig) -> Self {
        Provider {
            org_url: cfg.organization_url.trim_end_matches('/').to_string(),
            project: cfg.project.trim().to_string(),
            pat: cfg.pat.trim().to_string(),
            http: net::http_client(),
        }
    }

    /// Project override per call.
    fn resolve_project(&self, project_key: &str) -> String {
        let key = project_key.trim();
        if key.is_empty() {
            self.project.clone()
        } else {
            key.to_string()
        }
    }

    /// Basic token `":pat"` with per-request override.
    fn basic_token(&self, auth: &AuthContext) -> String {
        let pat = {
            let p = auth.azure_devops_pat.trim();
            if p.is_empty() {
                self.pat.clone()
            } else {
                p.to_string()
            }
        };
        base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"))
    }

    async fn request(
        &self,
        auth: &AuthContext,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<Value>,
        content_type: &str,
    ) -> Result<reqwest::Response, TicketingError> {
        let mut request = self.http.request(method, endpoint);
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        if !content_type.is_empty() {
            request = request.header("Content-Type", content_type);
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
            // the messages say "azure devops ..." (display name, not the
            // "azure_devops" provider key).
            provider: "azure devops",
            op,
            status,
            body,
        }
    }

    /// WIQL id query; `$top` bounds the result set (TI-08).
    async fn query_ids(
        &self,
        auth: &AuthContext,
        project: &str,
        wiql: &str,
        top: i64,
    ) -> Result<Vec<i64>, TicketingError> {
        let endpoint = format!(
            "{}/{}/_apis/wit/wiql?api-version=7.1&$top={top}",
            self.org_url,
            net::path_escape(project)
        );
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(json!({"query": wiql})),
                "application/json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "wiql").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(rename = "workItems", default)]
            work_items: Vec<RawId>,
        }
        #[derive(serde::Deserialize)]
        struct RawId {
            #[serde(default)]
            id: i64,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out.work_items.into_iter().map(|w| w.id).collect())
    }

    /// Batched work-item fetch.
    async fn get_work_items(
        &self,
        auth: &AuthContext,
        ids: &[i64],
    ) -> Result<Vec<Ticket>, TicketingError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let endpoint = format!(
            "{}/_apis/wit/workitems?ids={joined}&api-version=7.1",
            self.org_url
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None, "")
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "get workitems").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            value: Vec<WorkItem>,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out.value.iter().map(map_work_item).collect())
    }

    async fn do_search(
        &self,
        auth: &AuthContext,
        params: SearchParams,
        my_only: bool,
    ) -> Result<SearchResult, TicketingError> {
        let project = self.resolve_project(&params.project_key);
        if project.is_empty() {
            return Err(TicketingError::Invalid(
                "project key is required for azure_devops search".to_string(),
            ));
        }
        let limit = if params.limit <= 0 { 20 } else { params.limit };
        let wiql = build_wiql(&params, &project, my_only);
        let ids = self.query_ids(auth, &project, &wiql, limit).await?;
        let mut issues = self.get_work_items(auth, &ids).await?;
        if issues.len() as i64 > limit {
            issues.truncate(limit as usize);
        }
        Ok(SearchResult {
            total_count: issues.len() as i64,
            offset: params.offset,
            limit,
            issues,
        })
    }
}

/// A work item in REST shape.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct WorkItem {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    url: String,
    #[serde(default)]
    fields: Map<String, Value>,
}

/// Field extraction: strings verbatim, numbers rendered as
/// integers ( truncated floats the same way), `""` for absent/object values
/// (TI-11).
fn to_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => n
                .as_f64()
                .map(|f| (f as i64).to_string())
                .unwrap_or_default(),
        },
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn map_work_item(item: &WorkItem) -> Ticket {
    let id = item.id.to_string();
    let fields = &item.fields;
    let mut assignee = fields
        .get("System.AssignedTo")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if assignee.is_empty() {
        assignee = to_string(fields.get("System.AssignedTo"));
    }
    Ticket {
        provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
        id: id.clone(),
        external_id: id.clone(),
        external_key: id,
        project_key: to_string(fields.get("System.TeamProject")),
        title: to_string(fields.get("System.Title")),
        description: to_string(fields.get("System.Description")),
        status: to_string(fields.get("System.State")),
        issue_type: to_string(fields.get("System.WorkItemType")),
        assignee,
        url: item.url.clone(),
        updated_on: to_string(fields.get("System.ChangedDate")),
        created_on: to_string(fields.get("System.CreatedDate")),
        ..Ticket::default()
    }
}

/// Strips a `#<n>` suffix, keeping the trailing segment (
/// `normalizeWorkItemID`).
fn normalize_work_item_id(id: &str) -> String {
    let id = id.trim();
    match id.rfind('#') {
        Some(idx) => id[idx + 1..].to_string(),
        None => id.to_string(),
    }
}

/// Builds the WIQL query; single quotes are doubled.
fn build_wiql(params: &SearchParams, project: &str, my_only: bool) -> String {
    let esc = |v: &str| v.replace('\'', "''");
    let mut parts = vec![format!("[System.TeamProject] = '{}'", esc(project))];
    if !params.query.is_empty() {
        parts.push(format!("[System.Title] CONTAINS '{}'", esc(&params.query)));
    }
    if !params.status.is_empty() {
        parts.push(format!("[System.State] = '{}'", esc(&params.status)));
    }
    if !params.issue_type.is_empty() {
        parts.push(format!(
            "[System.WorkItemType] = '{}'",
            esc(&params.issue_type)
        ));
    }
    if !params.assignee.is_empty() && !my_only {
        parts.push(format!("[System.AssignedTo] = '{}'", esc(&params.assignee)));
    }
    if my_only {
        parts.push("[System.AssignedTo] = @Me".to_string());
    }
    format!(
        "SELECT [System.Id] FROM WorkItems WHERE {} ORDER BY [System.ChangedDate] DESC",
        parts.join(" AND ")
    )
}

#[async_trait]
impl TicketProvider for Provider {
    fn name(&self) -> ProviderName {
        ProviderName::from(PROVIDER_AZURE_DEVOPS)
    }

    async fn search_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        self.do_search(auth, params, false).await
    }

    async fn search_my_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        self.do_search(auth, params, true).await
    }

    async fn get_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
    ) -> Result<Option<Ticket>, TicketingError> {
        let id = normalize_work_item_id(id);
        let endpoint = format!(
            "{}/{}/_apis/wit/workitems/{}?api-version=7.1",
            self.org_url,
            net::path_escape(&self.resolve_project(project_key)),
            net::path_escape(&id)
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None, "")
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::api_error(response, "get issue").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        let item: WorkItem = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        let mut t = map_work_item(&item);
        if t.project_key.is_empty() {
            t.project_key = self.resolve_project(project_key);
        }
        Ok(Some(t))
    }

    async fn create_issue(
        &self,
        auth: &AuthContext,
        params: CreateParams,
    ) -> Result<Ticket, TicketingError> {
        let project = self.resolve_project(&params.project_key);
        if project.is_empty() {
            return Err(TicketingError::Invalid(
                "project key is required".to_string(),
            ));
        }
        let work_item_type = if params.issue_type.is_empty() {
            "Task".to_string()
        } else {
            params.issue_type.clone()
        };
        let mut ops = vec![json!({
            "op": "add",
            "path": "/fields/System.Title",
            "value": params.title,
        })];
        if !params.description.is_empty() {
            ops.push(json!({
                "op": "add",
                "path": "/fields/System.Description",
                "value": params.description,
            }));
        }
        if !params.assignee.is_empty() {
            ops.push(json!({
                "op": "add",
                "path": "/fields/System.AssignedTo",
                "value": params.assignee,
            }));
        }
        // Provider fields keep their JSON type (TI-09: stringified values).
        for (k, v) in params.provider_fields.iter().flatten() {
            ops.push(json!({
                "op": "add",
                "path": format!("/fields/{k}"),
                "value": v,
            }));
        }

        let endpoint = format!(
            "{}/{}/_apis/wit/workitems/${}?api-version=7.1",
            self.org_url,
            net::path_escape(&project),
            net::path_escape(&work_item_type)
        );
        let response = self
            .request(
                auth,
                reqwest::Method::POST,
                &endpoint,
                Some(Value::Array(ops)),
                "application/json-patch+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "create issue").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        let item: WorkItem = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(map_work_item(&item))
    }

    async fn update_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        params: UpdateParams,
    ) -> Result<Ticket, TicketingError> {
        let id = normalize_work_item_id(id);
        let mut ops: Vec<Value> = Vec::new();
        let add_op = |path: &str, value: &str, ops: &mut Vec<Value>| {
            ops.push(json!({"op": "add", "path": path, "value": value}));
        };
        if !params.status.is_empty() {
            add_op("/fields/System.State", &params.status, &mut ops);
        }
        if !params.issue_type.is_empty() {
            add_op("/fields/System.WorkItemType", &params.issue_type, &mut ops);
        }
        if !params.assignee.is_empty() {
            add_op("/fields/System.AssignedTo", &params.assignee, &mut ops);
        }
        if !params.priority.is_empty() {
            add_op(
                "/fields/Microsoft.VSTS.Common.Priority",
                &params.priority,
                &mut ops,
            );
        }
        if !params.notes.is_empty() {
            add_op("/fields/System.History", &params.notes, &mut ops);
        }
        for (k, v) in params.provider_fields.iter().flatten() {
            ops.push(json!({"op": "add", "path": format!("/fields/{k}"), "value": v}));
        }
        if ops.is_empty() {
            return self
                .get_issue(auth, &id, project_key)
                .await?
                .ok_or_else(|| TicketingError::Other(format!("issue {id} vanished after update")));
        }
        let endpoint = format!(
            "{}/{}/_apis/wit/workitems/{}?api-version=7.1",
            self.org_url,
            net::path_escape(&self.resolve_project(project_key)),
            net::path_escape(&id)
        );
        let response = self
            .request(
                auth,
                reqwest::Method::PATCH,
                &endpoint,
                Some(Value::Array(ops)),
                "application/json-patch+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "update issue").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        let item: WorkItem = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(map_work_item(&item))
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
        project_key: &str,
        issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError> {
        let project = self.resolve_project(project_key);
        let issue_type = if issue_type.is_empty() {
            "Task".to_string()
        } else {
            issue_type.to_string()
        };
        let endpoint = format!(
            "{}/{}/_apis/wit/workitemtypes/{}/states?api-version=7.1",
            self.org_url,
            net::path_escape(&project),
            net::path_escape(&issue_type)
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None, "")
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list statuses").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            value: Vec<RawState>,
        }
        #[derive(serde::Deserialize)]
        struct RawState {
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        // State names have no ids in this API — the name IS the transition
        // token (TI-10: fabricated sequential ids that broke the
        // workflow heuristics).
        Ok(out
            .value
            .into_iter()
            .map(|s| Status {
                id: s.name.clone(),
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
            "{}/_apis/identities?searchFilter=General&filterValue={}&queryMembership=None&api-version=7.1",
            self.org_url,
            net::path_escape(query)
        );
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None, "")
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "search users").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            value: Vec<RawIdentity>,
        }
        #[derive(serde::Deserialize)]
        struct RawIdentity {
            #[serde(default)]
            id: String,
            #[serde(rename = "displayName", default)]
            display_name: String,
            #[serde(rename = "mailAddress", default)]
            mail: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .value
            .into_iter()
            .take(limit as usize)
            .map(|u| User {
                id: u.id,
                name: u.display_name,
                email: u.mail,
            })
            .collect())
    }

    async fn list_projects(&self, auth: &AuthContext) -> Result<Vec<Project>, TicketingError> {
        let endpoint = format!("{}/_apis/projects?api-version=7.1", self.org_url);
        let response = self
            .request(auth, reqwest::Method::GET, &endpoint, None, "")
            .await?;
        if !response.status().is_success() {
            return Err(Self::api_error(response, "list projects").await);
        }
        let bytes = response.bytes().await.map_err(TicketingError::Request)?;
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            value: Vec<RawProject>,
        }
        #[derive(serde::Deserialize)]
        struct RawProject {
            #[serde(default)]
            id: String,
            #[serde(default)]
            name: String,
        }
        let out: Out = serde_json::from_slice(&bytes).map_err(TicketingError::Decode)?;
        Ok(out
            .value
            .into_iter()
            .map(|p| Project {
                id: p.id,
                key: p.name.clone(),
                name: p.name,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::routing::{get, post};
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct Capture {
        auth: Arc<Mutex<String>>,
        path: Arc<Mutex<String>>,
        query: Arc<Mutex<String>>,
        body: Arc<Mutex<String>>,
        method: Arc<Mutex<String>>,
        content_type: Arc<Mutex<String>>,
        calls: Arc<Mutex<Vec<String>>>,
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
        *capture.auth.lock().unwrap() = headers
            .get("Authorization")
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
            .push(format!("{method} {path}?{query} {body}"));
    }

    fn provider(base: &str) -> Provider {
        provider_with_project(base, "Core")
    }

    fn provider_with_project(base: &str, project: &str) -> Provider {
        Provider::new(&crate::config::AzureDevOpsConfig {
            organization_url: base.to_string(),
            project: project.to_string(),
            repository: "platform-repo".into(),
            pat: "pat-token".into(),
        })
    }

    fn basic(pat: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"))
        )
    }

    // Scenario: TestGetIssueUsesPAT.
    #[tokio::test]
    async fn get_issue_uses_pat() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/123",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    record(
                        &state,
                        "GET",
                        "/Core/_apis/wit/workitems/123",
                        "api-version=7.1",
                        &headers,
                        "",
                    );
                    axum::Json(serde_json::json!({
                        "id": 123,
                        "url": "http://ado/workitems/123",
                        "fields": {
                            "System.Title": "Fix bug",
                            "System.State": "Resolved",
                            "System.WorkItemType": "Bug",
                            "System.TeamProject": "Core"
                        }
                    }))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let issue = p
            .get_issue(&AuthContext::default(), "123", "Core")
            .await
            .unwrap()
            .expect("issue");
        assert_eq!(issue.external_id, "123");
        assert_eq!(issue.title, "Fix bug");
        assert_eq!(issue.status, "Resolved");
        assert_eq!(capture.auth.lock().unwrap().as_str(), &basic("pat-token"));
        assert!(capture
            .path
            .lock()
            .unwrap()
            .contains("/_apis/wit/workitems/"));

        // Per-request PAT override wins ( basicToken).
        p.get_issue(
            &AuthContext {
                azure_devops_pat: "user-pat".into(),
                ..AuthContext::default()
            },
            "123",
            "Core",
        )
        .await
        .unwrap();
        assert_eq!(capture.auth.lock().unwrap().as_str(), &basic("user-pat"));
    }

    #[tokio::test]
    async fn get_issue_404_is_none() {
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/404",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "missing") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        assert!(p
            .get_issue(&AuthContext::default(), "404", "Core")
            .await
            .unwrap()
            .is_none());
    }

    // Scenario: TestSearchIssuesWIQLFlow + TI-08 $top assertion.
    #[tokio::test]
    async fn search_issues_wiql_flow() {
        let capture = Capture::default();
        let state_wiql = capture.clone();
        let state_items = capture.clone();
        let app = axum::Router::new()
            .route(
                "/Core/_apis/wit/wiql",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state_wiql.clone();
                    async move {
                        record(
                            &state,
                            "POST",
                            "/Core/_apis/wit/wiql",
                            "api-version=7.1&$top=20",
                            &headers,
                            &body,
                        );
                        axum::Json(serde_json::json!({"workItems": [{"id": 50}]}))
                    }
                }),
            )
            .route(
                "/_apis/wit/workitems",
                get(
                    move |query: axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| {
                        let state = state_items.clone();
                        async move {
                            *state.query.lock().unwrap() = format!("{query:?}");
                            axum::Json(serde_json::json!({
                                "value": [{
                                    "id": 50,
                                    "fields": {
                                        "System.Title": "Timeout",
                                        "System.State": "Active",
                                        "System.WorkItemType": "Task",
                                        "System.TeamProject": "Core"
                                    }
                                }]
                            }))
                        }
                    },
                ),
            );
        let base = serve(app).await;

        let p = provider(&base);
        let result = p
            .search_issues(
                &AuthContext::default(),
                SearchParams {
                    project_key: "Core".into(),
                    query: "timeout".into(),
                    ..SearchParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].external_id, "50");
        assert_eq!(result.total_count, 1);
        assert_eq!(result.limit, 20);

        // TI-08: $top bounds the WIQL result set (recorded on the wiql call).
        let calls = capture.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            calls[0].contains("$top=20"),
            "expected $top=20: {}",
            calls[0]
        );
        assert!(
            calls[0].contains("[System.TeamProject] = 'Core'"),
            "{}",
            calls[0]
        );
        assert!(
            calls[0].contains("[System.Title] CONTAINS 'timeout'"),
            "{}",
            calls[0]
        );
        assert!(
            calls[0].contains("ORDER BY [System.ChangedDate] DESC"),
            "{}",
            calls[0]
        );
        // The batched work-item fetch carried the ids from the WIQL result.
        assert!(
            capture.query.lock().unwrap().contains(r#""ids": "50""#),
            "workitems ids: {:?}",
            capture.query.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn search_requires_project() {
        let p = provider_with_project("http://example.com", "");
        let err = p
            .search_issues(&AuthContext::default(), SearchParams::default())
            .await
            .expect_err("missing project must fail");
        assert_eq!(
            err.to_string(),
            "project key is required for azure_devops search"
        );
    }

    // search_my: @Me predicate, assignee filter dropped ( buildWIQL myOnly).
    #[tokio::test]
    async fn search_my_uses_me_predicate() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new()
            .route(
                "/Core/_apis/wit/wiql",
                post(move |headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        record(
                            &state,
                            "POST",
                            "/Core/_apis/wit/wiql",
                            "api-version=7.1&$top=20",
                            &headers,
                            &body,
                        );
                        axum::Json(serde_json::json!({"workItems": []}))
                    }
                }),
            )
            .route(
                "/_apis/wit/workitems",
                get(|| async { axum::Json(serde_json::json!({"value": []})) }),
            );
        let base = serve(app).await;

        let p = provider(&base);
        let result = p
            .search_my_issues(
                &AuthContext::default(),
                SearchParams {
                    project_key: "Core".into(),
                    assignee: "someone".into(),
                    ..SearchParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.issues.len(), 0);
        let wiql_body = capture.body.lock().unwrap().clone();
        assert!(
            wiql_body.contains("[System.AssignedTo] = @Me"),
            "{wiql_body}"
        );
        assert!(!wiql_body.contains("someone"), "{wiql_body}");
    }

    #[tokio::test]
    async fn create_requires_project() {
        let p = provider_with_project("http://example.com", "");
        let err = p
            .create_issue(
                &AuthContext::default(),
                CreateParams {
                    title: "t".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .expect_err("missing project must fail");
        assert_eq!(err.to_string(), "project key is required");
    }

    #[tokio::test]
    async fn create_posts_json_patch() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/$Task",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let state = state.clone();
                async move {
                    record(
                        &state,
                        "POST",
                        "/Core/_apis/wit/workitems/$Task",
                        "api-version=7.1",
                        &headers,
                        &body,
                    );
                    axum::Json(serde_json::json!({
                        "id": 77,
                        "url": "http://ado/77",
                        "fields": {"System.Title": "New bug", "System.State": "New",
                                   "System.WorkItemType": "Task", "System.TeamProject": "Core"}
                    }))
                }
            }),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let mut fields = serde_json::Map::new();
        fields.insert("custom_number".into(), serde_json::json!(5));
        fields.insert("custom_bool".into(), serde_json::json!(true));
        let t = p
            .create_issue(
                &AuthContext::default(),
                CreateParams {
                    project_key: "Core".into(),
                    title: "New bug".into(),
                    description: "Desc".into(),
                    assignee: "jane@x.co".into(),
                    provider_fields: Some(fields),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(t.external_id, "77");

        assert_eq!(
            capture.content_type.lock().unwrap().as_str(),
            "application/json-patch+json"
        );
        let body = capture.body.lock().unwrap().clone();
        assert!(
            body.contains(r#"{"op":"add","path":"/fields/System.Title","value":"New bug"}"#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/System.Description","value":"Desc""#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/System.AssignedTo","value":"jane@x.co""#),
            "{body}"
        );
        // TI-09: provider-field values keep their JSON type.
        assert!(
            body.contains(r#""path":"/fields/custom_number","value":5}"#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/custom_bool","value":true}"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn create_defaults_to_task_type() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/$Task",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let state = state.clone();
                async move {
                    record(
                        &state,
                        "POST",
                        "/Core/_apis/wit/workitems/$Task",
                        "api-version=7.1",
                        &headers,
                        &body,
                    );
                    axum::Json(serde_json::json!({"id": 1, "fields": {"System.Title": "t"}}))
                }
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        p.create_issue(
            &AuthContext::default(),
            CreateParams {
                project_key: "Core".into(),
                title: "t".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        // No description/assignee ops when empty.
        let body = capture.body.lock().unwrap().clone();
        assert_eq!(body.matches("\"op\":\"add\"").count(), 1, "{body}");
    }

    #[tokio::test]
    async fn update_posts_patch_with_field_paths() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/77",
            axum::routing::any(
                move |method: axum::http::Method, headers: axum::http::HeaderMap, body: String| {
                    let state = state.clone();
                    async move {
                        if method == axum::http::Method::PATCH {
                            record(
                                &state,
                                "PATCH",
                                "/Core/_apis/wit/workitems/77",
                                "api-version=7.1",
                                &headers,
                                &body,
                            );
                            return (
                                axum::http::StatusCode::OK,
                                r#"{"id":77,"url":"http://ado/77","fields":{"System.Title":"t"}}"#
                                    .to_string(),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            r#"{"id":77,"url":"http://ado/77","fields":{}}"#.to_string(),
                        )
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let t = p
            .update_issue(
                &AuthContext::default(),
                "77",
                "Core",
                UpdateParams {
                    status: "Resolved".into(),
                    issue_type: "Bug".into(),
                    assignee: "jane".into(),
                    priority: "2".into(),
                    notes: "history note".into(),
                    ..UpdateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(t.external_id, "77");

        let body = capture.body.lock().unwrap().clone();
        assert!(
            body.contains(r#""path":"/fields/System.State","value":"Resolved""#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/System.WorkItemType","value":"Bug""#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/System.AssignedTo","value":"jane""#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/Microsoft.VSTS.Common.Priority","value":"2""#),
            "{body}"
        );
        assert!(
            body.contains(r#""path":"/fields/System.History","value":"history note""#),
            "{body}"
        );
    }

    // Empty update → plain get (parity).
    #[tokio::test]
    async fn update_empty_is_get() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitems/77",
            get(move |headers: axum::http::HeaderMap| {
                let state = state.clone();
                async move {
                    record(
                        &state,
                        "GET",
                        "/Core/_apis/wit/workitems/77",
                        "api-version=7.1",
                        &headers,
                        "",
                    );
                    axum::Json(serde_json::json!({"id": 77, "fields": {"System.Title": "t"}}))
                }
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        p.update_issue(
            &AuthContext::default(),
            "77",
            "Core",
            UpdateParams::default(),
        )
        .await
        .unwrap();
        assert_eq!(capture.method.lock().unwrap().as_str(), "GET");
    }

    // TI-10: state names come back as their own id.
    #[tokio::test]
    async fn list_statuses_id_is_name() {
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitemtypes/Task/states",
            get(|| async {
                axum::Json(serde_json::json!({
                    "value": [{"name": "Proposed"}, {"name": "Resolved"}, {"name": "Closed"}]
                }))
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let statuses = p
            .list_statuses(&AuthContext::default(), "Core", "Task")
            .await
            .unwrap();
        assert_eq!(statuses.len(), 3);
        assert_eq!(statuses[1].id, "Resolved");
        assert_eq!(statuses[1].name, "Resolved");

        // Empty type defaults to Task (parity).
        p.list_statuses(&AuthContext::default(), "Core", "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_statuses_error_includes_body() {
        let app = axum::Router::new().route(
            "/Core/_apis/wit/workitemtypes/Task/states",
            get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "denied") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let err = p
            .list_statuses(&AuthContext::default(), "Core", "Task")
            .await
            .expect_err("401 must fail");
        assert_eq!(
            err.to_string(),
            "azure devops list statuses error 401: denied"
        );
    }

    #[tokio::test]
    async fn search_users_truncates_to_limit() {
        let capture = Capture::default();
        let state = capture.clone();
        let app = axum::Router::new().route(
            "/_apis/identities",
            get(
                move |query: axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let state = state.clone();
                    async move {
                        *state.query.lock().unwrap() = format!("{query:?}");
                        axum::Json(serde_json::json!({
                            "value": [
                                {"id": "u1", "displayName": "One", "mailAddress": "1@x.co"},
                                {"id": "u2", "displayName": "Two", "mailAddress": "2@x.co"},
                                {"id": "u3", "displayName": "Three", "mailAddress": "3@x.co"}
                            ]
                        }))
                    }
                },
            ),
        );
        let base = serve(app).await;

        let p = provider(&base);
        let users = p
            .search_users(&AuthContext::default(), "u", 2)
            .await
            .unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[1].id, "u2");

        let q = capture.query.lock().unwrap().clone();
        assert!(q.contains(r#""searchFilter": "General""#), "{q}");
        assert!(q.contains(r#""filterValue": "u""#), "{q}");
        assert!(q.contains(r#""queryMembership": "None""#), "{q}");
        assert!(q.contains(r#""api-version": "7.1""#), "{q}");
    }

    #[tokio::test]
    async fn list_projects_decodes() {
        let app = axum::Router::new().route(
            "/_apis/projects",
            get(|| async {
                axum::Json(serde_json::json!({
                    "value": [{"id": "p1", "name": "Core"}, {"id": "p2", "name": "Mobile"}]
                }))
            }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let projects = p.list_projects(&AuthContext::default()).await.unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].id, "p1");
        assert_eq!(projects[0].key, "Core");
    }

    // wiql endpoint errors surface in the original message shape.
    #[tokio::test]
    async fn wiql_error_includes_body() {
        let app = axum::Router::new().route(
            "/Core/_apis/wit/wiql",
            post(|| async { (axum::http::StatusCode::BAD_REQUEST, "bad wiql") }),
        );
        let base = serve(app).await;
        let p = provider(&base);
        let err = p
            .search_issues(
                &AuthContext::default(),
                SearchParams {
                    project_key: "Core".into(),
                    ..SearchParams::default()
                },
            )
            .await
            .expect_err("400 must fail");
        assert_eq!(err.to_string(), "azure devops wiql error 400: bad wiql");
    }
}
