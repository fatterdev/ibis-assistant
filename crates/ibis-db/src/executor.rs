//! The `Executor` trait and the live SurrealDB client.
//!
//! Contract:
//! - WebSocket `ws://…/rpc`, root sign-in, `USE NS/DB`, default timeout 300s.
//! - **3 retries on `transaction conflict`** with backoff `100*(i+1)ms`.
//! - "already exists" errors demoted to debug logs.
//! - single-statement responses are unwrapped into [`QueryOutcome::Single`]
//!   (multi-statement batches stay per-statement in [`QueryOutcome::Multi`], Q8/Q10).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use surrealdb::engine::any::{self, Any};
use surrealdb::Surreal;
use tokio::time::Instant;

use crate::error::DbError;
use crate::value::SurrealValue;

/// Default client timeout (`Client.Timeout: 300s).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Retry policy constants.
const MAX_RETRIES: u32 = 3;
const BACKOFF_BASE_MS: u64 = 100;

/// Bound variables for [`Executor::smart_query`] (F-002: every content-bearing
/// query binds its values; string interpolation is reserved for identifiers).
pub type Vars = BTreeMap<String, surrealdb::types::Value>;

/// Builds a [`Vars`] map from `(key, value)` pairs.
pub fn vars(pairs: impl IntoIterator<Item = (impl Into<String>, impl SurrealValue)>) -> Vars {
    pairs
        .into_iter()
        .map(|(k, v)| (k.into(), v.into_value()))
        .collect()
}

/// The result of one `execute`/`smart_query` call.
///
/// Mirrors the original client's unwrap semantics: a single-statement response is
/// unwrapped to its result; multi-statement responses keep one entry per
/// statement (Q8/Q10).
#[derive(Debug, Clone)]
pub enum QueryOutcome {
    Single(surrealdb::types::Value),
    Multi(Vec<surrealdb::types::Value>),
}

impl QueryOutcome {
    /// Per-statement results (length 1 for [`QueryOutcome::Single`]).
    pub fn statements(&self) -> &[surrealdb::types::Value] {
        match self {
            QueryOutcome::Single(v) => std::slice::from_ref(v),
            QueryOutcome::Multi(v) => v,
        }
    }

    /// First statement result, if any.
    pub fn first(&self) -> Option<&surrealdb::types::Value> {
        match self {
            QueryOutcome::Single(v) => Some(v),
            QueryOutcome::Multi(v) => v.first(),
        }
    }

    /// Decodes the first statement result into typed rows (see [`crate::value::decode_row_list`]).
    pub fn rows<T: SurrealValue>(&self, sql: &str) -> Result<Vec<T>, DbError> {
        let empty = surrealdb::types::Value::None;
        crate::value::decode_row_list(self.first().unwrap_or(&empty), sql)
    }

    /// Decodes the first statement result into a single typed value.
    pub fn single<T: SurrealValue>(&self, sql: &str) -> Result<T, DbError> {
        let empty = surrealdb::types::Value::None;
        crate::value::decode_single(self.first().unwrap_or(&empty), sql)
    }

    /// Decodes a `SELECT VALUE id …` result into canonical record-ID strings (Q7).
    pub fn value_ids(&self, sql: &str) -> Result<Vec<String>, DbError> {
        let empty = surrealdb::types::Value::None;
        crate::value::decode_select_value_ids(self.first().unwrap_or(&empty), sql)
    }
}

/// Database access boundary.
///
/// Implementations must be usable from multiple tasks concurrently (`&self`).
/// Dropping the executor closes the underlying connection — the `Close`
/// maps to ownership in Rust.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Executes raw SurrealQL with no bound variables (DDL, transactions).
    async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError>;

    /// Executes SurrealQL with bound variables — the default for every
    /// content-bearing statement (F-002).
    async fn smart_query(&self, sql: &str, vars: Vars) -> Result<QueryOutcome, DbError>;

    /// Typed convenience: `SELECT VALUE id FROM …` (Q7).
    async fn select_value_ids(&self, sql: &str) -> Result<Vec<String>, DbError> {
        self.smart_query(sql, Vars::new()).await?.value_ids(sql)
    }
}

/// Live SurrealDB executor over the crate's WebSocket client.
#[derive(Clone)]
pub struct SurrealDbExecutor {
    client: Surreal<Any>,
    timeout: Duration,
}

