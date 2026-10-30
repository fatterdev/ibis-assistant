//! Provider-agnostic ticketing service.
//!
//! Resolves the target provider per call (explicit arg → project map →
//! default), applies config-driven workflow targets with a multilingual
//! status-name heuristic fallback, and implements the [`Ingester`] boundary
//! the git history ingest consumes.
//!
//! Parity / D17 notes:
//! - **`upsert_issue_graph` is one explicit transaction with `UPSERT`-first
//!   rows** (TI-01/TI-02): built a multi-statement `UPDATE`-based
//!   query that silently no-ops on missing records (SurrealDB 3.x semantics,
//!   R1) and `RELATE`d edges to records that never existed (R4). The
//!   Rust shape mirrors the live-verified `ibis-ingest-redmine` ingest.
//! - **All ticket content is bound** (F-002); only record identifiers are
//!   inlined via [`ibis_db::ids::format_record_id`].
//! - Credentials are an explicit [`AuthContext`] argument ( read context
//!   values; no ambient context in Rust, E-A13 pattern).
//! - Provider registration order in [`Service::capabilities`] is deterministic
//!   ( ranged a map, TI-04).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::Serialize;

use ibis_db::executor::{Executor, Vars};
use ibis_db::ids::{format_record_id, sanitize_id};
use ibis_db::schema::names::{AUTHOR, AUTHORED, PART_OF, TRACKER};
use ibis_db::value::Value;

use crate::config::Config;
use crate::parse::parse_provider_fields_json;
use crate::types::{
    AuthContext, CreateParams, Ingester, IngesterError, IssueReference, Project, ProviderName,
    SearchParams, SearchResult, Status, Ticket, TicketProvider, TicketingError, UpdateParams, User,
    WorkflowActionResult, WORKFLOW_CLOSE, WORKFLOW_REOPEN, WORKFLOW_RESOLVE,
};

/// Multilingual status-name keywords per workflow action.
const RESOLVE_NAMES: &[&str] = &[
    // EN
    "resolved",
    "done",
    "fixed",
    "complete",
    "completed",
    "finished",
    "closed as resolved",
    // IT
    "risolto",
    "completato",
    "terminato",
    // FR
    "résolu",
    "terminé",
    "fait",
    // DE
    "gelöst",
    "erledigt",
    "abgeschlossen",
    // ES
    "resuelto",
    "completado",
    "terminado",
    // PT
    "resolvido",
    "concluído",
];

const CLOSE_NAMES: &[&str] = &[
    // EN
    "closed",
    "wont fix",
    "won't fix",
    "invalid",
    "duplicate",
    "rejected",
    // IT
    "chiuso",
    "rifiutato",
    "duplicato",
    // FR
    "fermé",
    "rejeté",
    // DE
    "geschlossen",
    "abgelehnt",
    // ES
    "cerrado",
    "rechazado",
    // PT
    "fechado",
    "rejeitado",
];

const REOPEN_NAMES: &[&str] = &[
    // EN
    "reopened",
    "open",
    "new",
    "in progress",
    "reopen",
    // IT
    "riaperto",
    "aperto",
    "nuovo",
    // FR
    "rouvert",
    "ouvert",
    "nouveau",
    // DE
    "wiedereröffnet",
    "offen",
    "neu",
    // ES
    "reabierto",
    "abierto",
    "nuevo",
    // PT
    "reaberto",
    "aberto",
    "novo",
];

/// Capability snapshot returned to the MCP client.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Capabilities {
    pub default_provider: String,
    pub project_provider_map: BTreeMap<String, String>,
    /// Registered providers, sorted for deterministic output (TI-04).
    pub providers: Vec<String>,
    pub header_hints: BTreeMap<String, Vec<String>>,
    pub client_env_hints: BTreeMap<String, String>,
}

/// The config + provider set, shared behind one lock so the F0.6 hot reload
/// can swap them atomically while in-flight tool calls clone the provider
/// handles they need.
struct Inner {
    cfg: Config,
    providers: BTreeMap<String, Arc<dyn TicketProvider>>,
}

/// The provider-agnostic ticketing service.
///
/// Interior mutability (F0.6): the config/providers pair is swappable at
/// runtime (`reload`) so a live server can pick up
/// `config/ticketing_config.json` changes without a restart — the dynamic
/// tool discovery gate (`capabilities().providers`) then flips and the MCP
/// surface announces/hides the `ticket_*` group accordingly.
pub struct Service {
    inner: RwLock<Inner>,
    db: Option<Arc<dyn Executor>>,
}

impl Service {
    /// Builds the service; the config is normalized in place
    ///
    pub fn new(mut cfg: Config, db: Option<Arc<dyn Executor>>) -> Self {
        cfg.normalize();
        Service {
            inner: RwLock::new(Inner {
                cfg,
                providers: BTreeMap::new(),
            }),
            db,
        }
    }

    /// Registers a provider under its name; `None` arguments are ignored
    /// (`RegisterProvider nil check).
    pub fn register_provider(&self, provider: Arc<dyn TicketProvider>) {
        self.inner
            .write()
            .unwrap()
            .providers
            .insert(provider.name().0.to_lowercase(), provider);
    }

