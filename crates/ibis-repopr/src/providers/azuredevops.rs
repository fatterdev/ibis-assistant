//! Azure DevOps pull-request provider (
//! ).
//!
//! Wire contract (api-version=7.1, PAT Basic auth):
//! - create: `POST {org}/{project}/_apis/git/repositories/{repo}/pullrequests?api-version=7.1`
//! - complete: `PATCH {org}/{project}/_apis/git/repositories/{repo}/pullrequests/{pr_id}?api-version=7.1`
//! - headers: `Content-Type: application/json`, `Accept: application/json`,
//!   `Authorization: Basic base64(":PAT")` — the per-call auth PAT overrides
//!   the provider-configured PAT.
//!
//! Deltas applied during this crate:
//! - **HTTP timeout 30s** (REP-A02: used `&http.Client{}` with no timeout —
//!   a stalled endpoint hung the synchronous MCP tool forever).
//! - **Credentials fail fast** (REP-A03, D15): with no PAT from auth or config
//!   the provider returns [`RepoprError::CredentialsRequired`] instead of
//!   sending an empty `Basic` header and surfacing the server's 401.
//! - **Error response bodies capped** at 64 KiB (REP-A01).
//! - **Payload encode errors propagate** (REP-A04: swallowed them).
//! - **Path segments percent-escaped** like `url.PathEscape (verified
//!   against the original toolchain: keep `A-Za-z0-9-_.~$&+:;=@`, escape `/;?, ` and
//!   everything else).

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine;
use reqwest::Method;
use serde_json::{json, Map, Value};
use tracing::debug;

use crate::error::{error_body, RepoprError};
use crate::types::{
    AuthContext, CompleteParams, CreateParams, Provider, ProviderName, PullRequest,
    PROVIDER_AZURE_DEVOPS,
};

/// HTTP budget for provider calls (the original had none; this crate pins the same 30s
/// used by the Redmine client — REP-A02).
pub const DEFAULT_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Provider construction options.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub organization_url: String,
    pub project: String,
    pub repository: String,
    pub pat: String,
}

enum Op {
    Create,
    Complete,
}

/// REST client for Azure DevOps pull requests.
pub struct AzureDevOpsProvider {
    org_url: String,
    project: String,
    repo: String,
    pat: String,
    http: reqwest::Client,
}

impl AzureDevOpsProvider {
    /// `New: trims the trailing `/` off the organization URL and the
    /// project/repository/PAT fields.
    pub fn new(cfg: Config) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_HTTP_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self::with_http_client(cfg, client)
    }

    /// Seam for tests/custom clients ( tests swapped the `http` field via
    /// the organization URL; `with_http_client` allows injecting directly).
    pub fn with_http_client(cfg: Config, http: reqwest::Client) -> Self {
        AzureDevOpsProvider {
            org_url: cfg.organization_url.trim_end_matches('/').to_string(),
            project: cfg.project.trim().to_string(),
            repo: cfg.repository.trim().to_string(),
            pat: cfg.pat.trim().to_string(),
            http,
        }
    }

    /// `resolveProject: explicit param (trimmed) wins over the configured
    /// project.
    fn resolve_project(&self, project: &str) -> String {
        let project = project.trim();
        if !project.is_empty() {
            project.to_string()
        } else {
            self.project.clone()
        }
    }

    /// `resolveRepo: explicit param (trimmed) wins over the configured repo.
    fn resolve_repo(&self, repo: &str) -> String {
        let repo = repo.trim();
        if !repo.is_empty() {
            repo.to_string()
        } else {
            self.repo.clone()
        }
    }

    /// `basicToken precedence: auth PAT (trimmed) over configured PAT.
    fn resolved_pat(&self, auth: &AuthContext) -> String {
        let pat = auth.azure_devops_pat.trim();
        if pat.is_empty() {
            self.pat.clone()
        } else {
            pat.to_string()
        }
    }

    async fn do_json(
        &self,
        auth: &AuthContext,
        method: Method,
        endpoint: &str,
        payload: Value,
    ) -> Result<reqwest::Response, RepoprError> {
        let pat = self.resolved_pat(auth);
        if pat.is_empty() {
            return Err(RepoprError::CredentialsRequired);
        }
        let body = serde_json::to_vec(&payload)?;
        debug!("Azure DevOps: {method} {endpoint}");
        let response = self
            .http
            .request(method, endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", BASE64_STD.encode(format!(":{pat}"))),
            )
            .body(body)
            .send()
            .await?;
        Ok(response)
    }

    async fn decode_pull_request(
        &self,
        response: reqwest::Response,
        op: Op,
    ) -> Result<PullRequest, RepoprError> {
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(RepoprError::Request)?;
        if status >= 300 {
            let body = error_body(&bytes);
            return Err(match op {
                Op::Create => RepoprError::CreateApi { status, body },
                Op::Complete => RepoprError::CompleteApi { status, body },
            });
        }
        #[derive(serde::Deserialize)]
        struct PrResponse {
            #[serde(default, rename = "pullRequestId")]
            pull_request_id: i64,
            #[serde(default)]
            title: String,
            #[serde(default)]
            status: String,
            #[serde(default)]
            url: String,
        }
        let out: PrResponse = serde_json::from_slice(&bytes)?;
        Ok(PullRequest {
            provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            id: out.pull_request_id.to_string(),
            url: out.url,
            title: out.title,
            status: out.status,
        })
    }
}

