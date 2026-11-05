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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ibis_core::config::Config;
use ibis_db::error::DbError;
use ibis_db::executor::{Executor, QueryOutcome, Vars};
use ibis_db::ids::{format_record_id, sanitize_id};
use ibis_db::value::{Object, RecordId, RecordIdKey, SurrealValue, Value};
use ibis_ingest_code::{ingest_codebase, CodeAIClient, ProgressCallback};
use surrealdb::types::Array;

// --- fakes ( MockDB / MockAI) ---

/// Fake `db.Executor`: captures every statement; `return_data` maps a
/// case-insensitive SQL substring to the rows returned for the first match.
/// `fail_on` injects a failure on statements containing the substring.
struct MockDb {
    calls: Mutex<Vec<String>>,
    return_data: Vec<(String, Value)>,
    fail_on: Option<String>,
}

impl MockDb {
    fn new(return_data: Vec<(&str, Value)>) -> Self {
        MockDb {
            calls: Mutex::new(Vec::new()),
            return_data: return_data
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            fail_on: None,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn lookup(&self, sql: &str) -> Result<QueryOutcome, DbError> {
        if let Some(fail) = &self.fail_on {
            if sql.contains(fail.as_str()) {
                return Err(DbError::Query {
                    message: format!("injected failure on {fail}"),
                    duration_ms: 0,
                });
            }
        }
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
) -> Result<(), ibis_ingest_code::CodeIngestError> {
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
/// `rel_path` ( wasProcessed helper). WQ-7: source_file ids are arrays
/// `[repo, file_path]` — built with the ingest's own helper.
fn was_processed(calls: &[String], tmp_dir: &Path, abs: &Path) -> bool {
    let rel = abs.strip_prefix(tmp_dir).unwrap().to_string_lossy();
    let repo_ref = format_record_id("repo", &sanitize_id("test-repo"));
    let needle_prefix = ibis_db::ids::source_file_id(&repo_ref, &rel);
    let needle = needle_prefix.trim_end_matches(']');
    calls.iter().any(|sql| sql.contains(needle))
}

#[test]
fn sanitize_id_matches_reference_contract() {
    // TestSanitizeID (ibis-db owns the implementation; the
    // code lane pins the call contract).
    assert_eq!(
        ibis_db::ids::sanitize_id("c:\\Users\\Foo\\Code.cs"),
        "c_users_foo_code_cs"
    );
}

#[test]
fn ingest_codebase_delta() {
    let tmp_dir = temp_repo("repo_delta");
    let file_path = tmp_dir.join("main.go");
    std::fs::write(&file_path, "package main\nfunc main() {}").unwrap();

    let expected_hash = file_hash_of(&file_path);
    // WQ-7: source_file ids are arrays `[repo, file_path]` (no hash suffix),
    // built with the ingest's own helper.
    let file_id = ibis_db::ids::source_file_id(
        &format_record_id("repo", &sanitize_id("test-repo")),
        "main.go",
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
                .any(|sql| sql.contains("UPSERT file_chunk:[") && sql.contains("batch_status='pending'")),
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
    // WQ-7 + F02: source_file ids are arrays `[repo, file_path]` with the
    // raw path; the mock returns decoded RecordId values (the live CBOR
    // path), and the DELETE lists use the canonical `to_sql` rendering
    // (bare repo key, quoted raw path).
    let repo_ref = format_record_id("repo", &sanitize_id("test-repo"));
    let source_file_value =
        |rel: &str| Value::RecordId(ibis_db::ids::source_file_record(&repo_ref, rel));
    let ignored_id = ibis_db::ids::source_file_id(&repo_ref, &ignored_rel);
    let deleted_id = ibis_db::ids::source_file_id(&repo_ref, "gone.go");
    let existing_id = ibis_db::ids::source_file_id(&repo_ref, "main.go");

    let deleted_file: PathBuf = tmp_dir.join("gone.go"); // not on disk
    let mock_db = Arc::new(MockDb::new(vec![(
        "SELECT id, path FROM source_file",
        rows(vec![
            obj([
                ("id", source_file_value(&ignored_rel)),
                ("path", string_row(&ignored_file.to_string_lossy())),
            ]),
            obj([
                ("id", source_file_value("gone.go")),
                ("path", string_row(&deleted_file.to_string_lossy())),
            ]),
            obj([
                ("id", source_file_value("main.go")),
                ("path", string_row(&existing_file.to_string_lossy())),
            ]),
        ]),
    )]));
    ingest(mock_db.clone(), &tmp_dir, None).expect("Error");

    // Verify deletes: with the atomic transaction the file ids appear in the
    // single BEGIN…COMMIT statement that also deletes their chunks.
    let all_calls = mock_db.calls();
    let txn_deletes: Vec<&String> = all_calls
        .iter()
        .filter(|sql| sql.contains("BEGIN TRANSACTION;") && sql.contains("DELETE ["))
        .collect();
    assert_eq!(
        txn_deletes.len(),
        1,
        "one prune transaction: {txn_deletes:?}"
    );
    assert!(
        txn_deletes[0].contains(&ignored_id) && txn_deletes[0].contains(&deleted_id),
        "obsolete files are deleted atomically with their chunks: {}",
        txn_deletes[0]
    );
    assert!(
        !txn_deletes[0].contains(&existing_id),
        "existing file {existing_id} must not be deleted: {}",
        txn_deletes[0]
    );
    assert!(
        !all_calls.iter().any(|sql| sql.starts_with("DELETE [")
            || (sql.starts_with("DELETE ") && !sql.contains("file_chunk WHERE"))),
        "no file-only DELETE may run outside the transaction: {all_calls:?}"
    );
}

#[test]
fn revert_rebuilds_full_manifest_per_chunk() {
    // F10/B3 (replaces the count()-cache test that rewarded the defect):
    // V1 → V2 → V1. On the return to V1 the chunk set on disk is a MIX of
    // residual V1 chunks (keys V2 never reclaimed) and V2 chunks (keys
    // re-written by V2). The ingest must rebuild the full manifest:
    // residual-V1-hash chunks are resumes (untouched, embedding preserved),
    // V2-hash chunks are re-queued with `embedding = NONE` and V1 content.
    let tmp_dir = temp_repo("repo_revert");
    let path = tmp_dir.join("main.go");
    let cfg = Config::default();

    let v1 = format!("{}\n{}\n", "A".repeat(600), "B".repeat(600));
    let v1_hash = {
        std::fs::write(&path, &v1).unwrap();
        file_hash_of(&path)
    };
    let file_id = ibis_db::ids::source_file_id(
        &format_record_id("repo", &sanitize_id("test-repo")),
        "main.go",
    );
    let sf_record = ibis_db::ids::source_file_record(
        &format_record_id("repo", &sanitize_id("test-repo")),
        "main.go",
    );
    let chunk_row = |line: i64, hash: &str| {
        obj([
            (
                "id",
                Value::RecordId(RecordId::new(
                    "file_chunk",
                    RecordIdKey::Array(Array::from(vec![
                        Value::RecordId(sf_record.clone()),
                        Value::Number(surrealdb::types::Number::Int(line)),
                    ])),
                )),
            ),
            ("hash", string_row(hash)),
            ("batch_status", string_row("completed")),
        ])
    };
    // V1 → V2 → back to V1 on disk; the DB still holds the mixed state:
    // line 1 was re-written by V2 (hash V2), line 2 still carries the
    // original V1 hash.
    let v2_hash = "0".repeat(64);
    std::fs::write(&path, &v1).unwrap();
    let state_rows = rows(vec![chunk_row(1, &v2_hash), chunk_row(2, &v1_hash)]);
    let mock_db = Arc::new(MockDb::new(vec![(
        "SELECT id, hash, batch_status FROM file_chunk",
        state_rows,
    )]));
    ingest(mock_db.clone(), &tmp_dir, Some(&cfg)).expect("ingest failed");

    let calls = mock_db.calls();
    let upserts: Vec<&String> = calls
        .iter()
        .filter(|sql| sql.contains("UPSERT file_chunk:["))
        .collect();
    assert_eq!(
        upserts.len(),
        1,
        "exactly one chunk must be re-queued (the V2-hash one), calls: {calls:?}"
    );
    assert!(
        upserts[0].contains(file_id.as_str()) && upserts[0].contains(", 1]"),
        "the V2-hash chunk (line 1) is the re-queued one: {}",
        upserts[0]
    );
    assert!(
        upserts[0].contains("embedding = NONE"),
        "a reused key with new content must invalidate the previous embedding: {}",
        upserts[0]
    );
    assert!(
        upserts[0].contains("batch_status='pending'"),
        "re-queued chunk goes back to pending: {}",
        upserts[0]
    );
    // Exactly one chunk UPSERT total: the line-2 resume chunk is untouched.
    assert_eq!(
        calls
            .iter()
            .filter(|sql| sql.contains("UPSERT file_chunk:["))
            .count(),
        1,
        "resume chunk with current hash must be left untouched: {calls:?}"
    );
    // The file hash advances only after the manifest was fully queued, and
    // the stale-chunk cleanup shares its transaction (atomic finalize: a
    // hash advanced without the cleanup would pin stale chunks forever via
    // the next-pass early return).
    let finalize = calls.iter().find(|sql| {
        sql.contains("BEGIN TRANSACTION;")
            && sql.contains(&format!("UPSERT {file_id} SET hash"))
            && sql.contains("DELETE file_chunk WHERE file =")
            && sql.contains("hash != $hash")
    });
    assert!(
        finalize.is_some(),
        "hash UPSERT and stale-chunk cleanup must share one transaction: {calls:?}"
    );
    // Ordering: every chunk write precedes the finalize transaction.
    let last_chunk_write = calls
        .iter()
        .position(|sql| sql.contains("UPSERT file_chunk:["))
        .unwrap();
    let finalize_pos = calls
        .iter()
        .position(|sql| sql.contains(&format!("UPSERT {file_id} SET hash")))
        .unwrap();
    assert!(
        last_chunk_write < finalize_pos,
        "file hash must advance after chunk queuing, not before"
    );
}

#[test]
fn prune_transaction_is_atomic_and_propagates() {
    // F12/B4: chunk + file deletion run in one transaction; a failure on
    // either statement propagates (no silent orphans) and the retry
    // completes.
    let tmp_dir = temp_repo("repo_prune_atomic");
    let main_go = tmp_dir.join("main.go");
    std::fs::write(&main_go, "package main").unwrap();
    let gone: PathBuf = tmp_dir.join("gone.go"); // not on disk

    let ignored_rel = "gone.go";
    let repo_ref = format_record_id("repo", &sanitize_id("test-repo"));
    let deleted_id = ibis_db::ids::source_file_id(&repo_ref, ignored_rel);
    let source_file_value =
        Value::RecordId(ibis_db::ids::source_file_record(&repo_ref, ignored_rel));
    let mock_rows = vec![(
        "SELECT id, path FROM source_file",
        rows(vec![obj([
            ("id", source_file_value),
            ("path", string_row(&gone.to_string_lossy())),
        ])]),
    )];

    // Failure on the chunk DELETE: the error propagates and the transaction
    // is a single statement (no partial deletes possible).
    let mut mock_db = Arc::new(MockDb::new(mock_rows.clone()));
    Arc::get_mut(&mut mock_db).unwrap().fail_on = Some("DELETE file_chunk".to_string());
    let err = ingest(mock_db.clone(), &tmp_dir, None).expect_err("prune failure must propagate");
    assert!(
        err.to_string().contains("prune transaction failed"),
        "{err}"
    );
    let calls = mock_db.calls();
    assert!(
        !calls.iter().any(|sql| sql.starts_with("DELETE [")),
        "no file-only DELETE may run outside the transaction: {calls:?}"
    );
    assert!(
        calls.iter().any(|sql| sql.contains("BEGIN TRANSACTION;")
            && sql.contains("DELETE file_chunk WHERE file IN [")
            && sql.contains(&deleted_id)),
        "chunk + file deletes share one atomic transaction: {calls:?}"
    );

    // Failure on the file DELETE propagates the same way.
    let mut mock_db = Arc::new(MockDb::new(mock_rows.clone()));
    Arc::get_mut(&mut mock_db).unwrap().fail_on = Some(format!("DELETE [{deleted_id}]"));
    let err = ingest(mock_db.clone(), &tmp_dir, None).expect_err("prune failure must propagate");
    assert!(
        err.to_string().contains("prune transaction failed"),
        "{err}"
    );

    // Retry with the failure cleared: the whole batch completes.
    let mock_db = Arc::new(MockDb::new(mock_rows));
    ingest(mock_db.clone(), &tmp_dir, None).expect("retry succeeds");
    assert!(
        mock_db
            .calls()
            .iter()
            .any(|sql| sql.contains("COMMIT TRANSACTION;") && sql.contains(&deleted_id)),
        "retry deletes the obsolete file: {:?}",
        mock_db.calls()
    );
}

/// The file-hash UPSERT and the stale-chunk cleanup share one transaction
/// (crash-window fix): a failure on the cleanup part fails the whole
/// finalize — the hash must never advance without the cleanup, or the next
/// pass would early-return on the matching hash and leave stale chunks
/// served forever.
#[test]
fn finalize_transaction_is_atomic_and_propagates() {
    let tmp_dir = temp_repo("repo_finalize_atomic");
    let file_path = tmp_dir.join("main.go");
    std::fs::write(&file_path, "package main\nfunc main() {}").unwrap();

    let file_id = ibis_db::ids::source_file_id(
        &format_record_id("repo", &sanitize_id("test-repo")),
        "main.go",
    );

    // Failure on the stale-chunk DELETE inside the finalize transaction.
    // Per-file failures are logged by the worker (the pipeline keeps going),
    // so the atomicity is verified from the captured statements.
    let mut mock_db = Arc::new(MockDb::new(vec![]));
    Arc::get_mut(&mut mock_db).unwrap().fail_on = Some("hash != $hash".to_string());
    ingest(mock_db.clone(), &tmp_dir, None).expect("walk completes");
    let calls = mock_db.calls();
    // The finalize was attempted as ONE statement (hash UPSERT + cleanup
    // inside BEGIN/COMMIT) — no hash UPSERT may have gone out without the
    // cleanup in the same statement (pre-fix the UPSERT ran alone first).
    assert!(
        calls.iter().any(|sql| sql.contains("BEGIN TRANSACTION;")
            && sql.contains(&format!("UPSERT {file_id} SET hash"))
            && sql.contains("hash != $hash")),
        "hash UPSERT + cleanup were submitted together: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|sql| sql.contains(&format!("UPSERT {file_id} SET hash"))
                && !sql.contains("hash != $hash")),
        "no hash UPSERT outside the atomic finalize transaction: {calls:?}"
    );

    // Retry with the failure cleared: the whole transaction completes.
    let mock_db = Arc::new(MockDb::new(vec![]));
    ingest(mock_db.clone(), &tmp_dir, None).expect("retry succeeds");
    assert!(
        mock_db
            .calls()
            .iter()
            .any(|sql| sql.contains("BEGIN TRANSACTION;")
                && sql.contains(&format!("UPSERT {file_id} SET hash"))
                && sql.contains("COMMIT TRANSACTION;")),
        "retry finalizes the file version atomically: {:?}",
        mock_db.calls()
    );
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

    // Chunk-batch transactions only: the finalize transaction (hash UPSERT +
    // stale cleanup) is expected on top of the chunk batches.
    let insert_count = mock_db
        .calls()
        .iter()
        .filter(|sql| sql.contains("BEGIN TRANSACTION;") && sql.contains("UPSERT file_chunk:["))
        .count();
    assert_eq!(
        insert_count, 1,
        "expected 1 batched chunk TRANSACTION, got {insert_count}"
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
        ibis_db::ids::format_record(&id),
        "source_file:⟨test-repo_secret.go⟩"
    );
    assert!(ibis_db::ids::is_safe_record_id(
        "source_file:⟨test-repo_secret.go⟩"
    ));
}
