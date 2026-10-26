//! Shared ticketing contract types:
//! provider names, `IssueReference`, the `Ingester` trait, `IssueRecordID`,
//! the provider-agnostic `Ticket`/params/result DTOs and the
//! [`TicketProvider`] interface consumed by the service.

use async_trait::async_trait;
use ibis_db::ids::{format_record_id, sanitize_id};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use ibis_db::schema::names::ISSUE;

/// Identifies a ticketing backend. Loose string
/// type: unknown values roundtrip and resolve to the redmine default where the
/// Scenario: code did so.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderName(pub String);

impl ProviderName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for ProviderName {
    fn from(s: &str) -> Self {
        ProviderName(s.to_string())
    }
}

impl From<String> for ProviderName {
    fn from(s: String) -> Self {
        ProviderName(s)
    }
}

impl std::fmt::Display for ProviderName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub const PROVIDER_REDMINE: &str = "redmine";
pub const PROVIDER_JIRA: &str = "jira";
pub const PROVIDER_AZURE_DEVOPS: &str = "azure_devops";

/// Workflow action names.
pub const WORKFLOW_RESOLVE: &str = "resolve";
pub const WORKFLOW_CLOSE: &str = "close";
pub const WORKFLOW_REOPEN: &str = "reopen";

/// A provider-agnostic ticket. Wire shape mirrors the
/// Scenario: JSON tags (`omitempty` → skip-when-empty via the `skip_serializing_if`
/// helpers); `provider_fields` carries raw provider JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub provider: ProviderName,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default)]
    pub external_id: String,
    #[serde(default)]
    pub external_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, rename = "type", skip_serializing_if = "String::is_empty")]
    pub issue_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_on: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_fields: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Ticket {
    /// Compact search line"`) — the response contract.
    /// Convenience for the future `ibis-server` ticket tools lane.
    pub fn compact_line(&self) -> String {
        format!(
            "[{}:{}] {} ({})",
            self.provider, self.external_key, self.title, self.status
        )
    }
}

/// Search filters. All fields optional; `limit`
/// and `offset` are `i64` because the MCP arg helpers accept signed ints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issue_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_from: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_to: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sort: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub limit: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub offset: i64,
}

/// `omitempty for int fields (0 is omitted).
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// Ticket-creation parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CreateParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issue_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_fields: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider_fields_json: String,
}

/// Ticket-update parameters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateParams {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issue_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow_action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fixed_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_fields: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider_fields_json: String,
}

/// Paginated search result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub issues: Vec<Ticket>,
    #[serde(default)]
    pub total_count: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
}

/// A workflow status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub name: String,
}

/// A ticketing user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
}

/// A ticketing project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    pub name: String,
}

/// Result of a workflow transition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowActionResult {
    pub provider: ProviderName,
    pub action: String,
    pub target: String,
    pub issue_id: String,
}

/// Per-request ticketing credentials. sourced
/// them from context values injected by the HTTP layer (`auth.*ContextKey`);
/// the Rust service takes them as an explicit argument per call (
/// E-A13 pattern — no ambient context).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthContext {
    pub redmine_api_key: String,
    pub jira_email: String,
    pub jira_api_token: String,
    pub azure_devops_pat: String,
}

/// A ticket reference extracted from commit text or produced by a provider
/// An empty provider means "unset", mirroring
/// the `omitempty semantics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueReference {
    pub provider: ProviderName,
    pub external_id: String,
    pub external_key: String,
    pub project_key: String,
    pub confidence: f64,
}

/// Minimal opaque error for [`Ingester`] implementations ( returned plain
/// `error`); kept stable for the already-implemented `ibis-ingest-git` consumer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct IngesterError(pub String);

/// Issue-ingest boundary consumed by the git history ingest (
/// `ticketing.Ingester`): resolve a parsed reference against the configured
/// providers, then fetch/ingest the referenced issue off the hot path.
/// [`crate::service::Service`] implements this.
#[async_trait]
pub trait Ingester: Send + Sync {
    async fn resolve_reference(
        &self,
        project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<IssueReference, IngesterError>;

    async fn ingest_issue_reference(
        &self,
        project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<(), IngesterError>;
}

/// One ticketing backend. Implementations live
/// in [`crate::providers`]; the service dispatches to them by provider name.
///
/// `auth` carries the per-request credentials header contract);
/// implementations fall back to their configured credentials when the
/// corresponding field is empty.
#[async_trait]
pub trait TicketProvider: Send + Sync {
    fn name(&self) -> ProviderName;

    async fn search_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError>;

    async fn search_my_issues(
        &self,
        auth: &AuthContext,
        params: SearchParams,
    ) -> Result<SearchResult, TicketingError>;

    /// Returns `Ok(None)` when the provider answers 404.
    async fn get_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
    ) -> Result<Option<Ticket>, TicketingError>;

    async fn create_issue(
        &self,
        auth: &AuthContext,
        params: CreateParams,
    ) -> Result<Ticket, TicketingError>;

    async fn update_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        params: UpdateParams,
    ) -> Result<Ticket, TicketingError>;