#[async_trait]
impl Provider for AzureDevOpsProvider {
    fn name(&self) -> ProviderName {
        ProviderName::from(PROVIDER_AZURE_DEVOPS)
    }

    async fn create_pr(
        &self,
        auth: &AuthContext,
        params: &CreateParams,
    ) -> Result<PullRequest, RepoprError> {
        let project = self.resolve_project(&params.project_name);
        let repo = self.resolve_repo(&params.repository);
        if project.is_empty() || repo.is_empty() {
            return Err(RepoprError::AzureDevOpsProjectRepoRequired);
        }
        if params.source_branch.trim().is_empty() {
            return Err(RepoprError::SourceBranchRequired);
        }
        let source = ensure_ref(&params.source_branch);
        let target = ensure_ref(&params.target_branch);
        let mut payload = Map::new();
        payload.insert("sourceRefName".into(), Value::String(source));
        payload.insert("targetRefName".into(), Value::String(target));
        payload.insert("title".into(), Value::String(params.title.clone()));
        payload.insert(
            "description".into(),
            Value::String(append_ticket_block(&params.description, &params.ticket_ids)),
        );
        if !params.reviewer_ids.is_empty() {
            let reviewers: Vec<Value> = params
                .reviewer_ids
                .iter()
                .map(|r| r.trim())
                .filter(|r| !r.is_empty())
                .map(|r| json!({"id": r}))
                .collect();
            if !reviewers.is_empty() {
                payload.insert("reviewers".into(), Value::Array(reviewers));
            }
        }
        if params.auto_complete {
            payload.insert(
                "completionOptions".into(),
                json!({"deleteSourceBranch": false, "squashMerge": false}),
            );
        }

        let endpoint = format!(
            "{}/{}/_apis/git/repositories/{}/pullrequests?api-version=7.1",
            self.org_url,
            path_escape(&project),
            path_escape(&repo),
        );
        let response = self
            .do_json(auth, Method::POST, &endpoint, Value::Object(payload))
            .await?;
        self.decode_pull_request(response, Op::Create).await
    }

    async fn complete_pr(
        &self,
        auth: &AuthContext,
        params: &CompleteParams,
    ) -> Result<PullRequest, RepoprError> {
        let project = self.resolve_project(&params.project_name);
        let repo = self.resolve_repo(&params.repository);
        if project.is_empty() || repo.is_empty() {
            return Err(RepoprError::AzureDevOpsProjectRepoRequired);
        }
        if params.pr_id.trim().is_empty() {
            return Err(RepoprError::PrIdRequired);
        }
        let payload = json!({
            "status": "completed",
            "completionOptions": {
                "deleteSourceBranch": params.delete_source_branch,
                "squashMerge": params.squash,
            },
        });
        let endpoint = format!(
            "{}/{}/_apis/git/repositories/{}/pullrequests/{}?api-version=7.1",
            self.org_url,
            path_escape(&project),
            path_escape(&repo),
            // Scenario: escaped `params.PRID` untrimmed (only the emptiness check trims)
            path_escape(&params.pr_id),
        );
        let response = self
            .do_json(auth, Method::PATCH, &endpoint, payload)
            .await?;
        self.decode_pull_request(response, Op::Complete).await
    }
}

/// `ensureRef: bare branch names get the `refs/heads/` prefix.
pub(crate) fn ensure_ref(branch: &str) -> String {
    let branch = branch.trim();
    if branch.starts_with("refs/") {
        return branch.to_string();
    }
    format!("refs/heads/{branch}")
}

