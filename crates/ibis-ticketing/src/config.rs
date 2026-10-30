//! Ticketing configuration.
//!
//! Wire shape matches `config/ticketing_config.json` (example carried in the
//! repo `config/ticketing_config.example.json`). Deserialization overlays
//! onto the defaults — a JSON file only overrides the fields it contains
//! (`json.Decode into a populated struct, LA-06 pattern).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{ProviderName, PROVIDER_AZURE_DEVOPS, PROVIDER_JIRA, PROVIDER_REDMINE};

/// Config-load failures (raw `os.Open` errors + `decode ticketing config:
/// %w` wrap).
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("decode ticketing config: {0}")]
    Decode(String),
}

/// Root config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_provider: ProviderName,
    pub project_provider_map: BTreeMap<String, ProviderName>,
    pub providers: ProviderConfigs,
    pub workflow: WorkflowConfig,
    pub reference_patterns: BTreeMap<String, Vec<String>>,
    pub default_create_type: BTreeMap<String, BTreeMap<String, String>>,
    pub pr: PRConfig,
    pub header_hints: BTreeMap<String, Vec<String>>,
    pub client_env_hints: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        let mut header_hints = BTreeMap::new();
        header_hints.insert(
            PROVIDER_REDMINE.to_string(),
            vec!["X-Redmine-API-Key".to_string()],
        );
        header_hints.insert(
            PROVIDER_JIRA.to_string(),
            vec!["X-Jira-Email".to_string(), "X-Jira-API-Token".to_string()],
        );
        header_hints.insert(
            PROVIDER_AZURE_DEVOPS.to_string(),
            vec!["X-Azure-DevOps-PAT".to_string()],
        );
        Config {
            default_provider: ProviderName::from(PROVIDER_REDMINE),
            project_provider_map: BTreeMap::new(),
            providers: ProviderConfigs::default(),
            workflow: WorkflowConfig::default(),
            reference_patterns: BTreeMap::new(),
            default_create_type: BTreeMap::new(),
            pr: PRConfig::default(),
            header_hints,
            client_env_hints: BTreeMap::new(),
        }
    }
}

/// Per-provider connection settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfigs {
    pub redmine: RedmineConfig,
    pub jira: JiraConfig,
    pub azure_devops: AzureDevOpsConfig,
}

/// Redmine connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RedmineConfig {
    #[serde(rename = "base_url")]
    pub base_url: String,
    #[serde(rename = "api_key")]
    pub api_key: String,
}

/// Jira connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JiraConfig {
    #[serde(rename = "base_url")]
    pub base_url: String,
    pub email: String,
    #[serde(rename = "api_token")]
    pub api_token: String,
}

/// Azure DevOps connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AzureDevOpsConfig {
    #[serde(rename = "organization_url")]
    pub organization_url: String,
    pub project: String,
    pub repository: String,
    pub pat: String,
}

/// Workflow overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowConfig {
    pub actions: BTreeMap<String, WorkflowActionConfig>,
}

/// One workflow action: explicit target
/// per provider, per project (`"*"` wildcard).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowActionConfig {
    pub per_provider: BTreeMap<String, BTreeMap<String, String>>,
}

/// Repo-PR branch defaults (`ticketing.PRConfig; consumed by the repopr
/// lane through the server wiring).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PRConfig {
    #[serde(rename = "default_target_branch")]
    pub default_target_branch: String,
    #[serde(rename = "project_target_branch")]
    pub project_target_branch: BTreeMap<String, String>,
}

impl Config {
    /// Normalizes the config: redmine default, and
    /// trimmed/lowercased keys and provider names. Unlikewise — which mutated
    /// the maps in place and left stale untrimmed keys behind (
    /// TI-05) — the maps are rebuilt, dropping stale entries.
    pub fn normalize(&mut self) {
        if self.default_provider.is_empty() {
            self.default_provider = ProviderName::from(PROVIDER_REDMINE);
        }

        self.project_provider_map = self
            .project_provider_map
            .iter()
            .map(|(k, v)| {
                (
                    k.trim().to_string(),
                    ProviderName::from(v.0.trim().to_lowercase()),
                )
            })
            .collect();

        self.workflow.actions = self
            .workflow
            .actions
            .iter()
            .map(|(k, v)| (normalize_workflow_action(k), v.clone()))
            .collect();
    }
}

/// Baseline configuration used when no runtime file is available
///
pub fn default_config() -> Config {
    let mut cfg = Config::default();
    cfg.normalize();
    cfg
}

