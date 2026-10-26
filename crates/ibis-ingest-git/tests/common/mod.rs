//! Shared fakes + git repo helpers.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use ibis_db::error::DbError;
use ibis_db::executor::{Executor, QueryOutcome, Vars};
use ibis_db::value::Value;
use ibis_git::GitRunner;
use ibis_ticketing::{Ingester, IngesterError, IssueReference, ProviderName, PROVIDER_REDMINE};

/// Fake `db.Executor` capturing every statement + its bound variables
/// `mock_ids` feeds the `SELECT VALUE id …` results.
#[derive(Default)]
pub struct MockDb {
    pub captured: Mutex<Vec<(String, Vars)>>,
    pub mock_ids: Mutex<Vec<String>>,
}

impl MockDb {
    pub fn queries(&self) -> Vec<String> {
        self.captured
            .lock()
            .unwrap()
            .iter()
            .map(|(sql, _)| sql.clone())
            .collect()
    }

    pub fn entries(&self) -> Vec<(String, Vars)> {
        self.captured.lock().unwrap().clone()
    }

    /// True when some captured statement (or its bound string values) contains
    /// `needle`. Bound variables are scanned for string values only, so the
    /// record-id list of the existing-check never produces false hits.
    pub fn any_contains(&self, needle: &str) -> bool {
        for (sql, vars) in self.entries() {
            if sql.contains(needle) {
                return true;
            }
            for value in vars.values() {
                if let Value::String(s) = value {
                    if s.contains(needle) {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn set_existing(&self, ids: &[String]) {
        *self.mock_ids.lock().unwrap() = ids.to_vec();
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
        let items: Vec<Value> = self
            .mock_ids
            .lock()
            .unwrap()
            .iter()
            .map(|s| Value::from_t(s.clone()))
            .collect();
        Ok(QueryOutcome::Single(Value::from_t(items)))
    }
}

/// Fake ticket ingester with optional per-call delay and
/// the concurrency probe of `ConcurrencyMockRedmine.
#[derive(Default)]
pub struct MockRedmine {
    pub ingested_ids: Mutex<Vec<String>>,
    pub ingested_refs: Mutex<Vec<IssueReference>>,
    pub delay: Option<Duration>,
    pub active: AtomicUsize,
    pub max_active: AtomicUsize,
    pub call_count: AtomicUsize,
}

#[async_trait]
impl Ingester for MockRedmine {
    async fn resolve_reference(
        &self,
        project_key: &str,
        mut issue_ref: IssueReference,
    ) -> Result<IssueReference, IngesterError> {
        if issue_ref.provider.is_empty() {
            issue_ref.provider = ProviderName::from(PROVIDER_REDMINE);
        }
        if issue_ref.external_key.is_empty() {
            issue_ref.external_key = issue_ref.external_id.clone();
        }
        if issue_ref.external_id.is_empty() {
            issue_ref.external_id = issue_ref.external_key.clone();
        }
        if issue_ref.project_key.is_empty() {
            issue_ref.project_key = project_key.to_string();
        }
        Ok(issue_ref)
    }

    async fn ingest_issue_reference(
        &self,
        _project_key: &str,
        issue_ref: IssueReference,
    ) -> Result<(), IngesterError> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.ingested_refs.lock().unwrap().push(issue_ref.clone());
        self.ingested_ids
            .lock()
            .unwrap()
            .push(issue_ref.external_id.clone());
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

pub fn mock_ingester() -> Arc<MockRedmine> {
    Arc::new(MockRedmine::default())
}

// --- git repo fixture ---

pub struct Repo {
    _dir: TempDir,
    pub path: String,
}

fn str_of(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

impl Repo {
    pub async fn run(&self, args: &[&str]) -> String {
        let out = GitRunner::anonymous()
            .run(&self.path, args)
            .await
            .unwrap_or_else(|e| panic!("git {args:?} in {} failed: {e}", self.path));
        String::from_utf8_lossy(&out).into_owned()
    }

    pub async fn head(&self) -> String {
        self.run(&["rev-parse", "HEAD"]).await.trim().to_string()
    }

    /// Writes a file relative to the repo root, stages it, commits it.
    pub async fn commit_file(&self, rel: &str, content: &str, msg: &str) -> String {
        let abs = std::path::Path::new(&self.path).join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        self.run(&["add", "."]).await;
        self.run(&["commit", "-q", "-m", msg]).await;
        self.head().await
    }
}

pub async fn init_repo(name: &str) -> Repo {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let repo_path = str_of(&path);
    let r = GitRunner::anonymous();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Test User"],
        vec!["config", "user.email", "test@example.com"],
    ] {
        r.run(&repo_path, &args)
            .await
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
    }
    Repo {
        _dir: dir,
        path: repo_path,
    }
}
