//! Pipeline tests over a fake `db.Executor`.
//!
//! The temp dirs are not git repositories, so `git ls-tree` fails and every
//! scenario exercises the directory-walk fallback — exactly the original test
//! conditions. The sg locator resolves no binary in the test environment, so
//! `ParseAST` fails fast and files fall back to whole-file chunking (same as
//! where `scanSGContent` fails the exec and returns empty results).
//!
//! scenarios (deferred as benches per:
//! `BenchmarkChunkContent` is pinned functionally by
//! `ingest::tests::chunk_content_large_input_scenario`;
//! `BenchmarkIngestCodebase_NoChange` (hash-skip loop) and
//! `BenchmarkIngestCodebase_Write` (200KB file) are pinned behaviorally by
//! `ingest_codebase_delta` (skip unchanged) and `ingest_codebase_batching`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use threski_core::config::Config;
use threski_db::error::DbError;
use threski_db::executor::{Executor, QueryOutcome, Vars};
use threski_db::ids::{format_record_id, sanitize_id};
use threski_db::value::{Object, RecordId, SurrealValue, Value};
use threski_ingest_code::{ingest_codebase, CodeAIClient, ProgressCallback};

// --- fakes ( MockDB / MockAI) ---

/// Fake `db.Executor`: captures every statement; `return_data` maps a
/// case-insensitive SQL substring to the rows returned for the first match.
struct MockDb {
    calls: Mutex<Vec<String>>,
    return_data: Vec<(String, Value)>,
}