impl SurrealDbExecutor {
    /// Connects, signs in and selects the namespace/database
    /// (`NewClient, default timeout 300s).
    pub async fn connect(
        endpoint: &str,
        ns: &str,
        db_name: &str,
        user: &str,
        pass: &str,
    ) -> Result<Self, DbError> {
        tracing::info!("Connecting to SurrealDB at {endpoint}...");
        let client = any::connect(endpoint).await?;
        client
            .signin(surrealdb::opt::auth::Root {
                username: user.to_string(),
                password: pass.to_string(),
            })
            .await
            .map_err(|e| DbError::Query {
                message: format!("failed to signin: {e}"),
                duration_ms: 0,
            })?;
        client
            .use_ns(ns)
            .use_db(db_name)
            .await
            .map_err(|e| DbError::Query {
                message: format!("failed to use ns/db: {e}"),
                duration_ms: 0,
            })?;
        Ok(SurrealDbExecutor {
            client,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Overrides the per-query timeout.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    async fn run(&self, sql: &str, variables: Vars) -> Result<QueryOutcome, DbError> {
        let started = Instant::now();
        let attempt = async {
            let mut query = self.client.query(sql);
            if !variables.is_empty() {
                query = query.bind(variables);
            }
            let mut response = query
                .await
                .map_err(|e| DbError::query(e, sql, started.elapsed()))?;
            response = response
                .check()
                .map_err(|e| DbError::query(e, sql, started.elapsed()))?;
            collect_outcome(response)
        };
        match tokio::time::timeout(self.timeout, attempt).await {
            Ok(inner) => inner,
            Err(_) => {
                tracing::debug!("SQL Cancelled/Timed Out during execution");
                Err(DbError::Timeout(self.timeout))
            }
        }
    }
}

/// Unwraps per-statement results following the original client contract.
fn collect_outcome(mut response: surrealdb::IndexedResults) -> Result<QueryOutcome, DbError> {
    let mut results = Vec::new();
    for idx in 0..response.num_statements() {
        let value: surrealdb::types::Value = response.take(idx).map_err(|e| DbError::Decode {
            message: format!("failed to take statement {idx}: {e}"),
        })?;
        results.push(value);
    }
    match results.pop() {
        Some(last) if results.is_empty() => Ok(QueryOutcome::Single(last)),
        Some(last) => {
            results.push(last);
            Ok(QueryOutcome::Multi(results))
        }
        None => Ok(QueryOutcome::Multi(results)),
    }
}

fn is_retryable_conflict(message: &str) -> bool {
    message.to_lowercase().contains("transaction conflict")
}

fn is_already_exists(message: &str) -> bool {
    message.to_lowercase().contains("already exists")
}

#[async_trait]
impl Executor for SurrealDbExecutor {
    async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
        tracing::debug!("SQL: {sql}");
        for attempt in 0..MAX_RETRIES {
            match self.run(sql, Vars::new()).await {
                Ok(outcome) => {
                    tracing::debug!("SQL SUCCESS");
                    return Ok(outcome);
                }
                Err(err) => {
                    let message = err.to_string();
                    if is_retryable_conflict(&message) && attempt < MAX_RETRIES - 1 {
                        tracing::warn!(
                            "SQL Transaction Conflict. Retrying ({}/{MAX_RETRIES})...",
                            attempt + 1
                        );
                        tokio::time::sleep(Duration::from_millis(
                            BACKOFF_BASE_MS * u64::from(attempt + 1),
                        ))
                        .await;
                        continue;
                    }
                    if is_already_exists(&message) {
                        tracing::debug!("SQL (Already Exists): {message}");
                    } else {
                        tracing::error!("SQL ERROR: {message}");
                    }
                    return Err(err);
                }
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn smart_query(&self, sql: &str, v: Vars) -> Result<QueryOutcome, DbError> {
        tracing::debug!("SQL (Smart): {sql}");
        for attempt in 0..MAX_RETRIES {
            match self.run(sql, v.clone()).await {
                Ok(outcome) => {
                    tracing::debug!("SQL SUCCESS");
                    return Ok(outcome);
                }
                Err(err) => {
                    let message = err.to_string();
                    if is_retryable_conflict(&message) && attempt < MAX_RETRIES - 1 {
                        tracing::warn!(
                            "SQL Transaction Conflict. Retrying ({}/{MAX_RETRIES})...",
                            attempt + 1
                        );
                        tokio::time::sleep(Duration::from_millis(
                            BACKOFF_BASE_MS * u64::from(attempt + 1),
                        ))
                        .await;
                        continue;
                    }
                    tracing::error!("SQL ERROR: {message}");
                    return Err(err);
                }
            }
        }
        unreachable!("retry loop always returns")
    }
}
