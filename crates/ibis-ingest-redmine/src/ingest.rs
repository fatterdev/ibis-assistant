//! On-demand issue graph ingest.
//!
//! The client's `IngestIssue` fetches a single issue (with journals) and
//! updates the knowledge graph. It is invoked "on demand" when a commit
//! references an issue — the git history ingest triggers it through the
//! `ticketing.Ingester` boundary (wave-3 service lane implements that;
//! here only the redmine half lives).
//!
//! Parity notes:
//! - **UPSERT instead of `UPDATE`** for the tracker/author/issue rows
//!   (E-A30: `UPDATE` does not create missing records on SurrealDB
//!   3.x — the original statements silently no-op'd on fresh databases).
//! - **One explicit transaction** (E-A12): the original five-statement SmartQuery
//!   could leave a partially applied graph on a mid-statement failure.
//! - All content is bound (F-002); only record identifiers are inlined.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use ibis_db::error::DbError;
use ibis_db::executor::{Executor, Vars};
use ibis_db::ids::{format_record_id, sanitize_id};
use ibis_db::schema::names::{AUTHOR, AUTHORED, ISSUE, PART_OF, TRACKER};
use ibis_db::value::Value;

use crate::client::Client;
use crate::error::RedmineError;

/// Issue-ingest boundary; [`Client`] implements it.
#[async_trait]
pub trait Ingester: Send + Sync {
    /// Fetches `issue_id` from Redmine and updates the graph. A not-found
    /// issue (404) is a successful no-op ( returns `nil`).
    async fn ingest_issue(
        &self,
        db: Arc<dyn Executor>,
        issue_id: &str,
    ) -> Result<(), IngestIssueError>;
}

