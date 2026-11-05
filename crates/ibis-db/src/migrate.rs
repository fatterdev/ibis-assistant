//! Migration framework.
//!
//! Everything is born fresh — the SurrealDB ships final-state DDL with migrations
//! 1–5 folded into [`crate::schema`], so this framework starts with an **empty
//! history** ([`MIGRATIONS`]). What is implemented and unit-tested is the framework
//! behavior itself:
//! - the version record `migration:current` (table `migration`, schemaless);
//! - **UPSERT-only persistence**: `UPSERT migration:current SET version = $version,
//!   applied_at = time::now;` — `UPSERT` creates the record on a fresh database
//!   where `UPDATE` cannot;
//! - pending migrations apply in ascending id order regardless of input order,
//!   duplicate ids are rejected (F13), each followed immediately by its
//!   version record write;
//! - already-applied versions are skipped.

use crate::error::DbError;
use crate::executor::{vars, Executor, Vars};
use crate::value::{Value, I64};

/// One migration: a monotonically increasing version and its SurrealQL.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub id: i64,
    pub sql: &'static str,
}

/// Migration history. Empty per D16: no data heritage exists, so the shipped
/// DDL is already the final state. New migrations append here.
pub const MIGRATIONS: &[Migration] = &[];

/// The version record (the only row of the `migration` table).
pub const CURRENT_RECORD: &str = "migration:current";

const DEFINE_MIGRATION_TABLE: &str = "DEFINE TABLE IF NOT EXISTS migration SCHEMALESS;";
const SELECT_CURRENT: &str = "SELECT version FROM migration:current;";
const UPSERT_CURRENT: &str =
    "UPSERT migration:current SET version = $version, applied_at = time::now();";

/// Applies all pending migrations from the built-in (currently empty) history.
pub async fn migrate(exec: &dyn Executor) -> Result<i64, DbError> {
    migrate_with(exec, MIGRATIONS).await
}

/// Framework entry point over an explicit migration list (unit-test seam).
///
/// The list is applied **deterministically**: migrations are sorted by id
/// ascending and duplicate ids are rejected (F13 — an unsorted `[3, 2]` list
/// used to end at version 2 and re-apply 3 on the next boot). The version
/// record is persisted only after each migration succeeds, so a failing
/// migration never advances the version.
pub async fn migrate_with(exec: &dyn Executor, migrations: &[Migration]) -> Result<i64, DbError> {
    tracing::info!("Checking for database migrations...");
    exec.execute(DEFINE_MIGRATION_TABLE).await?;

    let mut sorted: Vec<&Migration> = migrations.iter().collect();
    sorted.sort_by_key(|m| m.id);
    if let Some(dup) = sorted.windows(2).find(|w| w[0].id == w[1].id) {
        return Err(DbError::Decode {
            message: format!("duplicate migration id {}", dup[0].id),
        });
    }

    let current = current_version(exec).await?;
    tracing::info!("Current DB version: {current}");

    let mut version = current;
    for migration in sorted.iter().filter(|m| m.id > current) {
        tracing::info!("Applying migration {}...", migration.id);
        exec.execute(migration.sql).await?;
        record_version(exec, migration.id).await?;
        version = migration.id;
    }
    Ok(version)
}

/// Reads the persisted version. A missing `migration:current` record (fresh DB)
/// reads as version 0; query failures are propagated ( swallowed all errors
/// here, which could silently replay migrations — fixed in this crate).
pub async fn current_version(exec: &dyn Executor) -> Result<i64, DbError> {
    let outcome = exec.execute(SELECT_CURRENT).await?;
    let Some(Value::Array(rows)) = outcome.first() else {
        return Ok(0);
    };
    for row in rows {
        let Value::Object(map) = row else {
            continue;
        };
        let Some(v) = map.get("version") else {
            continue;
        };
        let n: surrealdb::types::Number = v.clone().into_t().map_err(|e| DbError::Decode {
            message: format!("migration:current.version: {e}"),
        })?;
        if let Some(version) = n.to_int() {
            return Ok(version);
        }
    }
    Ok(0)
}