    /// F0.6 hot reload: atomically swaps the live config and provider set
    /// (same normalization and name-keying as [`Service::new`] +
    /// [`Service::register_provider`]). In-flight calls keep the provider
    /// handles they cloned.
    pub fn reload(&self, mut cfg: Config, providers: Vec<Arc<dyn TicketProvider>>) {
        cfg.normalize();
        let providers: BTreeMap<String, Arc<dyn TicketProvider>> = providers
            .into_iter()
            .map(|p| (p.name().0.to_lowercase(), p))
            .collect();
        let mut inner = self.inner.write().unwrap();
        inner.providers = providers;
        inner.cfg = cfg;
    }

    /// The live (normalized) configuration.
    pub fn config(&self) -> Config {
        self.inner.read().unwrap().cfg.clone()
    }

    /// Capability snapshot for the `ticket_get_capabilities` tool.
    pub fn capabilities(&self) -> Capabilities {
        let inner = self.inner.read().unwrap();
        Capabilities {
            default_provider: inner.cfg.default_provider.0.clone(),
            project_provider_map: inner
                .cfg
                .project_provider_map
                .iter()
                .map(|(k, v)| (k.clone(), v.0.clone()))
                .collect(),
            providers: inner.providers.keys().cloned().collect(),
            header_hints: inner.cfg.header_hints.clone(),
            client_env_hints: inner.cfg.client_env_hints.clone(),
        }
    }

    /// Reference patterns for a project, falling back to the `"*"` wildcard
    ///
    pub fn reference_patterns_for_project(&self, project_key: &str) -> Vec<String> {
        let inner = self.inner.read().unwrap();
        if let Some(patterns) = inner.cfg.reference_patterns.get(project_key) {
            return patterns.clone();
        }
        if let Some(patterns) = inner.cfg.reference_patterns.get("*") {
            return patterns.clone();
        }
        Vec::new()
    }

    /// Provider resolution order: explicit arg →
    /// project map → default.
    fn resolve_provider(
        &self,
        provider_arg: &str,
        project_key: &str,
    ) -> Result<(ProviderName, Arc<dyn TicketProvider>), TicketingError> {
        let inner = self.inner.read().unwrap();
        if !provider_arg.is_empty() {
            let name = ProviderName::from(provider_arg.trim().to_lowercase());
            let provider = inner
                .providers
                .get(name.as_str())
                .ok_or_else(|| TicketingError::UnknownProvider(name.0.clone()))?;
            return Ok((name, provider.clone()));
        }

        if let Some(mapped) = inner.cfg.project_provider_map.get(project_key) {
            if !mapped.is_empty() {
                let provider = inner
                    .providers
                    .get(mapped.as_str())
                    .ok_or_else(|| TicketingError::UnknownProvider(mapped.0.clone()))?;
                return Ok((mapped.clone(), provider.clone()));
            }
        }

        let name = inner.cfg.default_provider.clone();
        let provider = inner
            .providers
            .get(name.as_str())
            .ok_or_else(|| TicketingError::UnknownProvider(name.0.clone()))?;
        Ok((name, provider.clone()))
    }

    /// Provider-wide search.
    pub async fn search(
        &self,
        auth: &AuthContext,
        mut params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        let (name, provider) = self.resolve_provider(&params.provider, &params.project_key)?;
        params.provider = name.0;
        provider.search_issues(auth, params).await
    }

    /// My-issues search.
    pub async fn search_my(
        &self,
        auth: &AuthContext,
        mut params: SearchParams,
    ) -> Result<SearchResult, TicketingError> {
        let (name, provider) = self.resolve_provider(&params.provider, &params.project_key)?;
        params.provider = name.0;
        provider.search_my_issues(auth, params).await
    }

