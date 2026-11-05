//! Live code-ingest integration tests (SurrealDB sidecar required).
//!
//! `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`, root/root)
//! selects the server; every test uses a distinct scratch database and
//! removes it at the end. With `IBIS_LIVE_REQUIRED=1` an unreachable server
//! FAILS the test instead of skipping (CI live gate).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use surrealdb::types::{RecordId, SurrealValue, Value};

use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::ids::{file_chunk_id, source_file_id};
use ibis_db::schema;
use ibis_ingest_code::{ingest_codebase, CodeAIClient};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

/// Embedding-functional fake (no network): ingest queues chunks, nothing
/// embeds them unless the test writes embeddings directly.
struct LiveAi;

#[async_trait]
impl CodeAIClient for LiveAi {
    fn is_embedding_functional(&self) -> bool {
        true
    }
}

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("ingest_live_{}", uuid::Uuid::new_v4().simple());
    let exec =
        match SurrealDbExecutor::connect(&url, "ibis_db_tests", &db_name, "root", "root").await {
            Ok(exec) => exec,
            Err(err) => {
                if std::env::var("IBIS_LIVE_REQUIRED").as_deref() == Ok("1") {
                    panic!("IBIS_LIVE_REQUIRED=1 but SurrealDB is unreachable: {err}");
                }
                eprintln!("live: SurrealDB not reachable ({err}), skipping");
                return None;
            }
        };
    Some((Arc::new(exec) as Arc<dyn Executor>, db_name))
}