/// Persists the applied version. UPSERT-only: the
/// statement must create `migration:current` on a fresh database, which plain
/// `UPDATE` cannot; `CREATE` would fail once the record exists.
pub async fn record_version(exec: &dyn Executor, version: i64) -> Result<(), DbError> {
    let v: Vars = vars([("version", I64(version))]);
    exec.smart_query(UPSERT_CURRENT, v).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use surrealdb::types::{Number, Object};

    struct RecordingExecutor {
        statements: Mutex<Vec<String>>,
        select_result: Option<Value>,
        /// SQL substring that makes `execute` fail (failure injection).
        fail_on: Option<String>,
        /// Versions bound by `UPSERT migration:current` writes.
        versions: Mutex<Vec<i64>>,
    }

    impl RecordingExecutor {
        fn new(select_result: Option<Value>) -> Self {
            RecordingExecutor {
                statements: Mutex::new(Vec::new()),
                select_result,
                fail_on: None,
                versions: Mutex::new(Vec::new()),
            }
        }

        fn statements(&self) -> Vec<String> {
            self.statements.lock().expect("recording executor").clone()
        }

        fn versions(&self) -> Vec<i64> {
            self.versions.lock().expect("recording executor").clone()
        }
    }

    fn version_result(version: i64) -> Value {
        let mut obj = Object::new();
        obj.insert("version", Number::Int(version));
        Value::from_t(vec![Value::Object(obj)])
    }

    #[async_trait]
    impl Executor for RecordingExecutor {
        async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
            self.statements
                .lock()
                .expect("recording executor")
                .push(sql.to_string());
            if sql
                .trim()
                .starts_with("SELECT version FROM migration:current")
            {
                let result = self.select_result.clone().unwrap_or(Value::None);
                return Ok(QueryOutcome::Single(result));
            }
            if let Some(fail) = &self.fail_on {
                if sql.contains(fail.as_str()) {
                    return Err(DbError::Query {
                        message: format!("injected failure on {fail}"),
                        duration_ms: 0,
                    });
                }
            }
            Ok(QueryOutcome::Single(Value::None))
        }

        async fn smart_query(&self, sql: &str, v: Vars) -> Result<QueryOutcome, DbError> {
            if sql.contains("UPSERT migration:current") {
                if let Some(version) = v.get("version").and_then(|val| match val {
                    Value::Number(n) => n.to_int(),
                    _ => None,
                }) {
                    self.versions
                        .lock()
                        .expect("recording executor")
                        .push(version);
                }
            }
            self.execute(sql).await
        }
    }

    use crate::executor::QueryOutcome;

    const MEMORY_BACKFILL: Migration = Migration {
        id: 3,
        sql: "UPDATE memory SET kind = 'fact', status = 'current', visibility = 'team', confidence = 0.7, created_at = time::now(), updated_at = time::now() WHERE kind = NONE;",
    };

    /// F13: an unsorted `[3, 2]` list applies 2 then 3 and ends at version 3.
    #[tokio::test]
    async fn unsorted_migrations_apply_in_id_order() {
        let exec = RecordingExecutor::new(None);
        let earlier = Migration {
            id: 2,
            sql: "DEFINE TABLE _probe_m2 SCHEMALESS;",
        };
        let migrations = [MEMORY_BACKFILL, earlier];
        let version = migrate_with(&exec, &migrations).await.expect("migrate");
        assert_eq!(version, 3);
        let statements = exec.statements();
        let m2 = statements
            .iter()
            .position(|s| s.contains("_probe_m2"))
            .expect("migration 2 applied");
        let m3 = statements
            .iter()
            .position(|s| s.contains("kind = 'fact'"))
            .expect("migration 3 applied");
        assert!(m2 < m3, "migration 2 must run before 3: {statements:?}");
        assert_eq!(exec.versions(), vec![2, 3], "versions persist in order");
    }

    /// F13: duplicate ids are rejected instead of silently double-applied.
    #[tokio::test]
    async fn duplicate_migration_ids_are_rejected() {
        let exec = RecordingExecutor::new(None);
        let migrations = [MEMORY_BACKFILL, MEMORY_BACKFILL];
        let err = migrate_with(&exec, &migrations)
            .await
            .expect_err("duplicate ids");
        assert!(
            err.to_string().contains("duplicate migration id 3"),
            "{err}"
        );
    }

    /// F13: a failing migration does not advance the persisted version; the
    /// retry applies it again and completes.
    #[tokio::test]
    async fn failed_migration_does_not_advance_version() {
        let mut exec = RecordingExecutor::new(None);
        exec.fail_on = Some("kind = 'fact'".to_string());
        let err = migrate_with(&exec, &[MEMORY_BACKFILL])
            .await
            .expect_err("injected failure");
        assert!(err.to_string().contains("injected failure"), "{err}");
        let statements = exec.statements();
        assert!(
            statements.iter().any(|s| s.contains("kind = 'fact'")),
            "the failing migration ran: {statements:?}"
        );
        assert!(
            exec.versions().is_empty(),
            "version must not advance past a failed migration: {:?}",
            exec.versions()
        );

        // Retry with the failure cleared: the migration applies and the
        // version lands on 3.
        exec.fail_on = None;
        let version = migrate_with(&exec, &[MEMORY_BACKFILL])
            .await
            .expect("retry");
        assert_eq!(version, 3);
        assert_eq!(exec.versions(), vec![3]);
    }

    #[tokio::test]
    async fn applies_pending_migration() {
        let exec = RecordingExecutor::new(Some(version_result(2)));
        let migrations = [MEMORY_BACKFILL];
        let version = migrate_with(&exec, &migrations).await.expect("migrate");
        assert_eq!(version, 3);

        let statements = exec.statements();
        assert!(
            statements
                .iter()
                .any(|s| s.contains("kind = 'fact'") && s.contains("WHERE kind = NONE")),
            "expected migration 3 to backfill legacy memory rows, statements: {statements:?}"
        );
    }

    #[tokio::test]
    async fn skips_already_applied() {
        let exec = RecordingExecutor::new(Some(version_result(3)));
        let migrations = [MEMORY_BACKFILL];
        let version = migrate_with(&exec, &migrations).await.expect("migrate");
        assert_eq!(version, 3);
        let statements = exec.statements();
        assert!(
            !statements.iter().any(|s| s.contains("kind = 'fact'")),
            "applied migration must be skipped, statements: {statements:?}"
        );
    }

    #[tokio::test]
    async fn persists_version_with_upsert_only() {
        let exec = RecordingExecutor::new(None);
        let migrations = [MEMORY_BACKFILL];
        migrate_with(&exec, &migrations).await.expect("migrate");

        for statement in exec.statements() {
            let trimmed = statement.trim_start();
            assert!(
                !trimmed.starts_with("UPDATE migration:current")
                    && !trimmed.starts_with("CREATE migration:current"),
                "expected UPSERT-only version persistence, got {statement:?}"
            );
        }
        let statements = exec.statements();
        assert!(
            statements.iter().any(|s| s
                .trim_start()
                .starts_with("UPSERT migration:current SET version")),
            "expected UPSERT migration:current, statements: {statements:?}"
        );
    }

    #[tokio::test]
    async fn empty_history_touches_nothing_but_the_version_record() {
        let exec = RecordingExecutor::new(None);
        let version = migrate(&exec).await.expect("migrate");
        assert_eq!(version, 0);
        let statements = exec.statements();
        assert_eq!(statements.len(), 2, "statements: {statements:?}");
        assert_eq!(
            statements[0],
            "DEFINE TABLE IF NOT EXISTS migration SCHEMALESS;"
        );
        assert_eq!(statements[1], "SELECT version FROM migration:current;");
        assert!(
            !statements
                .iter()
                .any(|s| s.contains("UPSERT migration:current")),
            "no version record must be written when nothing is applied, statements: {statements:?}"
        );
    }
}