/// Loads the ticketing config from `config/ticketing_config.json` relative to
/// the process working directory (`LoadConfig; CWD-dependence noted on the
/// TI-06 — the server lane decides the final path).
pub fn load_config() -> Result<Config, ConfigLoadError> {
    load_config_from_path(Path::new("config").join("ticketing_config.json"))
}

/// Loads and decodes a ticketing config file.
pub fn load_config_from_path(path: impl AsRef<Path>) -> Result<Config, ConfigLoadError> {
    let bytes = std::fs::read(path)?;
    let mut cfg: Config =
        serde_json::from_slice(&bytes).map_err(|e| ConfigLoadError::Decode(e.to_string()))?;
    cfg.normalize();
    Ok(cfg)
}

/// Canonicalizes a workflow action name.
pub fn normalize_workflow_action(action: &str) -> String {
    let action = action.trim().to_lowercase();
    match action.as_str() {
        "resolved" => crate::types::WORKFLOW_RESOLVE.to_string(),
        "closed" => crate::types::WORKFLOW_CLOSE.to_string(),
        "reopened" => crate::types::WORKFLOW_REOPEN.to_string(),
        _ => action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_reference() {
        let cfg = default_config();
        assert_eq!(cfg.default_provider.as_str(), "redmine");
        assert!(cfg.project_provider_map.is_empty());
        assert_eq!(
            cfg.header_hints.get("redmine").map(Vec::as_slice),
            Some(&["X-Redmine-API-Key".to_string()][..])
        );
        assert_eq!(
            cfg.header_hints.get("jira").map(Vec::as_slice),
            Some(&["X-Jira-Email".to_string(), "X-Jira-API-Token".to_string()][..])
        );
        assert_eq!(
            cfg.header_hints.get("azure_devops").map(Vec::as_slice),
            Some(&["X-Azure-DevOps-PAT".to_string()][..])
        );
        assert!(cfg.client_env_hints.is_empty());
    }

    // Scenario: the original decode-overlay semantics + normalize.
    #[test]
    fn decode_overlays_defaults_and_normalizes() {
        let raw = r#"{
            "default_provider": "",
            "project_provider_map": { " core ": " JIRA " },
            "workflow": { "actions": { " RESOLVED ": { "per_provider": {} } } }
        }"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.normalize();
        // Empty default falls back to redmine.
        assert_eq!(cfg.default_provider.as_str(), "redmine");
        // Provider values are trimmed + lowercased.
        assert_eq!(
            cfg.project_provider_map.get("core").unwrap().as_str(),
            "jira"
        );
        // Workflow action keys are canonicalized; "resolved" → "resolve".
        assert!(cfg.workflow.actions.contains_key("resolve"));
        assert!(!cfg.workflow.actions.contains_key(" RESOLVED "));
        // Absent fields keep defaults (decode-overlay).
        assert_eq!(
            cfg.header_hints.get("redmine").map(Vec::as_slice),
            Some(&["X-Redmine-API-Key".to_string()][..])
        );
    }

    // TI-05: normalization rebuilds the map — no stale untrimmed
    // keys ( left ` project-a` behind next to `project-a`). The surviving
    // value is order-dependent ( ranged a map too), so only the collapse
    // is asserted.
    #[test]
    fn normalize_drops_stale_untrimmed_keys() {
        let raw = r#"{ "project_provider_map": { "core": "jira", "  core ": "redmine" } }"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.normalize();
        assert_eq!(cfg.project_provider_map.len(), 1);
        let value = cfg.project_provider_map.get("core").unwrap();
        assert!(
            value.as_str() == "jira" || value.as_str() == "redmine",
            "{value}"
        );
    }

    #[test]
    fn normalize_workflow_action_aliases() {
        assert_eq!(normalize_workflow_action(" Resolved "), "resolve");
        assert_eq!(normalize_workflow_action("CLOSED"), "close");
        assert_eq!(normalize_workflow_action("Reopened"), "reopen");
        assert_eq!(normalize_workflow_action("custom"), "custom");
    }

    #[tokio::test]
    async fn load_config_from_path_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ibis_ticket_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ticketing_config.json");
        std::fs::write(
            &path,
            r#"{ "default_provider": "jira", "reference_patterns": { "*": ["jira::([A-Z]+-\\d+)"] } }"#,
        )
        .unwrap();
        let cfg = load_config_from_path(&path).unwrap();
        assert_eq!(cfg.default_provider.as_str(), "jira");
        assert_eq!(
            cfg.reference_patterns.get("*").map(Vec::as_slice),
            Some(&["jira::([A-Z]+-\\d+)".to_string()][..])
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_config_missing_file_errors() {
        assert!(load_config_from_path("nonexistent/ticketing_config.json").is_err());
    }
}