async fn cleanup(exec: &Arc<dyn Executor>, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

fn temp_repo(name: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn make_cfg(exts: &[&str]) -> ibis_core::config::Config {
    ibis_core::config::Config {
        supported_extensions: exts.iter().map(|s| s.to_string()).collect(),
        ..ibis_core::config::Config::default()
    }
}

/// Runs one ingest pass over the temp repo.
async fn ingest(db: Arc<dyn Executor>, repo: &Path, cfg: &ibis_core::config::Config) {
    ingest_codebase(
        db,
        Arc::new(LiveAi),
        repo.to_string_lossy().as_ref(),
        "liverem",
        Some(cfg),
        None,
    )
    .await
    .expect("ingest_codebase");
}

#[derive(Debug, SurrealValue)]
struct ChunkRow {
    id: RecordId,
    content: Option<String>,
    hash: Option<String>,
    start_line: Option<surrealdb::types::Number>,
    end_line: Option<surrealdb::types::Number>,
    batch_status: Option<String>,
    embedding: Option<Value>,
}

#[derive(Debug, SurrealValue)]
struct CountRow {
    count: Option<surrealdb::types::Number>,
}

#[derive(Debug, SurrealValue)]
struct HashRow {
    hash: Option<String>,
}

fn fallback_chunk_spans(text: &str, size: usize) -> Vec<(usize, usize)> {
    // Mirrors ingest::chunk_content: chunks split when the current buffer
    // plus the next line would exceed `size`; spans are contiguous.
    let lines: Vec<&str> = text.lines().collect();
    let mut spans = Vec::new();
    let mut current_len = 0usize;
    let mut start = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        if current_len + line.len() > size && current_len > 0 {
            spans.push((start + 1, idx));
            start = idx;
            current_len = 0;
        }
        current_len += line.len() + 1;
    }
    if current_len > 0 {
        spans.push((start + 1, lines.len()));
    }
    spans
}

async fn chunk_rows(exec: &Arc<dyn Executor>, file_id: &str) -> Vec<ChunkRow> {
    let sql = format!(
        "SELECT id, content, hash, start_line, end_line, batch_status, embedding FROM file_chunk WHERE file = {file_id} ORDER BY start_line;"
    );
    exec.smart_query(&sql, Vars::new())
        .await
        .expect("select chunks")
        .rows(&sql)
        .expect("decode chunks")
}

async fn count_chunks(exec: &Arc<dyn Executor>, file_id: &str) -> i64 {
    let sql = format!("SELECT count() FROM file_chunk WHERE file = {file_id};");
    let rows: Vec<CountRow> = exec
        .smart_query(&sql, Vars::new())
        .await
        .expect("count chunks")
        .rows(&sql)
        .expect("decode count");
    rows.first()
        .and_then(|r| r.count.clone())
        .and_then(|n| n.to_int())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// B1: fallback chunks carry unique, monotonic start lines — no key
/// collapse; contents match the line spans; the file row records the hash.
///
/// Uses an extension ast-grep has no rules for (`.txt` here is excluded from
/// the config; `.go` files WITHOUT parseable declarations fall back to
/// whole-file chunking — the `sg` rule set only chunks declared symbols).
#[tokio::test]
async fn fallback_chunks_get_unique_start_lines() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let repo = temp_repo("fallback_lines");
    let cfg = make_cfg(&[".go"]);
    // ~60-char lines → 1000-byte chunks span ~16 lines; 25 chunks.
    let line = format!("{}\n", "x".repeat(58));
    let content: String = line.repeat(25 * 16);
    std::fs::write(repo.join("bulk.go"), &content).unwrap();

    ingest(exec.clone(), &repo, &cfg).await;

    let file_id = source_file_id("repo:⟨liverem⟩", "bulk.go");
    let rows = chunk_rows(&exec, &file_id).await;
    let spans = fallback_chunk_spans(&content, 1000);
    assert_eq!(
        rows.len(),
        spans.len(),
        "one record per fallback chunk (no key collapse): {} vs {spans:?}",
        rows.len()
    );

    let mut keys = std::collections::HashSet::new();
    for (i, row) in rows.iter().enumerate() {
        let rendered = ibis_db::ids::format_record(&row.id);
        assert!(
            keys.insert(rendered.clone()),
            "duplicate chunk key: {rendered}"
        );
        let start = row
            .start_line
            .as_ref()
            .and_then(|n| n.to_int())
            .expect("start_line");
        let end = row
            .end_line
            .as_ref()
            .and_then(|n| n.to_int())
            .expect("end_line");
        assert_eq!(start as usize, spans[i].0, "chunk {i} start line");
        assert_eq!(end as usize, spans[i].1, "chunk {i} end line");
        let content_text = row.content.as_deref().expect("content");
        assert!(!content_text.trim().is_empty());
        assert_eq!(row.batch_status.as_deref(), Some("pending"));
    }
    // Distinct ids are interpolatable record references.
    for key in &keys {
        assert!(ibis_db::ids::is_safe_record_id(key), "{key}");
    }

    // The file row records the hash (B3: after the full manifest queued).
    let sql = format!("SELECT hash FROM {file_id};");
    let hash_rows: Vec<HashRow> = exec
        .smart_query(&sql, Vars::new())
        .await
        .expect("select file")
        .rows(&sql)
        .expect("decode file");
    let hash = hash_rows[0].hash.clone().expect("hash");
    let bytes = std::fs::read(repo.join("bulk.go")).unwrap();
    assert_eq!(
        hash,
        sha256_hex(&bytes),
        "file hash reflects the ingested version"
    );

    cleanup(&exec, &db_name).await;
}

/// B1: more than one batch (100 chunks per batch) — every chunk lands.
#[tokio::test]
async fn chunks_span_multiple_batches() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let repo = temp_repo("many_batches");
    let cfg = make_cfg(&[".go"]);
    let line = format!("{}\n", "y".repeat(58));
    let total: String = line.repeat(130 * 16); // 130 fallback chunks → 2 batches
    std::fs::write(repo.join("huge.go"), &total).unwrap();

    ingest(exec.clone(), &repo, &cfg).await;

    let file_id = source_file_id("repo:⟨liverem⟩", "huge.go");
    let rows = chunk_rows(&exec, &file_id).await;
    assert_eq!(rows.len(), 130, "every chunk across every batch persisted");
    let mut starts: Vec<i64> = rows
        .iter()
        .map(|r| {
            r.start_line
                .as_ref()
                .and_then(|n| n.to_int())
                .expect("start")
        })
        .collect();
    starts.sort_unstable();
    starts.dedup();
    assert_eq!(starts.len(), 130, "all start lines distinct");

    cleanup(&exec, &db_name).await;
}

