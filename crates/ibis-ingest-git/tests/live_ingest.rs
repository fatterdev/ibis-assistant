//! Live SurrealDB regression tests for the history ingest (
//! E-A30/E-A31): verifies node rows are actually created (UPSERT fix), edges
//! resolve, and re-ingest skips existing commits. Skips gracefully when the
//! sidecar (`ws://127.0.0.1:38000`, root/root) is unreachable.

mod common;

use std::sync::Arc;

use common::init_repo;
use ibis_db::value::{ToSql, Value};

use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_ingest_git::ingest_repo;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

async fn connect_scratch() -> Option<(SurrealDbExecutor, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("lanee_{}", uuid::Uuid::new_v4().simple());
    let exec =
        match SurrealDbExecutor::connect(&url, "ibis_db_tests", &db_name, "root", "root").await {
            Ok(exec) => exec,
            Err(err) => {
                eprintln!("live: SurrealDB not reachable ({err}), skipping");
                return None;
            }
        };
    Some((exec, db_name))
}

async fn cleanup(exec: &SurrealDbExecutor, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

#[tokio::test]
async fn live_ingest_creates_rows_and_dedupes() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    let repo = init_repo("live").await;
    repo.commit_file("main.go", "package main", "Initial commit")
        .await;
    repo.commit_file("main.go", "package main\n// update", "Fix bug #42")
        .await;

    // Ingest with no ticket ingester → bare issue rows + implements edges.
    ingest_repo(
        Arc::new(exec.clone()) as Arc<dyn Executor>,
        None,
        &repo.path,
        "live-repo",
        4,
        &[],
    )
    .await
    .expect("ingest failed");

    // E-A30 regression guard: commits, authors, files and the issue row exist.
    let commits = exec
        .execute("SELECT hash, batch_status FROM commit;")
        .await
        .expect("select commits");
    let text = commits.first().map(|v| v.to_sql()).unwrap_or_default();
    assert_eq!(
        text.matches("batch_status: 'pending'").count(),
        2,
        "commits: {text}"
    );

    let authors = exec
        .execute("SELECT * FROM author;")
        .await
        .expect("select authors");
    let text = authors.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(text.contains("Test User"), "authors: {text}");

    let files = exec
        .execute("SELECT path FROM source_file;")
        .await
        .expect("select files");
    let text = files.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(text.contains("main.go"), "files: {text}");

    let issue = exec
        .execute("SELECT provider, external_key, subject FROM issue;")
        .await
        .expect("select issue");
    let text = issue.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(
        text.contains("provider: 'redmine'") && text.contains("external_key: '42'"),
        "issue: {text}"
    );

    // changed edges carry impact/added/deleted.
    let edges = exec
        .execute("SELECT impact, added, deleted FROM changed;")
        .await
        .expect("select changed");
    let text = edges.first().map(|v| v.to_sql()).unwrap_or_default();
    assert!(
        text.contains("impact: 0.") && text.contains("added:") && text.contains("deleted:"),
        "changed: {text}"
    );

    // E-A31 regression guard: re-ingest dedupes (no new commit rows).
    ingest_repo(
        Arc::new(exec.clone()) as Arc<dyn Executor>,
        None,
        &repo.path,
        "live-repo",
        4,
        &[],
    )
    .await
    .expect("re-ingest failed");
    let commits = exec
        .execute("SELECT hash FROM commit;")
        .await
        .expect("select commits 2");
    let text = commits.first().map(|v| v.to_sql()).unwrap_or_default();
    assert_eq!(
        text.matches("hash:").count(),
        2,
        "commits after re-ingest: {text}"
    );

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn live_transaction_with_bound_vars_roundtrip() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };

    // Bound content (quotes, \r\n, unicode) survives a batch transaction.
    let sql = "BEGIN TRANSACTION;\n\
        UPSERT repo:⟨txn_repo⟩ SET name = $name;\n\
        UPSERT commit:⟨txn_hash⟩ SET hash = $h, date = $d, message = $m, repo = repo:⟨txn_repo⟩, batch_status = 'pending';\n\
        UPSERT author:⟨txn_author⟩ SET name = $n;\n\
        RELATE author:⟨txn_author⟩->authored->commit:⟨txn_hash⟩;\n\
        COMMIT TRANSACTION;";
    let mut vars: Vars = Vars::new();
    vars.insert("name".into(), Value::from_t("Repo 'quoted'"));
    vars.insert("h".into(), Value::from_t("txn_hash"));
    vars.insert("d".into(), Value::from_t("2024-01-01T00:00:00Z"));
    vars.insert("m".into(), Value::from_t("subject with\r\nnewline 'quote'"));
    vars.insert("n".into(), Value::from_t("Jöhn Dœ"));
    exec.smart_query(sql, vars).await.expect("txn failed");

    let rows = exec
        .execute("SELECT * FROM repo:⟨txn_repo⟩; SELECT * FROM commit:⟨txn_hash⟩; SELECT * FROM author:⟨txn_author⟩; SELECT VALUE id FROM commit WHERE id IN [commit:⟨txn_hash⟩];")
        .await
        .expect("readback");
    let texts: Vec<String> = rows.statements().iter().map(|v| v.to_sql()).collect();
    assert!(texts[0].contains("Repo 'quoted'"), "repo: {}", texts[0]);
    assert!(
        texts[1].contains("repo: repo:txn_repo"),
        "repo link: {}",
        texts[1]
    );
    assert!(texts[1].contains("subject with"), "message: {}", texts[1]);
    assert!(
        texts[1].contains("2024-01-01T00:00:00Z"),
        "date: {}",
        texts[1]
    );
    assert!(texts[2].contains("Jöhn Dœ"), "author: {}", texts[2]);
    // Q7 canonicalization: raw ids decode to the angle-wrapped project form.
    let ids = ibis_db::value::decode_select_value_ids(
        &rows.statements()[3],
        "SELECT VALUE id FROM commit WHERE id IN [commit:⟨txn_hash⟩];",
    )
    .expect("decode ids");
    assert_eq!(ids, vec!["commit:⟨txn_hash⟩".to_string()]);

    cleanup(&exec, &db_name).await;
}