#[derive(Debug, Error)]
pub enum IngestIssueError {
    #[error(transparent)]
    Client(#[from] RedmineError),
    #[error("failed to ingest issue graph: {0}")]
    Graph(#[source] DbError),
}

#[async_trait]
impl Ingester for Client {
    async fn ingest_issue(
        &self,
        db: Arc<dyn Executor>,
        issue_id_str: &str,
    ) -> Result<(), IngestIssueError> {
        tracing::info!("Fetching Redmine Issue #{issue_id_str}...");

        let Some(issue) = self.get_issue(None, issue_id_str).await? else {
            return Ok(()); // Not found
        };

        // Numeric issue ids stay unwrapped on the `issue` table (Q5); tracker
        // and author ids are angle-wrapped/sanitized likewise.
        let issue_id = format_record_id(ISSUE, &issue.id.to_string());
        let tracker_id = format_record_id(TRACKER, &issue.tracker.id.to_string());
        let author_id = format_record_id(AUTHOR, &sanitize_id(&issue.author.name));

        let sql = format!(
            "BEGIN TRANSACTION;\n\
             UPSERT {tracker_id} SET name = $tracker_name;\n\
             UPSERT {author_id} SET name = $author_name;\n\
             UPSERT {issue_id} SET subject = $subject, description = $description, status = $status, updated_on = $updated_on;\n\
             RELATE {issue_id}->{PART_OF}->{tracker_id};\n\
             RELATE {author_id}->{AUTHORED}->{issue_id};\n\
             COMMIT TRANSACTION;"
        );

        let mut vars = Vars::new();
        vars.insert(
            "tracker_name".to_string(),
            Value::from_t(issue.tracker.name.clone()),
        );
        vars.insert(
            "author_name".to_string(),
            Value::from_t(issue.author.name.clone()),
        );
        vars.insert("subject".to_string(), Value::from_t(issue.subject.clone()));
        vars.insert(
            "description".to_string(),
            Value::from_t(issue.description.clone()),
        );
        vars.insert(
            "status".to_string(),
            Value::from_t(issue.status.name.clone()),
        );
        vars.insert(
            "updated_on".to_string(),
            Value::from_t(issue.updated_on.clone()),
        );

        db.smart_query(&sql, vars)
            .await
            .map_err(IngestIssueError::Graph)?;

        tracing::info!("Successfully ingested Issue #{}", issue.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;
    use axum::routing::get;
    use ibis_db::executor::QueryOutcome;
    use std::sync::Mutex;

    /// Capturing fake `db.Executor`.
    #[derive(Default)]
    struct MockDb {
        captured: Mutex<Vec<(String, Vars)>>,
    }

    impl MockDb {
        fn entries(&self) -> Vec<(String, Vars)> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Executor for MockDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
            self.captured
                .lock()
                .unwrap()
                .push((sql.to_string(), Vars::new()));
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(&self, sql: &str, vars: Vars) -> Result<QueryOutcome, DbError> {
            self.captured.lock().unwrap().push((sql.to_string(), vars));
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    fn mock_db() -> Arc<MockDb> {
        Arc::new(MockDb::default())
    }

    async fn issue_server() -> String {
        let app = axum::Router::new().route(
            "/issues/123.json",
            get(|| async {
                axum::Json(serde_json::json!({
                    "issue": {
                        "id": 123,
                        "subject": "Test Issue with 'Quote'",
                        "description": "Desc\nLine 2",
                        "status": {"id": 1, "name": "New"},
                        "tracker": {"id": 2, "name": "Bug"},
                        "author": {"id": 5, "name": "John Doe"},
                        "created_on": "2023-01-01T12:00:00Z",
                        "updated_on": "2023-01-02T12:00:00Z",
                    }
                }))
            }),
        );
        serve(app).await
    }

    // Scenario: TestIngestIssue_Integration.
    #[tokio::test]
    async fn ingest_issue_writes_one_combined_transaction() {
        let base = issue_server().await;
        let client = Client::new(&base, "test-key");
        let mock = mock_db();

        client
            .ingest_issue(mock.clone() as Arc<dyn Executor>, "123")
            .await
            .expect("IngestIssue failed");

        let entries = mock.entries();
        assert_eq!(
            entries.len(),
            1,
            "expected 1 combined DB query, got {}",
            entries.len()
        );
        let (sql, vars) = &entries[0];

        let tracker_id = format_record_id(TRACKER, "2");
        let author_id = format_record_id(AUTHOR, &sanitize_id("John Doe"));
        let issue_id = format_record_id(ISSUE, "123");
        for fragment in [
            format!("UPSERT {tracker_id} SET name = $tracker_name"),
            format!("UPSERT {author_id} SET name = $author_name"),
            format!("UPSERT {issue_id} SET subject = $subject"),
            format!("RELATE {issue_id}->part_of->{tracker_id}"),
            format!("RELATE {author_id}->authored->{issue_id}"),
        ] {
            assert!(
                sql.contains(&fragment),
                "combined query missing: {fragment}\nGot: {sql}"
            );
        }
        assert!(sql.starts_with("BEGIN TRANSACTION;\n"), "tx wrapper: {sql}");
        assert!(sql.trim_end().ends_with("COMMIT TRANSACTION;"));

        // Bound variables carry the content (F-002).
        let var_string = |name: &str| -> String {
            match vars.get(name) {
                Some(Value::String(s)) => s.clone(),
                other => panic!("var {name}: {other:?}"),
            }
        };
        assert_eq!(var_string("tracker_name"), "Bug");
        assert_eq!(var_string("author_name"), "John Doe");
        assert_eq!(var_string("subject"), "Test Issue with 'Quote'");
        assert_eq!(var_string("description"), "Desc\nLine 2");
        assert_eq!(var_string("status"), "New");
        // No content interpolated into SQL.
        assert!(!sql.contains("John Doe") && !sql.contains("'Quote'"));
    }

    #[tokio::test]
    async fn ingest_issue_not_found_is_noop() {
        let app = axum::Router::new().route(
            "/issues/404.json",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "missing") }),
        );
        let base = serve(app).await;
        let client = Client::new(&base, "key");
        let mock = mock_db();

        client
            .ingest_issue(mock.clone() as Arc<dyn Executor>, "404")
            .await
            .expect("404 must be a no-op success");
        assert!(mock.entries().is_empty(), "no DB writes for missing issue");
    }

    #[tokio::test]
    async fn ingest_issue_propagates_db_errors() {
        use ibis_db::error::DbError;
        struct FailingDb;
        #[async_trait]
        impl Executor for FailingDb {
            async fn execute(&self, _sql: &str) -> Result<QueryOutcome, DbError> {
                Err(DbError::decode("boom", "SELECT 1"))
            }
            async fn smart_query(&self, _sql: &str, _vars: Vars) -> Result<QueryOutcome, DbError> {
                Err(DbError::decode("boom", "BEGIN TRANSACTION;"))
            }
        }

        let base = issue_server().await;
        let client = Client::new(&base, "key");
        let err = client
            .ingest_issue(Arc::new(FailingDb) as Arc<dyn Executor>, "123")
            .await
            .expect_err("db failure must propagate");
        assert!(err
            .to_string()
            .starts_with("failed to ingest issue graph: "));
    }
}
