//! Test fakes mirroring the original test estate (MockDB / MockAI / fake git
//! sources). Available to tests in this crate only (`pub(crate)`).
//!
//! Deliberately retains the full fake-surface (anchors/notes/claim-writer
//! fakes + helpers) ahead of the memory live-test estate (wave 4 closeout) —
//! the dead-code allowance below is scoped to that planned consumer, not to
//! forgotten leftovers.

#![allow(dead_code, clippy::type_complexity, clippy::derivable_impls)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use threski_db::value::{Object, Value};

use threski_ai::types::{Candidate, Content, GenerationConfig};
use threski_ai::AIProvider;
use threski_db::executor::{QueryOutcome, Vars};
use threski_db::Executor;
use threski_git::error::GitError;
use threski_git::{GitAnchorSource, GitClaimWriter, GitNotesSource};

/// Builds one object row from `(key, value)` pairs.
pub(crate) fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut o = Object::new();
    for (k, v) in pairs {
        o.insert(k, v);
    }
    Value::Object(o)
}

/// Builds an array value from a list of values.
pub(crate) fn rows_of(values: Vec<Value>) -> Value {
    Value::from_t(values)
}

/// `MockDB: records execute/smart calls and returns the value whose key
/// is the **longest** substring match of the SQL (so "FROM commit_chunk"
/// wins over "FROM commit").
#[derive(Default)]
pub(crate) struct FakeDb {
    pub execute_calls: Mutex<Vec<String>>,
    pub smart_calls: Mutex<Vec<String>>,
    pub smart_vars: Mutex<Vec<Vars>>,
    pub return_data: Vec<(String, Value)>,
    /// When set, matching calls fail with this message.
    pub fail_on: Option<String>,
}

impl FakeDb {
    pub fn new(return_data: Vec<(&str, Value)>) -> Self {
        FakeDb {
            return_data: return_data
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ..Default::default()
        }
    }

    fn outcome(value: &Value) -> QueryOutcome {
        QueryOutcome::Single(value.clone())
    }

    fn match_return_data(&self, sql: &str) -> Option<QueryOutcome> {
        let mut best_key = "";
        let mut best: Option<&Value> = None;
        for (k, v) in &self.return_data {
            if sql.contains(k.as_str()) && k.len() > best_key.len() {
                best_key = k;
                best = Some(v);
            }
        }
        best.map(Self::outcome)
    }

