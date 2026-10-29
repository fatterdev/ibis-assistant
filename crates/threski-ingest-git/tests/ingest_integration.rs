//! End-to-end ingest over REAL temp git repositories (
//! ).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{init_repo, mock_ingester, MockDb, MockRedmine};

use threski_db::ids::{format_record_id, sanitize_id};
use threski_db::schema::names::{COMMIT, ISSUE, SOURCE_FILE};
use threski_ingest_git::ingest_repo;

#[tokio::test]
async fn ingest_repo_integration() {
    let repo = init_repo("integration").await;
    repo.commit_file("main.go", "package main", "Initial commit")
        .await;
    repo.commit_file("main.go", "package main\n// update", "Fix bug #42")
        .await;

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = mock_ingester();

    ingest_repo(
        mock_db.clone(),
        Some(mock_redmine.clone()),
        &repo.path,
        "test-repo",
        10,
        &[],
    )
    .await
    .expect("IngestRepo failed");

    // Redmine trigger
    let ids = mock_redmine.ingested_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 1, "expected 1 redmine ingestion trigger");
    assert_eq!(ids[0], "42");

    // DB writes: implements -> issue:42
    assert!(
        mock_db
            .queries()
            .iter()
            .any(|q| q.contains("->implements->") && q.contains(&format_record_id(ISSUE, "42"))),
        "did not find implements->issue:42 relation in queries: {:?}",
        mock_db.queries()
    );
    assert!(!mock_db.queries().is_empty());
}

#[tokio::test]
async fn ingest_repo_quoted_paths() {
    let repo = init_repo("quoted").await;
    let file_name = "file with spaces.txt";
    repo.commit_file(file_name, "content", "Add quoted file")
        .await;

    let mock_db = Arc::new(MockDb::default());
    ingest_repo(mock_db.clone(), None, &repo.path, "test-repo", 10, &[])
        .await
        .expect("IngestRepo failed");

    let expected_id = format_record_id(
        SOURCE_FILE,
        &format!("{}_{}", sanitize_id("test-repo"), sanitize_id(file_name)),
    );
    // The clean (unquoted) path is bound, and the record id has no quotes.
    let found = mock_db.entries().iter().any(|(sql, vars)| {
        sql.contains(&format!("UPSERT {expected_id}"))
            && vars
                .values()
                .any(|v| matches!(v, threski_db::value::Value::String(s) if s == file_name))
    });
    assert!(found, "did not find correct file update for quoted path");
    // No quoted form ever leaks into a record id.
    assert!(!mock_db.any_contains("source_file:\""));
}

#[tokio::test]
async fn ingest_repo_incremental() {
    let repo = init_repo("incremental").await;
    repo.commit_file("file1.go", "package main", "Commit One")
        .await;
    let hash1 = repo.head().await;
    repo.commit_file("file2.go", "package main\n// new", "Commit Two")
        .await;
    let hash2 = repo.head().await;

    let mock_db = Arc::new(MockDb::default());
    // Commit 1 already exists (SELECT VALUE id … format).
    mock_db.set_existing(&[format_record_id(COMMIT, &hash1)]);

    ingest_repo(mock_db.clone(), None, &repo.path, "test-repo", 10, &[])
        .await
        .expect("IngestRepo failed");

    // Commit 1 skipped (its only appearance is the inline parent edge of
    // commit 2 — no bound value), commit 2 ingested.
    let mut commit1_bound = false;
    let mut commit2_bound = false;
    for (_, vars) in mock_db.entries() {
        for v in vars.values() {
            if let threski_db::value::Value::String(s) = v {
                if s == &hash1 {
                    commit1_bound = true;
                }
                if s == &hash2 {
                    commit2_bound = true;
                }
            }
        }
    }
    assert!(!commit1_bound, "expected Commit 1 to be skipped");
    assert!(commit2_bound, "expected Commit 2 to be ingested");
}

#[tokio::test]
async fn ingest_repo_slow_redmine_blocks_return() {
    let repo = init_repo("slow").await;
    repo.commit_file("main.go", "package main", "Fix #100")
        .await;

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = Arc::new(MockRedmine {
        delay: Some(Duration::from_millis(200)),
        ..MockRedmine::default()
    });

    let start = Instant::now();
    ingest_repo(
        mock_db.clone(),
        Some(mock_redmine.clone()),
        &repo.path,
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("IngestRepo failed");
    let duration = start.elapsed();

    let ids = mock_redmine.ingested_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], "100");
    assert!(
        duration >= Duration::from_millis(200),
        "IngestRepo returned too quickly ({duration:?}), implying it didn't wait for ticket ingestion"
    );
}

#[tokio::test]
async fn ingest_repo_concurrency_bounded() {
    let repo = init_repo("concurrency").await;
    let issue_count = 20;
    for i in 0..issue_count {
        repo.commit_file(&format!("file{i}.txt"), "content", &format!("Fix bug #{i}"))
            .await;
    }

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = Arc::new(MockRedmine {
        delay: Some(Duration::from_millis(10)),
        ..MockRedmine::default()
    });

    let concurrency = 5;
    ingest_repo(
        mock_db.clone(),
        Some(mock_redmine.clone()),
        &repo.path,
        "test-repo",
        concurrency,
        &[],
    )
    .await
    .expect("IngestRepo failed");

    assert_eq!(
        mock_redmine
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        issue_count
    );
    let max_active = mock_redmine
        .max_active
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        max_active <= concurrency,
        "Concurrency limit exceeded! MaxActive: {max_active}, Limit: {concurrency}"
    );
}

#[tokio::test]
async fn ingest_repo_jira_reference_uses_provider_scoped_issue_id() {
    let repo = init_repo("jira").await;
    repo.commit_file("main.go", "package main", "Fix ABC-321 timeout")
        .await;

    let mock_db = Arc::new(MockDb::default());
    let mock_ingester = mock_ingester();
    ingest_repo(
        mock_db.clone(),
        Some(mock_ingester.clone()),
        &repo.path,
        "test-repo",
        5,
        &[],
    )
    .await
    .expect("IngestRepo failed");

    let target_issue_id = threski_ticketing::issue_record_id(threski_ticketing::PROVIDER_JIRA, "ABC-321");
    assert!(
        mock_db
            .queries()
            .iter()
            .any(|q| q.contains(&target_issue_id) && q.contains("->implements->")),
        "expected implements relation to provider-scoped issue id {target_issue_id}"
    );

    let refs = mock_ingester.ingested_refs.lock().unwrap().clone();
    assert_eq!(refs.len(), 1, "expected one ingested ref");
    assert_eq!(refs[0].provider.as_str(), threski_ticketing::PROVIDER_JIRA);
}

#[tokio::test]
async fn ingest_repo_fails_loudly_when_git_log_fails() {
    // E-A29: the original implementation ignored `git log` exit errors —
    // a repo with no commits reported silent success. The port fails loudly.
    let repo = init_repo("empty").await;
    let mock_db = Arc::new(MockDb::default());
    let result = ingest_repo(mock_db.clone(), None, &repo.path, "test-repo", 2, &[]).await;
    assert!(
        result.is_err(),
        "empty repo must fail, not silently succeed"
    );
}