/// B2/B3: V1 → V2 → V1 with shifted chunk boundaries and different chunk
/// counts — the final state is exactly the V1 manifest (content/hash V1,
/// embeddings invalidated by the version changes, no V2 residue), and a
/// same-version resume preserves completed embeddings.
#[tokio::test]
async fn version_round_trip_leaves_a_coherent_manifest() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let repo = temp_repo("version_trip");
    let cfg = make_cfg(&[".go"]);
    let path = repo.join("main.go");

    // V1: 60-char lines → chunks at lines 1, 17, 33, 49, 65 (fallback).
    let v1_text: String = ("x".repeat(58) + "\n").repeat(70);
    // V2: 30-char lines → chunks at lines 1, 35, 69 (different boundaries).
    let v2_text: String = ("y".repeat(28) + "\n").repeat(80);

    let ingested_hash = |p: &Path| sha256_hex(&std::fs::read(p).unwrap());

    // V1
    std::fs::write(&path, &v1_text).unwrap();
    let h1 = ingested_hash(&path);
    ingest(exec.clone(), &repo, &cfg).await;

    // Simulate the batch manager on one chunk: embedding + completed.
    let file_id = source_file_id("repo:⟨liverem⟩", "main.go");
    let chunk1 = file_chunk_id(&file_id, 1);
    let fake_vec: Vec<Value> = (0..768)
        .map(|i| Value::from_t((i as f32) * 0.001))
        .collect();
    exec.smart_query(
        &format!("UPDATE {chunk1} SET embedding = $vec, batch_status = 'completed';"),
        Vars::from_iter([("vec".to_string(), Value::from_t(fake_vec.clone()))]),
    )
    .await
    .expect("simulate embedding");

    // V2 (different boundaries): the reused keys must drop the V1 embedding.
    std::fs::write(&path, &v2_text).unwrap();
    let h2 = ingested_hash(&path);
    ingest(exec.clone(), &repo, &cfg).await;

    let rows = chunk_rows(&exec, &file_id).await;
    assert!(
        rows.iter().all(|r| r.embedding.is_none()),
        "every V2 chunk must carry NO inherited embedding"
    );
    assert!(
        rows.iter().all(|r| r.hash.as_deref() == Some(h2.as_str())),
        "every V2 chunk records the V2 hash"
    );
    // V2 boundaries replace the V1 ones: old keys not reclaimed are gone.
    let starts: Vec<i64> = rows
        .iter()
        .map(|r| {
            r.start_line
                .as_ref()
                .and_then(|n| n.to_int())
                .expect("start")
        })
        .collect();
    let expected_v2: Vec<i64> = fallback_chunk_spans(&v2_text, 1000)
        .into_iter()
        .map(|(s, _)| s as i64)
        .collect();
    assert_eq!(starts, expected_v2, "V2 manifest");

    // V1 again: full manifest rebuilt; the completed V2 embedding is gone
    // for reclaimed keys and no V2 chunk survives outside the manifest.
    std::fs::write(&path, &v1_text).unwrap();
    ingest(exec.clone(), &repo, &cfg).await;

    let rows = chunk_rows(&exec, &file_id).await;
    let starts: Vec<i64> = rows
        .iter()
        .map(|r| {
            r.start_line
                .as_ref()
                .and_then(|n| n.to_int())
                .expect("start")
        })
        .collect();
    let expected_v1: Vec<i64> = fallback_chunk_spans(&v1_text, 1000)
        .into_iter()
        .map(|(s, _)| s as i64)
        .collect();
    assert_eq!(starts, expected_v1, "V1 manifest restored: {starts:?}");
    assert!(
        rows.iter().all(|r| r.hash.as_deref() == Some(h1.as_str())),
        "all chunks carry the V1 hash"
    );
    assert!(
        rows.iter().all(|r| r.embedding.is_none()),
        "round trip invalidates stale embeddings everywhere"
    );
    assert!(
        rows.iter()
            .all(|r| r.batch_status.as_deref() == Some("pending")),
        "all chunks re-queued for embedding"
    );

    // Resume of the SAME version (file hash was not advanced — simulated
    // interrupted pass): matching chunks keep their state.
    exec.smart_query(
        &format!("UPSERT {file_id} SET hash = 'stale', path = $p, rel_path = $r, repo_name = $n, is_head = true;"),
        Vars::from_iter([
            (
                "p".to_string(),
                Value::from_t(repo.join("main.go").to_string_lossy().to_string()),
            ),
            ("r".to_string(), Value::from_t("main.go")),
            ("n".to_string(), Value::from_t("liverem")),
        ]),
    )
    .await
    .expect("downgrade file hash");
    // Re-complete one chunk embedding to prove resume keeps it.
    exec.smart_query(
        &format!("UPDATE {chunk1} SET embedding = $vec, batch_status = 'completed';"),
        Vars::from_iter([("vec".to_string(), Value::from_t(fake_vec.clone()))]),
    )
    .await
    .expect("re-complete chunk 1");
    ingest(exec.clone(), &repo, &cfg).await;

    let rows = chunk_rows(&exec, &file_id).await;
    let completed = rows
        .iter()
        .find(|r| r.batch_status.as_deref() == Some("completed"))
        .expect("the completed chunk survives the resume");
    assert_eq!(
        ibis_db::ids::format_record(&completed.id),
        chunk1,
        "resume preserves the completed chunk untouched"
    );
    assert!(
        completed.embedding.is_some(),
        "resume must not invalidate a same-version embedding"
    );
    assert!(
        rows.iter().all(|r| r.hash.as_deref() == Some(h1.as_str())),
        "resume keeps the version coherent"
    );

    cleanup(&exec, &db_name).await;
}

