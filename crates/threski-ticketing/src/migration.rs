//! Legacy issue-record backfill.
//!
//! Kept for contract parity with the original startup path (TI-07): on a
//! born-fresh database (D16 — no data heritage) the table is empty and the
//! statement is a no-op. The bulk `UPDATE … WHERE` shape is D17-compliant
//!: bulk `UPDATE table WHERE` is always fine).

use threski_db::executor::Executor;
use threski_db::schema::names::ISSUE;

use crate::types::TicketingError;

/// Backfills provider metadata for pre-ticketing issue records
/// `None` executor is a no-op, mirroring the
/// nil check.
pub async fn migrate_legacy_issues(
    db: Option<&std::sync::Arc<dyn Executor>>,
) -> Result<(), TicketingError> {
    let Some(db) = db else {
        return Ok(());
    };
    let sql = format!(
        "UPDATE {ISSUE} SET provider = 'redmine', external_id = id, external_key = id \
         WHERE provider = NONE OR provider = '';"
    );
    db.execute(&sql).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use threski_db::executor::{QueryOutcome, Vars};
    use threski_db::value::Value;
    use std::sync::{Arc, Mutex};

    /// Capturing fake executor.
    #[derive(Default)]
    struct MockDb {
        queries: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Executor for MockDb {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, threski_db::error::DbError> {
            self.queries.lock().unwrap().push(sql.to_string());
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(
            &self,
            _sql: &str,
            _vars: Vars,
        ) -> Result<QueryOutcome, threski_db::error::DbError> {
            Ok(QueryOutcome::Single(Value::None))
        }
    }

    // Scenario: TestMigrateLegacyIssues.
    #[tokio::test]
    async fn emits_one_provider_backfill_query() {
        let mock = Arc::new(MockDb::default());
        migrate_legacy_issues(Some(&(mock.clone() as Arc<dyn Executor>)))
            .await
            .expect("migration");

        let queries = mock.queries.lock().unwrap();
        assert_eq!(queries.len(), 1, "one migration query");
        let query = queries[0].to_lowercase();
        assert!(
            query.contains("provider = 'redmine'"),
            "expected provider backfill query, got {}",
            queries[0]
        );
        assert!(
            query.starts_with("update issue "),
            "targets issue table: {}",
            queries[0]
        );
    }

    #[tokio::test]
    async fn nil_db_is_noop() {
        migrate_legacy_issues(None).await.expect("nil is a no-op");
    }
}