    async fn add_comment(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        comment: &str,
    ) -> Result<(), TicketingError>;

    async fn assign_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        assignee: &str,
    ) -> Result<(), TicketingError>;

    async fn transition_issue(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        transition: &str,
    ) -> Result<(), TicketingError>;

    async fn list_statuses(
        &self,
        auth: &AuthContext,
        project_key: &str,
        issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError>;

    async fn search_users(
        &self,
        auth: &AuthContext,
        query: &str,
        limit: i64,
    ) -> Result<Vec<User>, TicketingError>;

    async fn list_projects(&self, auth: &AuthContext) -> Result<Vec<Project>, TicketingError>;
}

/// Canonical SurrealDB record id for a ticket:
/// redmine keys stay bare on the `issue` table when numeric, every other
/// provider is prefixed into the sanitized key.
pub fn issue_record_id(provider: &str, external_key: &str) -> String {
    let key = external_key.trim();
    let key = if key.is_empty() { "unknown" } else { key };
    if provider == PROVIDER_REDMINE {
        return format_record_id(ISSUE, &sanitize_id(key));
    }
    format_record_id(ISSUE, &sanitize_id(&format!("{provider}_{key}")))
}

/// Typed error surface for ticketing operations ( returned plain `error`
/// strings; the Display strings preserve the original messages).
#[derive(Debug, Error)]
pub enum TicketingError {
    #[error("provider '{0}' is not registered")]
    UnknownProvider(String),
    #[error("no workflow target for action '{0}'")]
    NoWorkflowTarget(String),
    #[error("unable to infer workflow status target")]
    UnresolvedWorkflowTarget,
    #[error("title is required")]
    TitleRequired,
    #[error("invalid provider_fields_json: {0}")]
    InvalidProviderFieldsJson(String),
    #[error("provider request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error(transparent)]
    Redmine(#[from] ibis_ingest_redmine::RedmineError),
    /// Non-2xx provider API response; `op` reproduces the original message fragment
    /// (`"redmine create error 500: body"`, `"jira search error 400: body"`,
    /// `"azure devops get issue error 404: body"`).
    #[error("{provider} {op} error {status}: {body}")]
    Api {
        provider: &'static str,
        op: &'static str,
        status: u16,
        body: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("failed to decode provider response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error(transparent)]
    Db(#[from] ibis_db::error::DbError),
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_record_id_matches_reference_shapes() {
        // redmine numeric: bare issue id (Q5 numeric exception)
        assert_eq!(issue_record_id(PROVIDER_REDMINE, "42"), "issue:42");
        // redmine sanitized
        assert_eq!(
            issue_record_id(PROVIDER_REDMINE, "abc 12"),
            "issue:⟨abc_12⟩"
        );
        // provider-prefixed
        assert_eq!(
            issue_record_id(PROVIDER_JIRA, "ABC-321"),
            "issue:⟨jira_abc_321⟩"
        );
        assert_eq!(
            issue_record_id(PROVIDER_AZURE_DEVOPS, "77"),
            "issue:⟨azure_devops_77⟩"
        );
        // empty key
        assert_eq!(issue_record_id(PROVIDER_REDMINE, ""), "issue:⟨unknown⟩");
        assert_eq!(issue_record_id(PROVIDER_REDMINE, "  "), "issue:⟨unknown⟩");
    }

    #[test]
    fn provider_name_roundtrip() {
        let p = ProviderName::from("jira");
        assert_eq!(p.as_str(), "jira");
        assert!(!p.is_empty());
        assert!(ProviderName::default().is_empty());
        assert_eq!(p.to_string(), "jira");
    }

    // Compact search-line contract.
    #[test]
    fn compact_line_matches_reference_format() {
        let t = Ticket {
            provider: ProviderName::from("redmine"),
            external_key: "42".into(),
            title: "Fix startup".into(),
            status: "New".into(),
            ..Ticket::default()
        };
        assert_eq!(t.compact_line(), "[redmine:42] Fix startup (New)");
    }

    // Wire-shape spot checks: `type`/`provider_fields` tags and omitempty.
    #[test]
    fn ticket_json_tags_match_reference() {
        let t = Ticket {
            provider: ProviderName::from("jira"),
            external_id: "100".into(),
            external_key: "ABC-100".into(),
            issue_type: "Bug".into(),
            ..Ticket::default()
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["type"], "Bug");
        assert!(json.get("title").is_none(), "empty title omitted");
        assert!(json.get("provider_fields").is_none());
        let back: Ticket = serde_json::from_value(json).unwrap();
        assert_eq!(back, t);
    }
}
