//! Repopr contract types: provider names, the
//! `Provider` trait, params/results and the service `Config`.
//!
//! Serde shapes mirror the original json tags so the wave-4 tool handlers can emit
//! byte-compatible `PullRequest` JSON (only `provider`/`id` are always
//! present; `url`/`title`/`status` are omitted when empty, like `omitempty`).

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::RepoprError;

/// Identifies a repo-PR backend.
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

/// The only provider implemented day-1.
pub const PROVIDER_AZURE_DEVOPS: &str = "azure_devops";

/// Credentials handed to providers per call. In
/// the PAT arrived via a context value set by the HTTP auth middleware; in the
/// Rust port the service takes it as an explicit parameter (redmine-lane
/// precedent E-A13) and wave 4 builds it from
/// `ibis_core::auth::Credentials::azure_devops_pat`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthContext {
    pub azure_devops_pat: String,
}

/// Parameters for creating a pull request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateParams {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub origin_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repository: String,
    pub source_branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub target_branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ticket_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reviewer_ids: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub auto_complete: bool,
}

/// Parameters for completing/merging a pull request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompleteParams {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repository: String,
    pub pr_id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub delete_source_branch: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub squash: bool,
}

/// Create/complete result; this is the JSON the
/// `repo_pr_*` MCP tools return.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub provider: ProviderName,
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
}

/// Backend for the PR service.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> ProviderName;

    async fn create_pr(
        &self,
        auth: &AuthContext,
        params: &CreateParams,
    ) -> Result<PullRequest, RepoprError>;

    async fn complete_pr(
        &self,
        auth: &AuthContext,
        params: &CompleteParams,
    ) -> Result<PullRequest, RepoprError>;
}

/// Service configuration. Built by wave 4 from the
/// ticketing config (`ticketCfg.DefaultProvider`, `ticketCfg.ProjectProviderMap`
/// and `ticketCfg.PR.{DefaultTargetBranch,ProjectTargetBranch}` —
/// ).
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub default_provider: ProviderName,
    pub project_provider_map: HashMap<String, ProviderName>,
    pub default_target_branch: String,
    pub project_target_branch: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_request_json_matches_reference_tags() {
        let pr = PullRequest {
            provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            id: "88".into(),
            url: "http://ado/pr/88".into(),
            title: "My PR".into(),
            status: "active".into(),
        };
        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "provider": "azure_devops",
                "id": "88",
                "url": "http://ado/pr/88",
                "title": "My PR",
                "status": "active",
            })
        );
        // omitempty fields disappear when empty
        let pr = PullRequest {
            provider: ProviderName::from(PROVIDER_AZURE_DEVOPS),
            id: "10".into(),
            ..PullRequest::default()
        };
        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"provider": "azure_devops", "id": "10"})
        );
    }

    #[test]
    fn create_params_json_matches_reference_tags() {
        let params = CreateParams {
            source_branch: "feature/x".into(),
            ..CreateParams::default()
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json, serde_json::json!({"source_branch": "feature/x"}));
        let params = CreateParams {
            provider: "azure_devops".into(),
            source_branch: "b".into(),
            ticket_ids: vec!["ABC-1".into()],
            auto_complete: true,
            ..CreateParams::default()
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "provider": "azure_devops",
                "source_branch": "b",
                "ticket_ids": ["ABC-1"],
                "auto_complete": true,
            })
        );
    }

    #[test]
    fn complete_params_json_matches_reference_tags() {
        let params = CompleteParams {
            pr_id: "88".into(),
            ..CompleteParams::default()
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json, serde_json::json!({"pr_id": "88"}));
        let params = CompleteParams {
            provider: "azure_devops".into(),
            pr_id: "88".into(),
            delete_source_branch: true,
            squash: true,
            ..CompleteParams::default()
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "provider": "azure_devops",
                "pr_id": "88",
                "delete_source_branch": true,
                "squash": true,
            })
        );
    }

    #[test]
    fn provider_name_roundtrip() {
        let p = ProviderName::from("azure_devops");
        assert_eq!(p.as_str(), "azure_devops");
        assert!(!p.is_empty());
        assert!(ProviderName::default().is_empty());
        assert_eq!(p.to_string(), "azure_devops");
        assert_eq!(
            ProviderName::from(String::from("jira")),
            ProviderName::from("jira")
        );
    }
}