    /// Fetches one ticket.
    pub async fn get(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
    ) -> Result<Option<Ticket>, TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider.get_issue(auth, id, project_key).await
    }

    /// Creates a ticket: merges `provider_fields_json`
    /// into `provider_fields` and applies the per-provider/per-project default
    /// create type (`"*"` wildcard).
    pub async fn create(
        &self,
        auth: &AuthContext,
        mut params: CreateParams,
    ) -> Result<Ticket, TicketingError> {
        let (name, provider) = self.resolve_provider(&params.provider, &params.project_key)?;
        merge_provider_fields_json(&mut params.provider_fields, &params.provider_fields_json)?;
        if params.issue_type.is_empty() {
            let default_create_type = self.inner.read().unwrap().cfg.default_create_type.clone();
            if let Some(by_project) = default_create_type.get(name.as_str()) {
                if let Some(t) = by_project
                    .get(&params.project_key)
                    .filter(|t| !t.is_empty())
                {
                    params.issue_type = t.clone();
                } else if let Some(t) = by_project.get("*") {
                    params.issue_type = t.clone();
                }
            }
        }
        provider.create_issue(auth, params).await
    }

    /// Updates a ticket: a `workflow_action` first
    /// resolves and applies a transition, then the provider update runs.
    pub async fn update(
        &self,
        auth: &AuthContext,
        id: &str,
        project_key: &str,
        mut params: UpdateParams,
    ) -> Result<Ticket, TicketingError> {
        let (name, provider) = self.resolve_provider(&params.provider, project_key)?;
        merge_provider_fields_json(&mut params.provider_fields, &params.provider_fields_json)?;

        if !params.workflow_action.is_empty() {
            let target = self
                .resolve_workflow_target(
                    auth,
                    &name,
                    &params.workflow_action,
                    project_key,
                    &params.issue_type,
                )
                .await?;
            if !target.is_empty() {
                provider
                    .transition_issue(auth, id, project_key, &target)
                    .await?;
            }
        }

        provider.update_issue(auth, id, project_key, params).await
    }

    /// Adds a comment.
    pub async fn add_comment(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        comment: &str,
    ) -> Result<(), TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider.add_comment(auth, id, project_key, comment).await
    }

    /// Assigns a ticket.
    pub async fn assign(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        assignee: &str,
    ) -> Result<(), TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider.assign_issue(auth, id, project_key, assignee).await
    }

    /// Applies a raw transition.
    pub async fn transition(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        transition: &str,
    ) -> Result<(), TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider
            .transition_issue(auth, id, project_key, transition)
            .await
    }

    /// Lists statuses.
    pub async fn list_statuses(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        project_key: &str,
        issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider.list_statuses(auth, project_key, issue_type).await
    }

    /// Searches users.
    pub async fn search_users(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        project_key: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<User>, TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, project_key)?;
        provider.search_users(auth, query, limit).await
    }

    /// Lists projects.
    pub async fn list_projects(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
    ) -> Result<Vec<Project>, TicketingError> {
        let (_, provider) = self.resolve_provider(provider_arg, "")?;
        provider.list_projects(auth).await
    }

    /// Workflow: mark resolved.
    pub async fn mark_resolved(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        issue_type: &str,
    ) -> Result<WorkflowActionResult, TicketingError> {
        self.apply_workflow(
            auth,
            provider_arg,
            id,
            project_key,
            issue_type,
            WORKFLOW_RESOLVE,
        )
        .await
    }

    /// Workflow: mark closed.
    pub async fn mark_closed(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        issue_type: &str,
    ) -> Result<WorkflowActionResult, TicketingError> {
        self.apply_workflow(
            auth,
            provider_arg,
            id,
            project_key,
            issue_type,
            WORKFLOW_CLOSE,
        )
        .await
    }

    /// Workflow: reopen.
    pub async fn reopen(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        issue_type: &str,
    ) -> Result<WorkflowActionResult, TicketingError> {
        self.apply_workflow(
            auth,
            provider_arg,
            id,
            project_key,
            issue_type,
            WORKFLOW_REOPEN,
        )
        .await
    }

    async fn apply_workflow(
        &self,
        auth: &AuthContext,
        provider_arg: &str,
        id: &str,
        project_key: &str,
        issue_type: &str,
        action: &str,
    ) -> Result<WorkflowActionResult, TicketingError> {
        let (name, provider) = self.resolve_provider(provider_arg, project_key)?;
        let target = self
            .resolve_workflow_target(auth, &name, action, project_key, issue_type)
            .await?;
        if target.is_empty() {
            return Err(TicketingError::NoWorkflowTarget(action.to_string()));
        }
        provider
            .transition_issue(auth, id, project_key, &target)
            .await?;
        Ok(WorkflowActionResult {
            provider: name,
            action: action.to_string(),
            target,
            issue_id: id.to_string(),
        })
    }

    /// Workflow target resolution: config
    /// per-provider override (`project_key` then `"*"`), else the
    /// status-name heuristic over the provider's live status list.
    async fn resolve_workflow_target(
        &self,
        auth: &AuthContext,
        provider: &ProviderName,
        action: &str,
        project_key: &str,
        issue_type: &str,
    ) -> Result<String, TicketingError> {
        let action = crate::config::normalize_workflow_action(action);
        {
            let inner = self.inner.read().unwrap();
            if let Some(cfg) = inner.cfg.workflow.actions.get(&action) {
                if let Some(by_project) = cfg.per_provider.get(provider.as_str()) {
                    if let Some(target) = by_project.get(project_key).map(String::as_str) {
                        if !target.trim().is_empty() {
                            return Ok(target.trim().to_string());
                        }
                    }
                    if let Some(target) = by_project.get("*").map(String::as_str) {
                        if !target.trim().is_empty() {
                            return Ok(target.trim().to_string());
                        }
                    }
                }
            }
        }

        let provider = {
            let inner = self.inner.read().unwrap();
            inner
                .providers
                .get(provider.as_str())
                .ok_or_else(|| TicketingError::UnknownProvider(provider.0.clone()))?
                .clone()
        };
        let statuses = provider
            .list_statuses(auth, project_key, issue_type)
            .await?;
        let target = infer_status_id(&action, &statuses);
        if target.is_empty() {
            return Err(TicketingError::UnresolvedWorkflowTarget);
        }
        Ok(target)
    }

    /// Fills unset reference fields:
    /// external_key ← external_id, external_id ← first digit run of the key,
    /// provider ← resolved default, project_key ← the ingest project.
    pub fn resolve_reference(
        &self,
        project_key: &str,
        mut resolved: IssueReference,
    ) -> Result<IssueReference, TicketingError> {
        if resolved.external_key.trim().is_empty() {
            resolved.external_key = resolved.external_id.trim().to_string();
        }
        if resolved.external_id.trim().is_empty() {
            resolved.external_id = extract_digits(&resolved.external_key);
            if resolved.external_id.is_empty() {
                resolved.external_id = resolved.external_key.trim().to_string();
            }
        }
        if resolved.provider.is_empty() {
            let (provider_name, _) = self.resolve_provider("", project_key)?;
            resolved.provider = provider_name;
        }
        if resolved.project_key.is_empty() {
            resolved.project_key = project_key.to_string();
        }
        Ok(resolved)
    }

    /// Fetches the referenced issue and upserts it into the graph
    /// a 404 is a successful no-op.
    pub async fn ingest_issue_reference(
        &self,
        auth: &AuthContext,
        project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<(), TicketingError> {
        let resolved = self.resolve_reference(project_key, issue_ref)?;
        let (_, provider) = self.resolve_provider(resolved.provider.as_str(), project_key)?;
        let Some(mut issue) = provider
            .get_issue(auth, &resolved.external_key, &resolved.project_key)
            .await?
        else {
            return Ok(());
        };
        if issue.provider.is_empty() {
            issue.provider = resolved.provider.clone();
        }
        if issue.external_id.is_empty() {
            issue.external_id = resolved.external_id.clone();
        }
        if issue.external_key.is_empty() {
            issue.external_key = resolved.external_key.clone();
        }
        if issue.project_key.is_empty() {
            issue.project_key = resolved.project_key.clone();
        }
        self.upsert_issue_graph(&issue).await?;
        tracing::debug!(
            "Ingested ticket reference provider={} id={}",
            issue.provider,
            issue.external_id
        );
        Ok(())
    }

    /// Upserts the ticket into the knowledge graph (`upsertIssueGraph,
    /// TI-01/TI-02): one transaction, `UPSERT`-first rows, edges
    /// `RELATE`d after their endpoints are persisted (D17 R1/R4/R5 shape).
    /// No-op without a DB handle.
    async fn upsert_issue_graph(&self, issue: &Ticket) -> Result<(), TicketingError> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };

        let mut external_key = issue.external_key.trim().to_string();
        if external_key.is_empty() {
            external_key = issue.external_id.trim().to_string();
        }
        if external_key.is_empty() {
            external_key = "unknown".to_string();
        }

        let issue_id = crate::issue_record_id(issue.provider.as_str(), &external_key);
        let tracker_name = issue.issue_type.trim().to_string();
        let tracker_id = if tracker_name.is_empty() {
            String::new()
        } else {
            format_record_id(
                TRACKER,
                &sanitize_id(&format!("{}_{}", issue.provider, tracker_name)),
            )
        };
        let author_name = issue.author.trim().to_string();
        let author_id = if author_name.is_empty() {
            String::new()
        } else {
            format_record_id(AUTHOR, &sanitize_id(&author_name))
        };

        let mut sql = String::from("BEGIN TRANSACTION;\n");
        if !tracker_id.is_empty() {
            sql.push_str(&format!("UPSERT {tracker_id} SET name = $tracker_name;\n"));
        }
        if !author_id.is_empty() {
            sql.push_str(&format!("UPSERT {author_id} SET name = $author_name;\n"));
        }
        sql.push_str(&format!(
            "UPSERT {issue_id} SET provider = $provider, external_id = $external_id, \
             external_key = $external_key, project_key = $project_key, subject = $subject, \
             description = $description, status = $status, type = $type, \
             updated_on = $updated_on, url = $url;\n"
        ));
        if !tracker_id.is_empty() {
            sql.push_str(&format!("RELATE {issue_id}->{PART_OF}->{tracker_id};\n"));
        }
        if !author_id.is_empty() {
            sql.push_str(&format!("RELATE {author_id}->{AUTHORED}->{issue_id};\n"));
        }
        sql.push_str("COMMIT TRANSACTION;");

        let mut vars = Vars::new();
        vars.insert("provider".into(), Value::from_t(issue.provider.0.clone()));
        vars.insert(
            "external_id".into(),
            Value::from_t(issue.external_id.clone()),
        );
        vars.insert("external_key".into(), Value::from_t(external_key));
        vars.insert(
            "project_key".into(),
            Value::from_t(issue.project_key.clone()),
        );
        vars.insert("subject".into(), Value::from_t(issue.title.clone()));
        vars.insert(
            "description".into(),
            Value::from_t(issue.description.clone()),
        );
        vars.insert("status".into(), Value::from_t(issue.status.clone()));
        vars.insert("type".into(), Value::from_t(issue.issue_type.clone()));
        vars.insert("updated_on".into(), Value::from_t(issue.updated_on.clone()));
        vars.insert("url".into(), Value::from_t(issue.url.clone()));
        vars.insert(
            "tracker_name".into(),
            Value::from_t(issue.issue_type.clone()),
        );
        vars.insert("author_name".into(), Value::from_t(issue.author.clone()));

        db.smart_query(&sql, vars).await?;
        Ok(())
    }
}

