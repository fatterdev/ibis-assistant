//! Live SurrealDB regression test for the ticketing graph upsert (
//! TI-01/TI-02): issue/tracker/author rows are actually **created** (the
//! `UPDATE`-based statements silently no-op'd under SurrealDB 3.x) and the
//! edges resolve. Skips gracefully when the sidecar
//! (`ws://127.0.0.1:38000`, root/root) is unreachable.

use std::sync::Arc;

use async_trait::async_trait;
use threski_db::executor::{Executor, SurrealDbExecutor};
use threski_db::value::ToSql;
use threski_ticketing::types::{PROVIDER_JIRA, PROVIDER_REDMINE};
use threski_ticketing::{
    AuthContext, Config, CreateParams, IssueReference, Project, ProviderName, SearchParams,
    SearchResult, Service, Status, Ticket, TicketProvider, TicketingError, UpdateParams, User,
};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

async fn connect_scratch() -> Option<(SurrealDbExecutor, String)> {
    let url = std::env::var("THRESKI_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("lanei_{}", uuid::Uuid::new_v4().simple());
    let exec =
        match SurrealDbExecutor::connect(&url, "threski_db_tests", &db_name, "root", "root").await {
            Ok(exec) => exec,
            Err(err) => {
                eprintln!("live: SurrealDB not reachable ({err}), skipping");
                return None;
            }
        };
    Some((exec, db_name))
}

async fn cleanup(exec: &SurrealDbExecutor, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

/// Fake provider that returns a fully populated ticket (the graph upsert
/// needs type + author to exercise tracker/author rows and edges).
struct FakeTicket {
    name: &'static str,
}

#[async_trait]
impl TicketProvider for FakeTicket {
    fn name(&self) -> ProviderName {
        ProviderName::from(self.name)
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
            provider: ProviderName::from(self.name),
            id: id.to_string(),
            external_id: id.to_string(),
            external_key: id.to_string(),
            title: "Fix the thing \"with quotes\"".to_string(),
            description: "multi\nline\r\ndescription".to_string(),
            status: "New".to_string(),
            issue_type: "Bug".to_string(),
            author: "Jane Dœ".to_string(),
            updated_on: "2026-10-25T10:00:00Z".to_string(),
            ..Ticket::default()
        }))
    }

    async fn create_issue(
        &self,
        _auth: &AuthContext,
        _params: CreateParams,
    ) -> Result<Ticket, TicketingError> {
        Ok(Ticket::default())
    }

    async fn update_issue(
        &self,
        _auth: &AuthContext,
        _id: &str,
        _project_key: &str,
        _params: UpdateParams,
    ) -> Result<Ticket, TicketingError> {
        Ok(Ticket::default())
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
        _transition: &str,
    ) -> Result<(), TicketingError> {
        Ok(())
    }

    async fn list_statuses(
        &self,
        _auth: &AuthContext,
        _project_key: &str,
        _issue_type: &str,
    ) -> Result<Vec<Status>, TicketingError> {
        Ok(Vec::new())
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

#[tokio::test]
async fn live_ingest_issue_reference_creates_graph_rows() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    threski_db::schema::init_schema(&exec)
        .await
        .expect("init schema");

    let svc = Service::new(
        Config::default(),
        Some(Arc::new(exec.clone()) as Arc<dyn Executor>),
    );
    svc.register_provider(Arc::new(FakeTicket {
        name: PROVIDER_REDMINE,
    }));
    svc.register_provider(Arc::new(FakeTicket {
        name: PROVIDER_JIRA,
    }));

    svc.ingest_issue_reference(
        &AuthContext::default(),
        "proj",
        IssueReference {
            external_key: "42".to_string(),
            ..IssueReference::default()
        },
    )
    .await
    .expect("ingest issue reference");

    // TI-01 regression guard: the issue row EXISTS (UPDATE would no-op).
    let issue = exec
        .execute("SELECT provider, external_key, subject, status, type FROM issue;")
        .await
        .expect("select issue");
    let text = issue.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(
        text.contains("provider: 'redmine'")
            && text.contains("external_key: '42'")
            && text.contains("status: 'New'")
            && text.contains("type: 'Bug'"),
        "issue: {text}"
    );

    // Tracker + author rows exist (endpoints of the edges).
    let tracker = exec
        .execute("SELECT * FROM tracker;")
        .await
        .expect("select tracker");
    let text = tracker.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(text.contains("redmine_bug"), "tracker: {text}");

    let author = exec
        .execute("SELECT * FROM author;")
        .await
        .expect("select author");
    let text = author.first().map(|v| v.to_sql()).unwrap_or_default();
    // sanitize_id drops the non-ASCII œ (author:⟨jane_d⟩); the row keeps the
    // verbatim display name.
    assert!(
        text.contains("author:jane_d") && text.contains("Jane Dœ"),
        "author: {text}"
    );

    // Edges resolve (part_of issue→tracker, authored author→issue).
    let edges = exec
        .execute("SELECT in, out FROM part_of; SELECT in, out FROM authored;")
        .await
        .expect("select edges");
    let texts: Vec<String> = edges.statements().iter().map(|v| v.to_sql()).collect();
    assert!(
        texts[0].contains("issue:42") && texts[0].contains("tracker:redmine_bug"),
        "part_of: {}",
        texts[0]
    );
    assert!(
        texts[1].contains("author:jane_d") && texts[1].contains("issue:42"),
        "authored: {}",
        texts[1]
    );

    // Jira-prefixed record id (provider-prefixed keys are sanitized).
    svc.ingest_issue_reference(
        &AuthContext::default(),
        "proj",
        IssueReference {
            provider: ProviderName::from(PROVIDER_JIRA),
            external_key: "ABC-7".to_string(),
            external_id: "7".to_string(),
            project_key: "ABC".to_string(),
            confidence: 0.0,
        },
    )
    .await
    .expect("ingest jira ref");
    let issue = exec
        .execute("SELECT external_key FROM issue:⟨jira_abc_7⟩;")
        .await
        .expect("select jira issue");
    let text = issue.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(text.contains("ABC-7"), "jira issue: {text}");

    cleanup(&exec, &db_name).await;
}
