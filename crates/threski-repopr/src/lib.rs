//! threski-repopr — repository pull-request orchestration (
//! `internal/repopr`).
//!
//! The [`Service`] resolves a [`Provider`] per call (explicit provider argument
//! → project map → configured default), applies target-branch fallbacks
//! (explicit → project map → default → `main`), auto-builds title/description
//! and auto-detects ticket references (Jira `ABC-12`, Azure DevOps `PROJ#10`,
//! Redmine `#123`) before delegating to the provider. The only provider implemented
//! day-1 is [`AzureDevOpsProvider`], a plain REST
//! client over `reqwest` (PAT Basic auth, `api-version=7.1`).
//!
//! Parity notes / deltas applied:
//! - **Explicit [`AuthContext`] parameter** replaces the original context-value
//!   lookup (`auth.AzureDevOpsPATContextKey`, set by the HTTP auth middleware
//!   from `X-Azure-DevOps-PAT`) — the redmine-lane precedent (E-A13). The
//!   precedence `auth PAT → provider config PAT` and the trimming are
//!   preserved.
//! - **Error response bodies are capped** (64 KiB) before inclusion in error
//!   messages (REP-A01; read the body unbounded).
//! - **HTTP timeout 30s** added (REP-A02; the `http.Client{}` had none, so a
//!   stalled endpoint hung the synchronous MCP tool forever).
//! - **Credentials fail fast** (REP-A03, D15): when no PAT resolves from auth
//!   or config, [`RepoprError::CredentialsRequired`] is returned instead of
//!   sending an empty `Basic` header and surfacing the server's 401 body.
//! - **Payload encode errors propagate** (REP-A04; swallowed the
//!   `json.Marshal` error and sent an empty body).
//!
//! Scope notes for the audit: the original package performs **no git operations**
//! (verified — no `exec.Command`, nothing to route through `threski_git::GitRunner`,
//! D-A07 N/A) and **no DB writes** (no SurrealQL, `threski-db` untouched). Wave 4
//! wires `repo_pr_create` / `repo_pr_complete` to
//! [`Service::create_pr`] / [`Service::complete_pr`].

pub mod error;
pub mod providers;
pub mod service;
pub mod types;

pub use error::RepoprError;
pub use providers::azuredevops::AzureDevOpsProvider;
pub use service::{detect_ticket_ids, Service};
pub use types::{
    AuthContext, CompleteParams, Config, CreateParams, Provider, ProviderName, PullRequest,
    PROVIDER_AZURE_DEVOPS,
};

#[cfg(test)]
pub(crate) mod test_support {
    /// Binds a local axum server on a random loopback port and returns its
    /// base URL (same pattern as the threski-ai test suite).
    pub async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }
}