/// `Ingester` boundary for the git history ingest (`Service satisfies
/// `ticketing.Ingester`): typed errors degrade to the opaque string error the
/// already-implemented consumer expects.
#[async_trait]
impl Ingester for Service {
    async fn resolve_reference(
        &self,
        project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<IssueReference, IngesterError> {
        Service::resolve_reference(self, project_key, issue_ref)
            .map_err(|e| IngesterError(e.to_string()))
    }

    async fn ingest_issue_reference(
        &self,
        project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<(), IngesterError> {
        // The git ingest runs reference resolution off the request path —
        // credentials come from the service config, no per-request auth.
        Service::ingest_issue_reference(self, &AuthContext::default(), project_key, issue_ref)
            .await
            .map_err(|e| IngesterError(e.to_string()))
    }
}

/// Merges `provider_fields_json` into the provider fields map
/// ( inline in `Create`/`Update`).
fn merge_provider_fields_json(
    fields: &mut Option<serde_json::Map<String, serde_json::Value>>,
    raw: &str,
) -> Result<(), TicketingError> {
    if raw.is_empty() {
        return Ok(());
    }
    let extra = parse_provider_fields_json(raw)?;
    fields
        .get_or_insert_with(serde_json::Map::new)
        .extend(extra);
    Ok(())
}

/// Status-name heuristic: first status whose lowercased
/// name is in the action's keyword set; empty when nothing matches.
fn infer_status_id(action: &str, statuses: &[Status]) -> String {
    let keywords: &[&str] = match action {
        WORKFLOW_RESOLVE => RESOLVE_NAMES,
        WORKFLOW_CLOSE => CLOSE_NAMES,
        WORKFLOW_REOPEN => REOPEN_NAMES,
        _ => &[],
    };
    for status in statuses {
        let name = status.name.trim().to_lowercase();
        if keywords.contains(&name.as_str()) {
            return status.id.clone();
        }
    }
    String::new()
}

/// First digit run of the input.
fn extract_digits(input: &str) -> String {
    let start = input.find(|c: char| c.is_ascii_digit());
    let Some(start) = start else {
        return String::new();
    };
    let rest = &input[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PROVIDER_JIRA;
    use ibis_db::executor::QueryOutcome;
    use serde_json::json;
    use std::sync::Mutex;

    /// Recording fake provider.
    struct FakeProvider {
        name: ProviderName,
        transitions: Mutex<Vec<String>>,
        creates: Mutex<Vec<CreateParams>>,
    }

    impl FakeProvider {
        fn new(name: &str) -> Arc<Self> {
            Arc::new(FakeProvider {
                name: ProviderName::from(name),
                transitions: Mutex::new(Vec::new()),
                creates: Mutex::new(Vec::new()),
            })
        }

        fn transitions(&self) -> Vec<String> {
            self.transitions.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TicketProvider for FakeProvider {
        fn name(&self) -> ProviderName {
            self.name.clone()
        }

        async fn search_issues(
            &self,
            _auth: &AuthContext,
            _params: SearchParams,
        ) -> Result<SearchResult, TicketingError> {
            Ok(SearchResult::default())
        }

        async fn search_my_issues(
            &self,
            _auth: &AuthContext,
            _params: SearchParams,
        ) -> Result<SearchResult, TicketingError> {
            Ok(SearchResult::default())
        }

        async fn get_issue(
            &self,
            _auth: &AuthContext,
            id: &str,
            _project_key: &str,
        ) -> Result<Option<Ticket>, TicketingError> {
            Ok(Some(Ticket {
                provider: self.name.clone(),
                external_id: id.to_string(),
                external_key: id.to_string(),
                issue_type: "Bug".into(),
                author: "Dev".into(),
                ..Ticket::default()
            }))
        }

        async fn create_issue(
            &self,
            _auth: &AuthContext,
            params: CreateParams,
        ) -> Result<Ticket, TicketingError> {
            self.creates.lock().unwrap().push(params);
            Ok(Ticket {
                provider: self.name.clone(),
                external_id: "1".into(),
                external_key: "1".into(),
                ..Ticket::default()
            })
        }

        async fn update_issue(
            &self,
            _auth: &AuthContext,
            id: &str,
            _project_key: &str,
            _params: UpdateParams,
        ) -> Result<Ticket, TicketingError> {
            Ok(Ticket {
                provider: self.name.clone(),
                external_id: id.to_string(),
                external_key: id.to_string(),
                ..Ticket::default()
            })
        }

        async fn add_comment(
            &self,
            _auth: &AuthContext,
            _id: &str,
            _project_key: &str,
            _comment: &str,
        ) -> Result<(), TicketingError> {
            Ok(())
        }

        async fn assign_issue(
            &self,
            _auth: &AuthContext,
            _id: &str,
            _project_key: &str,
            _assignee: &str,
        ) -> Result<(), TicketingError> {
            Ok(())
        }

        async fn transition_issue(
            &self,
            _auth: &AuthContext,
            _id: &str,
            _project_key: &str,
            transition: &str,
        ) -> Result<(), TicketingError> {
            self.transitions
                .lock()
                .unwrap()
                .push(transition.to_string());
            Ok(())
        }

        async fn list_statuses(
            &self,
            _auth: &AuthContext,
            _project_key: &str,
            _issue_type: &str,
        ) -> Result<Vec<Status>, TicketingError> {
            Ok(vec![
                Status {
                    id: "2".into(),
                    name: "Resolved".into(),
                },
                Status {
                    id: "3".into(),
                    name: "Closed".into(),
                },
                Status {
                    id: "4".into(),
                    name: "Reopened".into(),
                },
            ])
        }

        async fn search_users(
            &self,
            _auth: &AuthContext,
            _query: &str,
            _limit: i64,
        ) -> Result<Vec<User>, TicketingError> {
            Ok(Vec::new())
        }

        async fn list_projects(&self, _auth: &AuthContext) -> Result<Vec<Project>, TicketingError> {
            Ok(Vec::new())
        }
    }

    /// Capturing fake `db.Executor` (`migrationMockDB pattern).
    #[derive(Default)]
    struct MockDb {
        captured: Mutex<Vec<(String, Vars)>>,
    }

    #[async_trait]
    impl Executor for MockDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, ibis_db::error::DbError> {
            self.captured
                .lock()
                .unwrap()
                .push((sql.to_string(), Vars::new()));
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(
            &self,
            sql: &str,
            vars: Vars,
        ) -> Result<QueryOutcome, ibis_db::error::DbError> {
            self.captured.lock().unwrap().push((sql.to_string(), vars));
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.project_provider_map
            .insert("project-a".to_string(), ProviderName::from(PROVIDER_JIRA));
        cfg
    }

    fn service() -> Service {
        let svc = Service::new(test_config(), None);
        svc.register_provider(FakeProvider::new(crate::types::PROVIDER_REDMINE));
        svc.register_provider(FakeProvider::new(PROVIDER_JIRA));
        svc.register_provider(FakeProvider::new(crate::types::PROVIDER_AZURE_DEVOPS));
        svc
    }

    // Scenario: TestResolveProviderPrecedence.
    #[tokio::test]
    async fn resolve_provider_precedence() {
        let svc = service();

        let (name, _) = svc
            .resolve_provider("azure_devops", "project-a")
            .expect("resolve with arg");
        assert_eq!(name.as_str(), "azure_devops");

        let (name, _) = svc.resolve_provider("", "project-a").expect("project map");
        assert_eq!(name.as_str(), "jira");

        let (name, _) = svc
            .resolve_provider("", "unknown-project")
            .expect("default");
        assert_eq!(name.as_str(), "redmine");
    }

    #[tokio::test]
    async fn resolve_provider_unregistered_errors() {
        let svc = service();
        let err = svc
            .resolve_provider("notaprovider", "p")
            .err()
            .expect("unregistered provider must fail");
        assert_eq!(err.to_string(), "provider 'notaprovider' is not registered");
    }

    // Scenario: TestResolveWorkflowTargetHybrid.
    #[tokio::test]
    async fn resolve_workflow_target_hybrid() {
        let mut cfg = test_config();
        let mut per_provider = BTreeMap::new();
        let mut by_project = BTreeMap::new();
        by_project.insert("*".to_string(), "3".to_string());
        per_provider.insert(crate::types::PROVIDER_REDMINE.to_string(), by_project);
        cfg.workflow.actions.insert(
            "resolve".to_string(),
            crate::config::WorkflowActionConfig { per_provider },
        );

        let svc = Service::new(cfg, None);
        svc.register_provider(FakeProvider::new(crate::types::PROVIDER_REDMINE));
        svc.register_provider(FakeProvider::new(PROVIDER_JIRA));

        // Explicit config target wins for redmine.
        let target = svc
            .resolve_workflow_target(
                &AuthContext::default(),
                &ProviderName::from(crate::types::PROVIDER_REDMINE),
                "resolved",
                "proj",
                "",
            )
            .await
            .expect("explicit target");
        assert_eq!(target, "3");

        // Jira falls back to the status-name heuristic (fake returns id 2
        // for "Resolved").
        let target = svc
            .resolve_workflow_target(
                &AuthContext::default(),
                &ProviderName::from(PROVIDER_JIRA),
                "resolved",
                "proj",
                "",
            )
            .await
            .expect("heuristic target");
        assert_eq!(target, "2");
    }

    #[tokio::test]
    async fn unresolved_workflow_target_errors() {
        let svc = Service::new(Config::default(), None);
        svc.register_provider(FakeProvider::new(crate::types::PROVIDER_REDMINE));
        // An action outside the resolve/close/reopen keyword sets matches no
        // status name → the original "unable to infer workflow status target" error.
        let err = svc
            .resolve_workflow_target(
                &AuthContext::default(),
                &ProviderName::from(crate::types::PROVIDER_REDMINE),
                "unknown-action",
                "proj",
                "",
            )
            .await
            .expect_err("unresolved target must fail");
        assert_eq!(err.to_string(), "unable to infer workflow status target");
    }

    #[tokio::test]
    async fn apply_workflow_transitions_and_reports() {
        let svc = service();
        let redmine = FakeProvider::new(crate::types::PROVIDER_REDMINE);
        svc.register_provider(redmine.clone());
        let result = svc
            .mark_resolved(&AuthContext::default(), "redmine", "42", "proj", "bug")
            .await
            .expect("workflow");
        assert_eq!(result.provider.as_str(), "redmine");
        assert_eq!(result.action, "resolve");
        // Heuristic picks the fake's "Resolved" status id 2.
        assert_eq!(result.target, "2");
        assert_eq!(result.issue_id, "42");
        assert_eq!(redmine.transitions(), vec!["2".to_string()]);
    }

    #[test]
    fn reference_patterns_for_project_fallback() {
        let mut cfg = test_config();
        cfg.reference_patterns
            .insert("*".to_string(), vec!["jira::([A-Z]+-\\d+)".to_string()]);
        cfg.reference_patterns.insert(
            "platform".to_string(),
            vec!["azure_devops::AB#(\\d+)".to_string()],
        );
        let svc = Service::new(cfg, None);
        assert_eq!(
            svc.reference_patterns_for_project("platform"),
            vec!["azure_devops::AB#(\\d+)".to_string()]
        );
        assert_eq!(
            svc.reference_patterns_for_project("other"),
            vec!["jira::([A-Z]+-\\d+)".to_string()]
        );
        // Empty project still falls back to the "*" wildcard (parity).
        assert_eq!(
            svc.reference_patterns_for_project(""),
            vec!["jira::([A-Z]+-\\d+)".to_string()]
        );
        // No wildcard → empty.
        let svc = Service::new(Config::default(), None);
        assert!(svc.reference_patterns_for_project("").is_empty());
    }

    // Reference-fill matrix.
    #[tokio::test]
    async fn resolve_reference_fill_matrix() {
        let svc = service();

        // key from id
        let r = svc
            .resolve_reference(
                "proj",
                IssueReference {
                    external_id: "42".into(),
                    ..IssueReference::default()
                },
            )
            .unwrap();
        assert_eq!(r.external_key, "42");
        assert_eq!(r.external_id, "42");
        assert_eq!(r.provider.as_str(), "redmine");
        assert_eq!(r.project_key, "proj");

        // id from first digit run of the key
        let r = svc
            .resolve_reference(
                "proj",
                IssueReference {
                    provider: ProviderName::from(PROVIDER_JIRA),
                    external_key: "ABC-321".into(),
                    ..IssueReference::default()
                },
            )
            .unwrap();
        assert_eq!(r.external_id, "321");
        assert_eq!(r.external_key, "ABC-321");
        assert_eq!(r.project_key, "proj");

        // no digits → id = key
        let r = svc
            .resolve_reference(
                "proj",
                IssueReference {
                    provider: ProviderName::from(PROVIDER_JIRA),
                    external_key: "KEYONLY".into(),
                    ..IssueReference::default()
                },
            )
            .unwrap();
        assert_eq!(r.external_id, "KEYONLY");
    }

    // Scenario: TestMigrateLegacyIssues-style mock assertions applied to
    // the graph upsert: one transaction, UPSERT-first rows, RELATE edges,
    // content bound (F-002), D17 shape (TI-01/TI-02).
    #[tokio::test]
    async fn ingest_issue_reference_upserts_graph_in_one_transaction() {
        let mock = Arc::new(MockDb::default());
        let svc = Service::new(test_config(), Some(mock.clone() as Arc<dyn Executor>));
        svc.register_provider(FakeProvider::new(crate::types::PROVIDER_REDMINE));

        svc.ingest_issue_reference(
            &AuthContext::default(),
            "proj",
            IssueReference {
                external_key: "42".into(),
                ..IssueReference::default()
            },
        )
        .await
        .expect("ingest");

        let entries = mock.captured.lock().unwrap().clone();
        assert_eq!(entries.len(), 1, "one combined query");
        let (sql, vars) = &entries[0];

        assert!(sql.starts_with("BEGIN TRANSACTION;\n"), "tx: {sql}");
        assert!(sql.trim_end().ends_with("COMMIT TRANSACTION;"));
        // UPSERT-first rows (never UPDATE — 3.x no-op, TI-01).
        assert!(
            sql.contains("UPSERT issue:42 SET provider = $provider"),
            "{sql}"
        );
        assert!(
            sql.contains("UPSERT tracker:⟨redmine_bug⟩ SET name = $tracker_name"),
            "{sql}"
        );
        assert!(
            sql.contains("UPSERT author:⟨dev⟩ SET name = $author_name"),
            "{sql}"
        );
        // Endpoints persisted before the RELATEs (TI-02).
        assert!(
            sql.contains("RELATE issue:42->part_of->tracker:⟨redmine_bug⟩;"),
            "{sql}"
        );
        assert!(
            sql.contains("RELATE author:⟨dev⟩->authored->issue:42;"),
            "{sql}"
        );

        let var_string = |name: &str| -> String {
            match vars.get(name) {
                Some(Value::String(s)) => s.clone(),
                other => panic!("var {name}: {other:?}"),
            }
        };
        assert_eq!(var_string("provider"), "redmine");
        assert_eq!(var_string("external_id"), "42");
        assert_eq!(var_string("external_key"), "42");
        assert_eq!(var_string("project_key"), "proj");
        assert_eq!(var_string("tracker_name"), "Bug");
        // No content interpolated into the SQL text (F-002).
        assert!(!sql.contains("'"), "content leaked into SQL: {sql}");
    }

    #[tokio::test]
    async fn graph_upsert_without_db_is_noop() {
        let svc = service();
        svc.upsert_issue_graph(&Ticket::default())
            .await
            .expect("nil db is a no-op");
    }

    #[tokio::test]
    async fn graph_upsert_skips_empty_tracker_and_author() {
        let mock = Arc::new(MockDb::default());
        let svc = Service::new(Config::default(), Some(mock.clone() as Arc<dyn Executor>));

        svc.upsert_issue_graph(&Ticket {
            provider: ProviderName::from("redmine"),
            external_key: "7".into(),
            ..Ticket::default()
        })
        .await
        .expect("upsert");

        let sql = &mock.captured.lock().unwrap()[0].0;
        assert!(!sql.contains("tracker:"), "no tracker row: {sql}");
        assert!(!sql.contains("author:"), "no author row: {sql}");
        assert!(!sql.contains("RELATE"), "no edges: {sql}");
    }

    // Provider-fields merge + default create type.
    #[tokio::test]
    async fn create_merges_fields_json_and_default_type() {
        let mut cfg = test_config();
        let mut by_project = BTreeMap::new();
        by_project.insert("*".to_string(), "Task".to_string());
        cfg.default_create_type
            .insert(PROVIDER_JIRA.to_string(), by_project);
        let svc = Service::new(cfg, None);
        let jira = FakeProvider::new(PROVIDER_JIRA);
        svc.register_provider(jira.clone());

        let params = CreateParams {
            provider: "jira".into(),
            project_key: "ABC".into(),
            title: "New bug".into(),
            provider_fields_json: r#"{"custom":"x"}"#.into(),
            ..CreateParams::default()
        };
        let created = svc.create(&AuthContext::default(), params).await.unwrap();
        assert_eq!(created.external_key, "1");

        let sent = jira.creates.lock().unwrap();
        let p = &sent[0];
        assert_eq!(p.issue_type, "Task", "default create type applied");
        assert_eq!(
            p.provider_fields.as_ref().unwrap().get("custom"),
            Some(&json!("x")),
            "provider_fields_json merged"
        );
    }

    // Per-project default create type overrides the wildcard ( precedence).
    #[tokio::test]
    async fn create_per_project_type_wins_over_wildcard() {
        let mut cfg = test_config();
        let mut by_project = BTreeMap::new();
        by_project.insert("project-a".to_string(), "Epic".to_string());
        by_project.insert("*".to_string(), "Task".to_string());
        cfg.default_create_type
            .insert(PROVIDER_JIRA.to_string(), by_project);
        let svc = Service::new(cfg, None);
        let jira = FakeProvider::new(PROVIDER_JIRA);
        svc.register_provider(jira.clone());

        svc.create(
            &AuthContext::default(),
            CreateParams {
                provider: "jira".into(),
                project_key: "project-a".into(),
                title: "x".into(),
                ..CreateParams::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(jira.creates.lock().unwrap()[0].issue_type, "Epic");
    }

    //: capabilities shape with deterministic provider order (TI-04).
    #[test]
    fn capabilities_shape_is_deterministic() {
        let svc = service();
        // Register in non-sorted order.
        svc.register_provider(FakeProvider::new("zeta"));
        let caps = svc.capabilities();
        assert_eq!(caps.default_provider, "redmine");
        assert_eq!(
            caps.providers,
            vec![
                "azure_devops".to_string(),
                "jira".to_string(),
                "redmine".to_string(),
                "zeta".to_string(),
            ]
        );
        assert_eq!(
            caps.project_provider_map.get("project-a"),
            Some(&"jira".to_string())
        );
        // Serializes as a JSON object for the MCP layer.
        let v = serde_json::to_value(&caps).unwrap();
        assert_eq!(
            v["providers"],
            json!(["azure_devops", "jira", "redmine", "zeta"])
        );
    }

    // The service satisfies the git-ingest boundary.
    #[tokio::test]
    async fn ingester_trait_delegates() {
        let svc = Arc::new(service());
        let ingester: Arc<dyn Ingester> = svc.clone();
        let resolved = ingester
            .resolve_reference(
                "project-a",
                IssueReference {
                    external_id: "42".into(),
                    ..IssueReference::default()
                },
            )
            .await
            .expect("resolve");
        assert_eq!(resolved.provider.as_str(), "jira");
        assert_eq!(resolved.project_key, "project-a");
    }

    // Scenario: digit extraction semantics ( extractDigits).
    #[test]
    fn extract_digits_first_run() {
        assert_eq!(extract_digits("ABC-321"), "321");
        assert_eq!(extract_digits("fix 2A-3 now"), "2");
        assert_eq!(extract_digits("no digits"), "");
        assert_eq!(extract_digits(""), "");
    }
}