impl MockDb {
    fn new(return_data: Vec<(&str, Value)>) -> Self {
        MockDb {
            calls: Mutex::new(Vec::new()),
            return_data: return_data
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn lookup(&self, sql: &str) -> Result<QueryOutcome, DbError> {
        let lower = sql.to_lowercase();
        for (pattern, val) in &self.return_data {
            if lower.contains(&pattern.to_lowercase()) {
                return Ok(QueryOutcome::Single(val.clone()));
            }
        }
        Ok(QueryOutcome::Single(Value::None))
    }
}

#[async_trait]
impl Executor for MockDb {
    async fn execute(&self, sql: &str) -> Result<QueryOutcome, DbError> {
        self.calls.lock().unwrap().push(sql.to_string());
        self.lookup(sql)
    }

    async fn smart_query(&self, sql: &str, _vars: Vars) -> Result<QueryOutcome, DbError> {
        self.calls.lock().unwrap().push(sql.to_string());
        self.lookup(sql)
    }
}

/// `MockAI: embedding functional, no rule-synthesis generator.
struct MockAi;

#[async_trait]
impl CodeAIClient for MockAi {
    fn is_embedding_functional(&self) -> bool {
        true
    }
}

// --- value builders ---

fn obj(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut o = Object::new();
    for (k, v) in pairs {
        o.insert(k, v);
    }
    Value::Object(o)
}

fn rows(values: Vec<Value>) -> Value {
    Value::from_t(values)
}

fn string_row(value: &str) -> Value {
    Value::from_t(value.to_string())
}

// --- helpers ( getFileHashHelper / getHash) ---

fn file_hash_of(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read file for hashing");
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn ingest(
    db: Arc<MockDb>,
    tmp_dir: &Path,
    cfg: Option<&Config>,
) -> Result<(), threski_ingest_code::CodeIngestError> {
    // config.Load for the default-config scenarios.
    let default_cfg;
    let cfg_ref = match cfg {
        Some(c) => Some(c),
        None => {
            default_cfg = Config::default();
            Some(&default_cfg)
        }
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            ingest_codebase(
                db as Arc<dyn Executor>,
                Arc::new(MockAi) as Arc<dyn CodeAIClient>,
                &tmp_dir.to_string_lossy(),
                "test-repo",
                cfg_ref,
                None::<ProgressCallback>,
            )
            .await
        })
}

fn temp_repo(name: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// True when some captured statement looks like the file record UPSERT for
/// `rel_path` ( wasProcessed helper).
fn was_processed(calls: &[String], tmp_dir: &Path, abs: &Path) -> bool {
    let rel = abs.strip_prefix(tmp_dir).unwrap().to_string_lossy();
    let needle = format!(
        "source_file:⟨{}_{}",
        sanitize_id("test-repo"),
        sanitize_id(&rel)
    );
    calls.iter().any(|sql| sql.contains(&needle))
}

#[test]
fn sanitize_id_matches_reference_contract() {
    // TestSanitizeID (threski-db owns the implementation; the
    // code lane pins the call contract).
    assert_eq!(
        threski_db::ids::sanitize_id("c:\\Users\\Foo\\Code.cs"),
        "c_users_foo_code_cs"
    );
}

#[test]
fn ingest_codebase_delta() {
    let tmp_dir = temp_repo("repo_delta");
    let file_path = tmp_dir.join("main.go");
    std::fs::write(&file_path, "package main\nfunc main() {}").unwrap();

    let expected_hash = file_hash_of(&file_path);
    let file_id = format_record_id(
        "source_file",
        &format!(
            "{}_{}_{}",
            sanitize_id("test-repo"),
            sanitize_id("main.go"),
            expected_hash
        ),
    );
    let cfg = Config::default();

    // Skip Unchanged: DB hash matches → zero chunk writes.
    {
        let mock_db = Arc::new(MockDb::new(vec![(
            &format!("SELECT hash FROM {file_id}"),
            rows(vec![obj([("hash", string_row(&expected_hash))])]),
        )]));
        ingest(mock_db.clone(), &tmp_dir, Some(&cfg)).expect("IngestCodebase failed");

        assert!(
            !mock_db
                .calls()
                .iter()
                .any(|sql| sql.contains("UPSERT file_chunk")),
            "processed embedding (unoptimized) — expected 0 chunk updates, calls: {:?}",
            mock_db.calls()
        );
    }

    // Update Chunks: no hash match → chunks queued as pending.
    {
        let mock_db = Arc::new(MockDb::new(vec![]));
        ingest(mock_db.clone(), &tmp_dir, Some(&cfg)).expect("IngestCodebase failed");

        assert!(
            mock_db
                .calls()
                .iter()
                .any(|sql| sql.contains("UPSERT file_chunk:⟨") && sql.contains("batch_status='pending'")),
            "expected CREATE file_chunk statement with batch_status='pending', but not found. Calls: {:?}",
            mock_db.calls()
        );
    }
}

#[test]
fn ingest_codebase_exclusions() {
    // Structure:
    // (supported)
    // /node_modules/ (ignored dir) /lib.js inside
    // /package-lock.json (ignored file)
    // (too large)
    // /readme.txt (unsupported extension)
    let tmp_dir = temp_repo("repo_exclusions");

    let main_go = tmp_dir.join("main.go");
    std::fs::write(&main_go, "package main").unwrap();

    let ignored_dir = tmp_dir.join("node_modules");
    std::fs::create_dir_all(&ignored_dir).unwrap();
    std::fs::write(ignored_dir.join("lib.js"), "console.log()").unwrap();

    let lock_file = tmp_dir.join("package-lock.json");
    std::fs::write(&lock_file, "{}").unwrap();

    let large_file = tmp_dir.join("large.go");
    std::fs::write(&large_file, "a".repeat(200)).unwrap();

    let txt_file = tmp_dir.join("readme.txt");
    std::fs::write(&txt_file, "readme").unwrap();

    let cfg = Config {
        max_file_size: 100,
        ignored_dirs: vec!["node_modules".to_string()],
        ignored_files: vec!["package-lock.json".to_string()],
        supported_extensions: vec![".go".to_string(), ".js".to_string()],
        ..Config::default()
    };

    let mock_db = Arc::new(MockDb::new(vec![]));
    ingest(mock_db.clone(), &tmp_dir, Some(&cfg)).expect("IngestCodebase failed");

    let calls = mock_db.calls();
    assert!(
        was_processed(&calls, &tmp_dir, &main_go),
        "main.go should have been processed"
    );
    assert!(
        !was_processed(&calls, &tmp_dir, &ignored_dir.join("lib.js")),
        "node_modules/lib.js should have been ignored (ignored dir)"
    );
    assert!(
        !was_processed(&calls, &tmp_dir, &lock_file),
        "package-lock.json should have been ignored (ignored file)"
    );
    assert!(
        !was_processed(&calls, &tmp_dir, &large_file),
        "large.go should have been ignored (too large)"
    );
    assert!(
        !was_processed(&calls, &tmp_dir, &txt_file),
        "readme.txt should have been ignored (unsupported ext)"
    );
}

#[test]
fn ingest_codebase_pruning() {
    let tmp_dir = temp_repo("repo_pruning");

    // 1. A file ignored by .aiignore.
    let ignored_file = tmp_dir.join("secret.go");
    std::fs::write(&ignored_file, "secret").unwrap();
    std::fs::write(tmp_dir.join(".aiignore"), "secret.go").unwrap();

    // 2. Existing file that must be kept.
    let existing_file = tmp_dir.join("main.go");
    std::fs::write(&existing_file, "package main").unwrap();

    let ignored_rel = ignored_file
        .strip_prefix(&tmp_dir)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let ignored_id = format_record_id(
        "source_file",
        &format!("{}_{}", sanitize_id("test-repo"), sanitize_id(&ignored_rel)),
    );
    let deleted_id = format_record_id(
        "source_file",
        &format!("{}_{}", sanitize_id("test-repo"), sanitize_id("gone.go")),
    );
    let existing_id = format_record_id(
        "source_file",
        &format!("{}_{}", sanitize_id("test-repo"), sanitize_id("main.go")),
    );

    let deleted_file: PathBuf = tmp_dir.join("gone.go"); // not on disk
    let mock_db = Arc::new(MockDb::new(vec![(
        "SELECT id, path FROM source_file",
        rows(vec![
            obj([
                ("id", string_row(&ignored_id)),
                ("path", string_row(&ignored_file.to_string_lossy())),
            ]),
            obj([
                ("id", string_row(&deleted_id)),
                ("path", string_row(&deleted_file.to_string_lossy())),
            ]),
            obj([
                ("id", string_row(&existing_id)),
                ("path", string_row(&existing_file.to_string_lossy())),
            ]),
        ]),
    )]));
    ingest(mock_db.clone(), &tmp_dir, None).expect("Error");

    // Verify deletes: file-record DELETEs (not the file_chunk ones).
    let mut deleted_ids = HashMap::new();
    for sql in mock_db.calls() {
        if sql.starts_with("DELETE") && !sql.contains("file_chunk") {
            if sql.contains(&ignored_id) {
                deleted_ids.insert(ignored_id.clone(), true);
            }
            if sql.contains(&deleted_id) {
                deleted_ids.insert(deleted_id.clone(), true);
            }
            if sql.contains(&existing_id) {
                panic!("Existing file {existing_id} was deleted! SQL: {sql}");
            }
        }
    }
    assert!(
        deleted_ids.contains_key(&ignored_id),
        "ignored file was not deleted. Calls: {:?}",
        mock_db.calls()
    );
    assert!(
        deleted_ids.contains_key(&deleted_id),
        "deleted file (physically missing) was NOT deleted from DB! It should be removed as it's not in HEAD."
    );
}

#[test]
fn ingest_codebase_versioning() {
    // Scenario: content V1 → V2 → V1. V1 embeddings must not regenerate when
    // switching back (the hash-keyed count check is the cache).
    let tmp_dir = temp_repo("repo_versioning");
    let path = tmp_dir.join("main.go");
    let cfg = Config::default();

    let run = |content: &str, db: Arc<MockDb>| {
        std::fs::write(&path, content).unwrap();
        ingest(db, &tmp_dir, Some(&cfg)).expect("ingest failed");
    };

    let has_queue_call = |db: &MockDb| {
        db.calls().iter().any(|sql| {
            sql.contains("UPSERT file_chunk:⟨") && sql.contains("batch_status='pending'")
        })
    };

    // 1. Ingest V1 → queued.
    let mock_db = Arc::new(MockDb::new(vec![]));
    run("version1", mock_db.clone());
    assert!(
        has_queue_call(&mock_db),
        "expected V1 to be queued (CREATE file_chunk ... pending)"
    );

    // 2. Ingest V2 (different content) → queued.
    let mock_db = Arc::new(MockDb::new(vec![]));
    run("version2", mock_db.clone());
    assert!(has_queue_call(&mock_db), "expected V2 to be queued");

    // 3. Ingest V1 again: the count query reports existing embeddings.
    let mock_db = Arc::new(MockDb::new(vec![(
        "SELECT count()",
        rows(vec![obj([(
            "count",
            Value::Number(surrealdb::types::Number::Int(5)),
        )])]),
    )]));
    run("version1", mock_db.clone());
    for sql in mock_db.calls() {
        assert!(
            !(sql.contains("UPSERT file_chunk:⟨") && sql.contains("batch_status='pending'")),
            "expected 0 queuing calls for returning to V1 (cache hit), got call: {sql}"
        );
    }
}

#[test]
fn ingest_codebase_batching() {
    let tmp_dir = temp_repo("repo_batching");

    // ~25 chunks of 1000 bytes → a single 100-chunk batch.
    let file_path = tmp_dir.join("batch.go");
    let line = format!("{}\n", "A".repeat(100));
    std::fs::write(&file_path, line.repeat(250)).unwrap();

    let mock_db = Arc::new(MockDb::new(vec![]));
    ingest(mock_db.clone(), &tmp_dir, None).expect("Error");

    let insert_count = mock_db
        .calls()
        .iter()
        .filter(|sql| sql.contains("BEGIN TRANSACTION;"))
        .count();
    assert_eq!(
        insert_count, 1,
        "expected 1 batched TRANSACTION, got {insert_count}"
    );
}

#[test]
fn ingest_codebase_pruning_query_syntax() {
    let tmp_dir = temp_repo("repo_prune_syntax");
    let mock_db = Arc::new(MockDb::new(vec![]));

    ingest(mock_db.clone(), &tmp_dir, None).expect("IngestCodebase failed");

    let mut found_query = false;
    for sql in mock_db.calls() {
        if sql.contains("SELECT id, path FROM source_file WHERE") {
            found_query = true;
            assert!(
                !sql.contains("BEGINSWITH"),
                "query uses deprecated BEGINSWITH syntax: {sql}"
            );
            assert!(
                sql.contains("string::starts_with"),
                "query should use string::starts_with syntax: {sql}"
            );
        }
    }
    assert!(
        found_query,
        "pruning query not found in calls: {:?}",
        mock_db.calls()
    );
}

/// The RecordId-decode path of prune rows (Q3): a `table:id` string decodes
/// through the same codec used by the live driver responses.
#[test]
fn prune_row_ids_decode_from_string_shape() {
    let id = RecordId::from_value(string_row("source_file:⟨test-repo_secret.go⟩"))
        .expect("string record id decodes");
    assert_eq!(
        threski_db::ids::format_record(&id),
        "source_file:⟨test-repo_secret.go⟩"
    );
    assert!(threski_db::ids::is_safe_record_id(
        "source_file:⟨test-repo_secret.go⟩"
    ));
}