    pub fn calls(&self) -> Vec<String> {
        self.execute_calls
            .lock()
            .unwrap()
            .iter()
            .chain(self.smart_calls.lock().unwrap().iter())
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Executor for FakeDb {
    async fn execute(&self, sql: &str) -> Result<QueryOutcome, threski_db::DbError> {
        self.execute_calls.lock().unwrap().push(sql.to_string());
        if let Some(fail) = &self.fail_on {
            if sql.contains(fail.as_str()) {
                return Err(threski_db::DbError::Query {
                    message: format!("fake failure on {fail}"),
                    duration_ms: 0,
                });
            }
        }
        Ok(self
            .match_return_data(sql)
            .unwrap_or(QueryOutcome::Single(Value::None)))
    }

    async fn smart_query(&self, sql: &str, vars: Vars) -> Result<QueryOutcome, threski_db::DbError> {
        self.smart_calls.lock().unwrap().push(sql.to_string());
        self.smart_vars.lock().unwrap().push(vars);
        if let Some(fail) = &self.fail_on {
            if sql.contains(fail.as_str()) {
                return Err(threski_db::DbError::Query {
                    message: format!("fake failure on {fail}"),
                    duration_ms: 0,
                });
            }
        }
        Ok(self
            .match_return_data(sql)
            .unwrap_or(QueryOutcome::Single(Value::None)))
    }
}

/// `MockAI: fixed [0.1, 0.2, 0.3] embeddings and a configurable
/// generation function (defaults to "FINAL ANSWER: Mocked Default").
pub(crate) struct FakeAI {
    pub generate: Option<Box<dyn Fn(&[Content]) -> Candidate + Send + Sync>>,
    pub embed: Option<Box<dyn Fn(&str) -> Vec<f32> + Send + Sync>>,
}

impl FakeAI {
    pub fn new() -> Arc<FakeAI> {
        Arc::new(FakeAI {
            generate: None,
            embed: None,
        })
    }

    pub fn with_generate(
        f: impl Fn(&[Content]) -> Candidate + Send + Sync + 'static,
    ) -> Arc<FakeAI> {
        Arc::new(FakeAI {
            generate: Some(Box::new(f)),
            embed: None,
        })
    }
}

impl Default for FakeAI {
    fn default() -> Self {
        FakeAI {
            generate: None,
            embed: None,
        }
    }
}

#[async_trait]
impl AIProvider for FakeAI {
    fn name(&self) -> &str {
        "fake"
    }

    async fn is_functional(&self) -> bool {
        true
    }

    async fn embed_text(&self, _text: &str) -> Result<Vec<f32>, threski_ai::AiError> {
        match &self.embed {
            Some(f) => Ok(f(_text)),
            None => Ok(vec![0.1, 0.2, 0.3]),
        }
    }

    async fn generate_content(
        &self,
        contents: &[Content],
        _config: GenerationConfig,
    ) -> Result<Candidate, threski_ai::AiError> {
        if let Some(f) = &self.generate {
            return Ok(f(contents));
        }
        Ok(Candidate {
            content: Content {
                role: "model".into(),
                parts: vec![threski_ai::types::Part {
                    text: "FINAL ANSWER: Mocked Default".into(),
                }],
            },
            finish_reason: "STOP".into(),
            usage_metadata: None,
        })
    }
}

/// Convenience constructors used by several test modules.
pub(crate) fn fake_db(return_data: Vec<(&str, Value)>) -> Arc<dyn Executor> {
    Arc::new(FakeDb::new(return_data))
}

pub(crate) fn fake_ai() -> Arc<dyn AIProvider> {
    FakeAI::new()
}

/// A record id value.
pub(crate) fn id_value(id: &str) -> Value {
    Value::from_t(id)
}

// --- git fakes -------------------------------------------------------------

/// Configurable fake [`GitAnchorSource`] ( fakeGitAnchors).
#[derive(Default)]
pub(crate) struct FakeAnchors {
    pub head_oid: Mutex<String>,
    pub clean: Mutex<bool>,
    pub work_hashes: Mutex<std::collections::HashMap<String, String>>,
    pub head_hashes: Mutex<std::collections::HashMap<String, String>>,
    pub trees: Mutex<std::collections::HashMap<String, Vec<String>>>,
    pub files_at_ref: Mutex<std::collections::HashMap<(String, String), Vec<u8>>>,
    pub file_history: Mutex<std::collections::HashMap<String, Vec<String>>>,
    pub missing_commits: Mutex<std::collections::HashSet<String>>,
    pub shallow: Mutex<bool>,
    pub ancestors: Mutex<std::collections::HashMap<String, Vec<String>>>,
    pub diff_paths: Mutex<Vec<String>>,
    pub merge_base: Mutex<String>,
}

impl FakeAnchors {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeAnchors::default())
    }

    pub fn with_head(head: &str) -> Arc<Self> {
        let f = Self::new();
        *f.head_oid.lock().unwrap() = head.to_string();
        *f.clean.lock().unwrap() = true;
        f
    }
}

#[async_trait]
impl GitAnchorSource for FakeAnchors {
    async fn rev_parse_head(&self, _repo_path: &str) -> Result<String, GitError> {
        Ok(self.head_oid.lock().unwrap().clone())
    }

    async fn head_branch(&self, _repo_path: &str) -> Result<String, GitError> {
        Ok("main".to_string())
    }

    async fn work_file_hash(
        &self,
        _repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        Ok(self.work_hashes.lock().unwrap().get(rel_path).cloned())
    }

    async fn head_file_hash(
        &self,
        _repo_path: &str,
        rel_path: &str,
    ) -> Result<Option<String>, GitError> {
        Ok(self.head_hashes.lock().unwrap().get(rel_path).cloned())
    }

    async fn is_clean(&self, _repo_path: &str) -> Result<bool, GitError> {
        Ok(*self.clean.lock().unwrap())
    }

    async fn verify_commits(
        &self,
        _repo_path: &str,
        oids: &[String],
    ) -> Result<std::collections::HashSet<String>, GitError> {
        let missing = self.missing_commits.lock().unwrap();
        Ok(oids
            .iter()
            .filter(|o| missing.contains(o.as_str()))
            .cloned()
            .collect())
    }

    async fn list_ancestors(&self, _repo_path: &str, oid: &str) -> Result<Vec<String>, GitError> {
        Ok(self
            .ancestors
            .lock()
            .unwrap()
            .get(oid)
            .cloned()
            .unwrap_or_default())
    }

