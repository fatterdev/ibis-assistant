//! threski-ticketing — provider-agnostic ticketing.
//!
//! Contents:
//! - [`types`]: the shared contract types — `ProviderName`, `IssueReference`,
//!   the [`Ingester`] trait consumed by the git history ingest, the
//!   provider-agnostic `Ticket`/params DTOs, [`AuthContext`] and the
//!   [`TicketProvider`] interface, plus the canonical `issue_record_id`.
//! - [`config`]: `ticketing_config.json` schema, defaults and normalization.
//! - [`service`]: provider resolution, workflow targets (config + multilingual
//!   status-name heuristic), and the D17-compliant issue-graph upsert the
//!   git ingest triggers on demand.
//! - [`parse`]: `provider_fields_json` decoding.
//! - [`migration`]: legacy issue backfill (contract parity; no-op on fresh DBs,
//!   D16).
//! - [`providers`]: the three backends — [`providers::redmine`] (reusing
//!   [`threski_ingest_redmine`]), [`providers::jira`] and
//!   [`providers::azuredevops`].
//!
//! Parity / D17 notes: the graph upsert is one explicit transaction with
//! `UPSERT`-first rows and edges related after their endpoints are persisted
//! (TI-01/TI-02 — the `UPDATE-based statements silently no-op'd
//! under SurrealDB 3.x). All ticket content is bound (F-002); record
//! identifiers are inlined via [`threski_db::ids::format_record_id`]. The `issue`,
//! `tracker`, `author`, `part_of` and `authored` fields the service writes were
//! already declared in `threski-db/src/schema.rs` — no schema additions needed.

pub mod config;
pub mod migration;
pub mod parse;
pub mod providers;
pub mod service;
pub mod types;

pub use config::{
    default_config, load_config, load_config_from_path, normalize_workflow_action,
    AzureDevOpsConfig, Config, ConfigLoadError, JiraConfig, PRConfig, ProviderConfigs,
    RedmineConfig, WorkflowActionConfig, WorkflowConfig,
};
pub use migration::migrate_legacy_issues;
pub use parse::parse_provider_fields_json;
pub use providers::azuredevops::Provider as AzureDevOpsProvider;
pub use providers::jira::Provider as JiraProvider;
pub use providers::redmine::Provider as RedmineProvider;
pub use service::{Capabilities, Service};
pub use types::{
    issue_record_id, AuthContext, CreateParams, Ingester, IngesterError, IssueReference, Project,
    ProviderName, SearchParams, SearchResult, Status, Ticket, TicketProvider, TicketingError,
    UpdateParams, User, WorkflowActionResult, PROVIDER_AZURE_DEVOPS, PROVIDER_JIRA,
    PROVIDER_REDMINE, WORKFLOW_CLOSE, WORKFLOW_REOPEN, WORKFLOW_RESOLVE,
};

#[cfg(test)]
pub(crate) mod test_support {
    /// Binds a local axum server on a random loopback port and returns its
    /// base URL (same pattern as the threski-ai / threski-ingest-redmine suites).
    pub async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }
}
