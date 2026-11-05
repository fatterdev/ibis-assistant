//! Live `apply_commit_txn` integration tests (SurrealDB sidecar required).
//!
//! `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`, root/root);
//! scratch database per test, removed at the end. With `IBIS_LIVE_REQUIRED=1`
//! an unreachable server FAILS instead of skipping.
//!
//! F07/C1: the transaction must persist every bound value (SurrealDB 4.0
//! evaluates an unbound param as NONE — the values are asserted, not just
//! the query success) and roll back completely on an SQL error.

use std::sync::Arc;

use surrealdb::types::SurrealValue;

use ibis_ai::{ChunkStore, CommitChunkWrite, CommitTxn};
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_server::assembly::DbChunkStore;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("commit_txn_live_{}", uuid::Uuid::new_v4().simple());
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

#[derive(Debug, SurrealValue)]
struct CommitChunkRow {
    content: Option<String>,
    commit_hash: Option<String>,
    commit_date: Option<String>,
    updated_at: Option<surrealdb::types::Datetime>,
    batch_status: Option<String>,
}

#[derive(Debug, SurrealValue)]
struct CommitRow {
    batch_status: Option<String>,
    batch_error: Option<String>,
}

/// Pending commit → chunk with content/hash/date/updated_at → completed.
#[tokio::test]
async fn commit_txn_persists_every_bound_value() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let commit_id = "commit:⟨livef07⟩";
    exec.smart_query(
        &format!("UPSERT {commit_id} SET hash = 'livef07', batch_status = 'pending';"),
        Vars::new(),
    )
    .await
    .expect("seed pending commit");

    let store = DbChunkStore(exec.clone());
    let txn = CommitTxn {
        chunk_writes: vec![CommitChunkWrite {
            commit_record: commit_id.to_string(),
            chunk_key: "livef07_chunk0".into(),
            content: "the commit meaning".into(),
            hash: "livef07".into(),
            date: Some("2026-11-05T10:00:00+00:00".into()),
        }],
        completed_commits: vec![commit_id.to_string()],
        failed_commits: vec![],
    };
    store
        .apply_commit_txn(&txn)
        .await
        .expect("apply commit txn");

    let sql = "SELECT content, commit_hash, commit_date, updated_at, batch_status FROM commit_chunk:⟨livef07_chunk0⟩;";
    let rows: Vec<CommitChunkRow> = exec
        .smart_query(sql, Vars::new())
        .await
        .expect("select chunk")
        .rows(sql)
        .expect("decode chunk");
    assert_eq!(rows.len(), 1, "chunk exists");
    let row = &rows[0];
    assert_eq!(
        row.content.as_deref(),
        Some("the commit meaning"),
        "content bound"
    );
    assert_eq!(row.commit_hash.as_deref(), Some("livef07"), "hash bound");
    assert!(
        row.commit_date
            .as_deref()
            .unwrap_or_default()
            .contains("2026-11-05"),
        "commit_date bound: {:?}",
        row.commit_date
    );
    assert!(
        row.updated_at.is_some(),
        "updated_at must be set (declared field, F07 schema)"
    );

    // The commit flipped to completed and carries the materialized edge.
    let sql = "SELECT batch_status FROM commit:⟨livef07⟩;";
    let rows: Vec<CommitRow> = exec
        .smart_query(sql, Vars::new())
        .await
        .expect("select commit")
        .rows(sql)
        .expect("decode commit");
    assert_eq!(rows[0].batch_status.as_deref(), Some("completed"));
    let edges = exec
        .execute("SELECT VALUE out FROM commit:⟨livef07⟩->has_commit_chunk;")
        .await
        .expect("edges");
    assert!(
        format!("{:?}", edges.first()).contains("livef07_chunk0"),
        "has_commit_chunk edge materialized: {:?}",
        edges.first()
    );

    cleanup(&exec, &db_name).await;
}

/// A failed commit flip carries the bound error code; a mid-transaction SQL
/// error rolls the whole transaction back (commit stays pending, no chunk).
#[tokio::test]
async fn commit_txn_failure_marks_failed_and_sql_error_rolls_back() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    let ok_commit = "commit:⟨okc⟩";
    let bad_commit = "commit:⟨badc⟩";
    exec.smart_query(
        &format!("UPSERT {ok_commit} SET hash = 'okc', batch_status = 'pending'; UPSERT {bad_commit} SET hash = 'badc', batch_status = 'pending';"),
        Vars::new(),
    )
    .await
    .expect("seed pending commits");

    let store = DbChunkStore(exec.clone());
    // okc completes; badc fails with an error code.
    store
        .apply_commit_txn(&CommitTxn {
            chunk_writes: vec![CommitChunkWrite {
                commit_record: ok_commit.into(),
                chunk_key: "okc_chunk0".into(),
                content: "ok meaning".into(),
                hash: "okc".into(),
                date: None,
            }],
            completed_commits: vec![ok_commit.to_string()],
            failed_commits: vec![(bad_commit.to_string(), "embed_failed".into())],
        })
        .await
        .expect("apply");

    let sql = "SELECT batch_status, batch_error FROM commit:⟨badc⟩;";
    let rows: Vec<CommitRow> = exec
        .smart_query(sql, Vars::new())
        .await
        .expect("select bad commit")
        .rows(sql)
        .expect("decode");
    assert_eq!(rows[0].batch_status.as_deref(), Some("failed"));
    assert_eq!(rows[0].batch_error.as_deref(), Some("embed_failed"));

    // SQL error mid-transaction: the chunk write references a commit record
    // of the wrong type — RELATE `has_commit_chunk` (IN commit) from a
    // memory record fails at runtime and the whole transaction rolls back.
    let poisoned = "memory:⟨poison⟩";
    exec.smart_query(
        &format!("UPSERT {poisoned} SET content = 'x';"),
        Vars::new(),
    )
    .await
    .expect("seed poisoned record");
    let err = store
        .apply_commit_txn(&CommitTxn {
            chunk_writes: vec![CommitChunkWrite {
                commit_record: poisoned.to_string(),
                chunk_key: "poison_chunk0".into(),
                content: "must roll back".into(),
                hash: "poison".into(),
                date: None,
            }],
            completed_commits: vec![],
            failed_commits: vec![],
        })
        .await;
    assert!(
        err.is_err(),
        "type mismatch must fail the transaction: {err:?}"
    );

    // Rollback: no chunk row for the poisoned hash, no has_commit_chunk edge.
    let sql = "SELECT count() FROM commit_chunk WHERE commit_hash = 'poison';";
    let rows: Vec<CountRow> = exec
        .smart_query(sql, Vars::new())
        .await
        .expect("count chunks")
        .rows(sql)
        .expect("decode count");
    assert_eq!(
        rows.first()
            .and_then(|r| r.count.clone())
            .and_then(|n| n.to_int())
            .unwrap_or(0),
        0,
        "rollback leaves no chunk"
    );
    let edges = exec
        .execute("SELECT VALUE id FROM has_commit_chunk;")
        .await
        .expect("edges");
    let text = format!("{:?}", edges.first());
    assert!(
        !text.contains("poison_chunk0"),
        "no edge may survive the rollback: {text}"
    );

    cleanup(&exec, &db_name).await;
}

#[derive(Debug, SurrealValue)]
struct CountRow {
    count: Option<surrealdb::types::Number>,
}