    async fn resolve_ref(&self, _repo_path: &str, reference: &str) -> Result<String, GitError> {
        if reference.trim().is_empty() {
            return Err(GitError::other("empty ref"));
        }
        if threski_git::anchor::is_full_commit_oid(reference.trim()) {
            return Ok(reference.trim().to_string());
        }
        Ok(self.head_oid.lock().unwrap().clone())
    }

    async fn list_tree(&self, _repo_path: &str, oid: &str) -> Result<Vec<String>, GitError> {
        Ok(self
            .trees
            .lock()
            .unwrap()
            .get(oid)
            .cloned()
            .unwrap_or_default())
    }

    async fn file_at_ref(
        &self,
        _repo_path: &str,
        oid: &str,
        rel_path: &str,
    ) -> Result<Option<Vec<u8>>, GitError> {
        Ok(self
            .files_at_ref
            .lock()
            .unwrap()
            .get(&(oid.to_string(), rel_path.to_string()))
            .cloned())
    }

    async fn file_history(
        &self,
        _repo_path: &str,
        rel_path: &str,
    ) -> Result<Vec<String>, GitError> {
        Ok(self
            .file_history
            .lock()
            .unwrap()
            .get(rel_path)
            .cloned()
            .unwrap_or_default())
    }

    async fn merge_base(&self, _repo_path: &str, _a: &str, _b: &str) -> Result<String, GitError> {
        Ok(self.merge_base.lock().unwrap().clone())
    }

    async fn is_shallow(&self, _repo_path: &str) -> Result<bool, GitError> {
        Ok(*self.shallow.lock().unwrap())
    }

    async fn diff_paths(
        &self,
        _repo_path: &str,
        _from_oid: &str,
        _to_oid: &str,
    ) -> Result<Vec<String>, GitError> {
        Ok(self.diff_paths.lock().unwrap().clone())
    }
}

/// Fake [`GitNotesSource`] recording calls ( fakeNotesSource).
#[derive(Default)]
pub(crate) struct FakeNotes {
    pub notes: Mutex<std::collections::HashMap<String, String>>,
    pub added: Mutex<Vec<String>>,
    pub removed: Mutex<Vec<String>>,
    pub fail_add: Mutex<bool>,
}

impl FakeNotes {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeNotes::default())
    }
}

#[async_trait]
impl GitNotesSource for FakeNotes {
    async fn notes_list(
        &self,
        _repo_path: &str,
        _reference: &str,
    ) -> Result<std::collections::HashMap<String, String>, GitError> {
        Ok(self.notes.lock().unwrap().clone())
    }

    async fn notes_add(
        &self,
        _repo_path: &str,
        _reference: &str,
        oid: &str,
        message: &str,
    ) -> Result<(), GitError> {
        if *self.fail_add.lock().unwrap() {
            return Err(GitError::other("notes add failed"));
        }
        self.added.lock().unwrap().push(format!("{oid}:{message}"));
        self.notes
            .lock()
            .unwrap()
            .insert(oid.to_string(), message.to_string());
        Ok(())
    }

    async fn notes_remove(
        &self,
        _repo_path: &str,
        _reference: &str,
        oid: &str,
    ) -> Result<(), GitError> {
        self.removed.lock().unwrap().push(oid.to_string());
        self.notes.lock().unwrap().remove(oid);
        Ok(())
    }
}

/// Fake [`GitClaimWriter`] recording commits ( fakeClaimWriter).
#[derive(Default)]
pub(crate) struct FakeClaimWriter {
    pub commits: Mutex<
        Vec<(
            String,
            std::collections::HashMap<String, Vec<u8>>,
            Vec<String>,
        )>,
    >,
    pub fail: Mutex<bool>,
    pub next_oid: Mutex<u32>,
}

impl FakeClaimWriter {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeClaimWriter::default())
    }
}

#[async_trait]
impl GitClaimWriter for FakeClaimWriter {
    async fn commit_claim(
        &self,
        repo_path: &str,
        message: &str,
        writes: &std::collections::HashMap<String, Vec<u8>>,
        deletes: &[String],
    ) -> Result<String, GitError> {
        if *self.fail.lock().unwrap() {
            return Err(GitError::other("claim commit failed"));
        }
        self.commits
            .lock()
            .unwrap()
            .push((message.to_string(), writes.clone(), deletes.to_vec()));
        let mut n = self.next_oid.lock().unwrap();
        *n += 1;
        let _ = repo_path;
        Ok(format!("{:040x}", *n as u64))
    }
}