/// B4: pruning removes obsolete files together with their chunks (no
/// orphans); surviving files stay intact.
#[tokio::test]
async fn prune_removes_files_and_chunks_without_orphans() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let repo = temp_repo("prune_live");
    let cfg = make_cfg(&[".go"]);
    std::fs::write(repo.join("keep.go"), "package main\nfunc keep() {}\n").unwrap();
    std::fs::write(repo.join("gone.go"), "package main\nfunc gone() {}\n").unwrap();

    ingest(exec.clone(), &repo, &cfg).await;

    let sf_gone = source_file_id("repo:⟨liverem⟩", "gone.go");
    assert!(
        count_chunks(&exec, &sf_gone).await > 0,
        "gone.go chunks exist"
    );

    // File leaves the tree (removed from disk) → the next ingest prunes it.
    std::fs::remove_file(repo.join("gone.go")).unwrap();
    ingest(exec.clone(), &repo, &cfg).await;

    assert_eq!(
        count_chunks(&exec, &sf_gone).await,
        0,
        "no chunk orphans may survive the file"
    );
    let sql = format!("SELECT count() FROM {sf_gone};");
    let rows: Vec<CountRow> = exec
        .smart_query(&sql, Vars::new())
        .await
        .expect("count file")
        .rows(&sql)
        .expect("decode count");
    // The record is gone: an empty row set (missing record) or count 0.
    assert_eq!(
        rows.first()
            .and_then(|r| r.count.clone())
            .and_then(|n| n.to_int())
            .unwrap_or(0),
        0
    );

    // The surviving file is untouched.
    let sf_keep = source_file_id("repo:⟨liverem⟩", "keep.go");
    assert!(count_chunks(&exec, &sf_keep).await > 0, "kept file intact");

    cleanup(&exec, &db_name).await;
}