/// `appendTicketBlock: appends `Linked tickets: ...` to the trimmed
/// description, replacing it when empty; no-op when there are no tickets.
pub(crate) fn append_ticket_block(description: &str, ticket_ids: &[String]) -> String {
    let description = description.trim();
    if ticket_ids.is_empty() {
        return description.to_string();
    }
    let block = format!("Linked tickets: {}", ticket_ids.join(", "));
    if description.is_empty() {
        return block;
    }
    format!("{description}\n\n{block}")
}

/// `url.PathEscape (encodePathSegment): percent-escapes every byte except
/// unreserved `A-Za-z0-9-_.~` and the reserved `$ & + : = @`; `/ ; , ?` and
/// anything else (including spaces, non-ASCII and `%`) become `%XX`.
pub(crate) fn path_escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        let ch = *byte as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '-' | '_' | '.' | '~' | '$' | '&' | '+' | ':' | '=' | '@'
            );
        if keep {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    async fn serve(router: axum::Router) -> String {
        crate::test_support::serve(router).await
    }

    fn provider(base: &str) -> AzureDevOpsProvider {
        AzureDevOpsProvider::new(Config {
            organization_url: base.to_string(),
            project: "core".into(),
            repository: "repo".into(),
            pat: "pat".into(),
        })
    }

    // Scenario: TestCreatePR: exact PAT Basic auth, POST, /pullrequests path,
    // response decode.
    #[tokio::test]
    async fn create_pr_posts_json_with_pat_basic_auth() {
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_in_handler = seen.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen = seen_in_handler.clone();
                async move {
                    let authz = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    *seen.lock().unwrap() = authz;
                    axum::Json(serde_json::json!({
                        "pullRequestId": 88,
                        "title": "My PR",
                        "status": "active",
                        "url": "http://ado/pr/88",
                    }))
                }
            }),
        );
        let base = serve(app).await;
        let pr = provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "feature/x".into(),
                    target_branch: "main".into(),
                    title: "My PR".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.id, "88");
        assert_eq!(pr.title, "My PR");
        assert_eq!(pr.status, "active");
        assert_eq!(pr.url, "http://ado/pr/88");
        assert_eq!(
            *seen.lock().unwrap(),
            format!("Basic {}", BASE64_STD.encode(":pat"))
        );
    }

    // Scenario: TestCompletePR: PATCH verb and completed status.
    #[tokio::test]
    async fn complete_pr_patches_and_returns_status() {
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests/88",
            axum::routing::patch(|| async {
                axum::Json(serde_json::json!({
                    "pullRequestId": 88,
                    "title": "My PR",
                    "status": "completed",
                }))
            }),
        );
        let base = serve(app).await;
        let pr = provider(&base)
            .complete_pr(
                &AuthContext::default(),
                &CompleteParams {
                    pr_id: "88".into(),
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.status, "completed");
        assert_eq!(pr.id, "88");
    }

    // Extra (REP-A02/REP-A03): auth PAT from the call overrides the
    // configured PAT (`basicToken precedence).
    #[tokio::test]
    async fn auth_pat_overrides_configured_pat() {
        let seen = Arc::new(Mutex::new(String::new()));
        let seen_in_handler = seen.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen = seen_in_handler.clone();
                async move {
                    *seen.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    axum::Json(serde_json::json!({"pullRequestId": 1}))
                }
            }),
        );
        let base = serve(app).await;
        let mut provider = provider(&base);
        provider.pat = "cfg-pat".into();
        provider
            .create_pr(
                &AuthContext {
                    azure_devops_pat: "hdr-pat".into(),
                },
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            format!("Basic {}", BASE64_STD.encode(":hdr-pat"))
        );
    }

    // Extra (REP-A03, D15): no PAT anywhere → typed credentials error, no HTTP call.
    #[tokio::test]
    async fn credentials_required_when_no_pat() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = hits.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move || {
                hits_in_handler.fetch_add(1, Ordering::SeqCst);
                async { axum::Json(serde_json::json!({"pullRequestId": 1})) }
            }),
        );
        let base = serve(app).await;
        let mut provider = provider(&base);
        provider.pat = String::new();
        let err = provider
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RepoprError::CredentialsRequired));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    // Extra: create payload shape — refs, ticket block, reviewers filtering,
    // auto-complete completionOptions, empty title/description kept as "".
    #[tokio::test]
    async fn create_payload_shape() {
        let body = Arc::new(Mutex::new(Value::Null));
        let body_in_handler = body.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move |payload: axum::Json<Value>| {
                let body = body_in_handler.clone();
                async move {
                    *body.lock().unwrap() = payload.0;
                    axum::Json(serde_json::json!({"pullRequestId": 5}))
                }
            }),
        );
        let base = serve(app).await;
        let pr = provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: " feature/x ".into(),
                    target_branch: "main".into(),
                    title: "My PR".into(),
                    description: "hello".into(),
                    ticket_ids: vec!["#12".into(), "ABC-1".into()],
                    reviewer_ids: vec!["u1".into(), "  ".into(), String::new()],
                    auto_complete: true,
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.id, "5");
        let payload = body.lock().unwrap().clone();
        assert_eq!(payload["sourceRefName"], "refs/heads/feature/x");
        assert_eq!(payload["targetRefName"], "refs/heads/main");
        assert_eq!(payload["title"], "My PR");
        assert_eq!(
            payload["description"],
            "hello\n\nLinked tickets: #12, ABC-1"
        );
        assert_eq!(payload["reviewers"], serde_json::json!([{"id": "u1"}]));
        assert_eq!(
            payload["completionOptions"],
            serde_json::json!({"deleteSourceBranch": false, "squashMerge": false})
        );
    }

    // Extra: all-blank reviewers omit the key; auto_complete off omits
    // completionOptions; empty title/description are sent as empty strings
    // ( map marshal parity).
    #[tokio::test]
    async fn create_payload_optional_keys() {
        let body = Arc::new(Mutex::new(Value::Null));
        let body_in_handler = body.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move |payload: axum::Json<Value>| {
                let body = body_in_handler.clone();
                async move {
                    *body.lock().unwrap() = payload.0;
                    axum::Json(serde_json::json!({"pullRequestId": 6}))
                }
            }),
        );
        let base = serve(app).await;
        provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    target_branch: "main".into(),
                    reviewer_ids: vec![" ".into(), String::new()],
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        let payload = body.lock().unwrap().clone();
        assert!(payload.get("reviewers").is_none());
        assert!(payload.get("completionOptions").is_none());
        assert_eq!(payload["title"], "");
        assert_eq!(payload["description"], "");
    }

    // Extra: complete payload shape.
    #[tokio::test]
    async fn complete_payload_shape() {
        let body = Arc::new(Mutex::new(Value::Null));
        let body_in_handler = body.clone();
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests/{pr}",
            axum::routing::patch(
                move |axum::extract::Path(pr): axum::extract::Path<String>,
                      payload: axum::Json<Value>| {
                    let body = body_in_handler.clone();
                    async move {
                        assert_eq!(pr, "88");
                        *body.lock().unwrap() = payload.0;
                        axum::Json(serde_json::json!({"pullRequestId": 88, "status": "completed"}))
                    }
                },
            ),
        );
        let base = serve(app).await;
        provider(&base)
            .complete_pr(
                &AuthContext::default(),
                &CompleteParams {
                    pr_id: "88".into(),
                    delete_source_branch: true,
                    squash: false,
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap();
        let payload = body.lock().unwrap().clone();
        assert_eq!(payload["status"], "completed");
        assert_eq!(
            payload["completionOptions"],
            serde_json::json!({"deleteSourceBranch": true, "squashMerge": false})
        );
    }

    // Extra (REP-A01): non-2xx bodies surface in the error, capped at 64 KiB.
    #[tokio::test]
    async fn create_error_surfaces_capped_body() {
        let long = "x".repeat(crate::error::MAX_ERROR_BODY + 10);
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(move || {
                let long = long.clone();
                async move { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, long) }
            }),
        );
        let base = serve(app).await;
        let err = provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("azure devops create pr error 500: "));
        assert!(text.contains("…(truncated)"));
        assert_eq!(
            text.len(),
            "azure devops create pr error 500: ".len()
                + crate::error::MAX_ERROR_BODY
                + "…(truncated)".len()
        );
    }

    // Extra: complete error keeps the original "complete pr" wording.
    #[tokio::test]
    async fn complete_error_surfaces_body() {
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests/88",
            axum::routing::patch(|| async { (axum::http::StatusCode::BAD_REQUEST, "boom") }),
        );
        let base = serve(app).await;
        let err = provider(&base)
            .complete_pr(
                &AuthContext::default(),
                &CompleteParams {
                    pr_id: "88".into(),
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "azure devops complete pr error 400: boom");
    }

    // Extra: zero-value decode parity — a response without pullRequestId
    // yields id "0" ( decoded into the zero int).
    #[tokio::test]
    async fn missing_pull_request_id_defaults_to_zero() {
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(|| async { axum::Json(serde_json::json!({})) }),
        );
        let base = serve(app).await;
        let pr = provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.id, "0");
    }

    // Extra: validation errors, exact messages.
    #[tokio::test]
    async fn validation_errors() {
        let base = "http://127.0.0.1:1"; // never reached
        let bare = AzureDevOpsProvider::new(Config {
            organization_url: base.into(),
            ..Config::default()
        });
        let err = bare
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "azure devops project and repository are required"
        );
        let err = bare
            .complete_pr(
                &AuthContext::default(),
                &CompleteParams {
                    pr_id: "1".into(),
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "azure devops project and repository are required"
        );

        let p = provider(base);
        let err = p
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "source_branch is required");
        let err = p
            .complete_pr(
                &AuthContext::default(),
                &CompleteParams {
                    ..CompleteParams::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "pr_id is required");
    }

    // Extra: trailing slashes on the organization URL are trimmed in New.
    #[tokio::test]
    async fn organization_url_trailing_slash_trimmed() {
        let app = axum::Router::new().route(
            "/core/_apis/git/repositories/repo/pullrequests",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"pullRequestId": 9})) }),
        );
        let base = serve(app).await;
        let cfg = Config {
            organization_url: format!("{base}/"),
            project: "core".into(),
            repository: "repo".into(),
            pat: "pat".into(),
        };
        let pr = AzureDevOpsProvider::new(cfg)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.id, "9");
    }

    // Extra: param-level project/repository overrides beat the configured ones
    // (`resolveProject/`resolveRepo`).
    #[tokio::test]
    async fn param_overrides_for_project_and_repo() {
        let app = axum::Router::new().route(
            "/other/_apis/git/repositories/other-repo/pullrequests",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"pullRequestId": 11})) }),
        );
        let base = serve(app).await;
        let pr = provider(&base)
            .create_pr(
                &AuthContext::default(),
                &CreateParams {
                    source_branch: "b".into(),
                    project_name: "other".into(),
                    repository: "other-repo".into(),
                    ..CreateParams::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(pr.id, "11");
    }

    #[test]
    fn ensure_ref_matrix() {
        assert_eq!(ensure_ref("main"), "refs/heads/main");
        assert_eq!(ensure_ref(" feature/x "), "refs/heads/feature/x");
        assert_eq!(ensure_ref("refs/heads/main"), "refs/heads/main");
        assert_eq!(ensure_ref("refs/releases/1"), "refs/releases/1");
    }

    #[test]
    fn append_ticket_block_matrix() {
        assert_eq!(append_ticket_block("  desc  ", &[]), "desc");
        assert_eq!(append_ticket_block("", &[]), "");
        assert_eq!(
            append_ticket_block("", &["#1".to_string()]),
            "Linked tickets: #1"
        );
        assert_eq!(
            append_ticket_block("desc", &["#1".to_string(), "#2".to_string()]),
            "desc\n\nLinked tickets: #1, #2"
        );
    }

    // Byte-parity with url.PathEscape (verified against the original toolchain).
    #[test]
    fn path_escape_matches_reference() {
        assert_eq!(path_escape("allowed/&$+,:;=@~"), "allowed%2F&$+%2C:%3B=@~");
        assert_eq!(path_escape("My Org"), "My%20Org");
        assert_eq!(path_escape("a?b"), "a%3Fb");
        assert_eq!(path_escape("a;b,c/d"), "a%3Bb%2Cc%2Fd");
        assert_eq!(path_escape("plain"), "plain");
        assert_eq!(path_escape("repo%20x"), "repo%2520x");
        assert_eq!(path_escape("café"), "caf%C3%A9");
        assert_eq!(path_escape("!"), "%21");
        assert_eq!(path_escape("()'"), "%28%29%27");
        assert_eq!(path_escape("#frag"), "%23frag");
        assert_eq!(path_escape("a+b"), "a+b");
        assert_eq!(path_escape("pr/88"), "pr%2F88");
    }
}
