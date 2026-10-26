//! Numstat parser contract tests —
//! `process_git_log_lines` over constructed git-log output.

mod common;

use std::sync::Arc;

use common::{mock_ingester, MockDb};

use ibis_db::ids::{format_record_id, sanitize_id};
use ibis_db::schema::names::SOURCE_FILE;
use ibis_ingest_git::process_git_log_lines;

fn file_id(repo: &str, path: &str) -> String {
    format_record_id(
        SOURCE_FILE,
        &format!("{}_{}", sanitize_id(repo), sanitize_id(path)),
    )
}

#[tokio::test]
async fn process_git_log_stream_parsing() {
    let input = "\
COMMIT|hash1|parent1|Author Name|2024-01-01T00:00:00Z|Initial commit
10\t5\tfile1.txt
-\t-\tbinary.png

COMMIT|hash2|hash1|Author Two|2024-01-02T00:00:00Z|Fix bug #123 and #456
2\t1\tsrc/code.go

COMMIT|hash3|hash2|Author Three|2024-01-03T00:00:00Z|Update \"quoted file.txt\"
5\t0\t\"path/to/quoted file.txt\"

COMMIT|hash4|hash3|Author Four|2024-01-04T00:00:00Z|Only verify space in path without quotes (unlikely in git but testing robustness)
1\t1\tpath/with spaces.txt

COMMIT|hash5|hash4|Author Five|2024-01-05T00:00:00Z|Test tab split failure fallback
1 1 fallback_test.txt
";

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = mock_ingester();

    let lines: Vec<String> = input.lines().map(str::to_string).collect();
    process_git_log_lines(
        lines,
        mock_db.clone(),
        Some(mock_redmine.clone()),
        "repo:test",
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("processGitLogStream failed");

    // 1. Issue parsing (hash2 has #123 and #456)
    let ids = mock_redmine.ingested_ids.lock().unwrap().clone();
    assert!(
        ids.contains(&"123".to_string()),
        "failed to parse issue #123"
    );
    assert!(
        ids.contains(&"456".to_string()),
        "failed to parse issue #456"
    );

    // 2. Quoted path parsing (hash3)
    let target_path = "path/to/quoted file.txt";
    let expected_id = file_id("test-repo", target_path);
    assert!(
        mock_db.entries().iter().any(|(sql, vars)| {
            sql.contains(&format!("UPSERT {expected_id}"))
                && vars
                    .values()
                    .any(|v| matches!(v, ibis_db::value::Value::String(s) if s == target_path))
        }),
        "failed to parse quoted path: {target_path}"
    );

    // 3. Binary file parsing (hash1): added=0, deleted=0
    let binary_id = file_id("test-repo", "binary.png");
    assert!(
        mock_db.queries().iter().any(|q| q.contains(&binary_id)
            && q.contains("added = 0")
            && q.contains("deleted = 0")),
        "failed to parse binary file stats"
    );
}

#[tokio::test]
async fn process_git_log_stream_parsing_robustness() {
    let mut sb = String::new();
    sb.push_str("COMMIT|hash1|parent|Me|Date|Msg\n");
    sb.push_str("5\t3\tpath/with spaces.txt\n");
    sb.push_str("COMMIT|hash2|hash1|Me|Date|Msg\n");
    sb.push_str("1 1 space_separated_fallback.txt\n");

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = mock_ingester();

    let lines: Vec<String> = sb.lines().map(str::to_string).collect();
    process_git_log_lines(
        lines,
        mock_db.clone(),
        Some(mock_redmine.clone()),
        "repo:test",
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("processGitLogStream failed");

    // Tab-separated path with spaces parsed.
    assert!(
        mock_db.entries().iter().any(|(sql, vars)| {
            sql.contains("path = $path")
                && vars.values().any(|v| matches!(v, ibis_db::value::Value::String(s) if s == "path/with spaces.txt"))
        }),
        "failed to parse tab-separated path with spaces"
    );

    // Space-separated line is skipped (no tabs) — logs and skips.
    assert!(
        !mock_db.any_contains("space_separated_fallback"),
        "space-separated fallback line must be skipped"
    );
}

#[tokio::test]
async fn process_git_log_stream_tab_in_path() {
    let input = "COMMIT|hashTab|parent|Me|Date|Msg\n1\t1\tpath/with\ttab/file.txt\n";

    let mock_db = Arc::new(MockDb::default());
    let mock_redmine = mock_ingester();

    let lines: Vec<String> = input.lines().map(str::to_string).collect();
    process_git_log_lines(
        lines,
        mock_db.clone(),
        Some(mock_redmine.clone()),
        "repo:test",
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("processGitLogStream failed");

    let expected_path = "path/with\ttab/file.txt";
    assert!(
        mock_db.entries().iter().any(|(_, vars)| vars
            .values()
            .any(|v| matches!(v, ibis_db::value::Value::String(s) if s == expected_path))),
        "failed to parse path with embedded tab"
    );
}

#[tokio::test]
async fn transaction_batches_at_1000_ops() {
    // 500 commits + 500 numstat lines = 1000 ops → exactly one flush.
    let mut input = String::new();
    for i in 0..500 {
        input.push_str(&format!(
            "COMMIT|h{i:04}|p{i:04}|A|2024-01-01T00:00:00Z|Commit {i}\n"
        ));
        input.push_str(&format!("{i}\t0\tfile{i}.txt\n"));
    }
    let mock_db = Arc::new(MockDb::default());
    let lines: Vec<String> = input.lines().map(str::to_string).collect();
    process_git_log_lines(
        lines,
        mock_db.clone(),
        None,
        "repo:test",
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("processGitLogStream failed");

    let executes = mock_db
        .entries()
        .iter()
        .filter(|(sql, _)| sql.contains("BEGIN TRANSACTION"))
        .count();
    assert_eq!(executes, 1, "exactly one 1000-op transaction expected");
    let txn = &mock_db.entries()[0].0;
    assert!(txn.starts_with("BEGIN TRANSACTION;\n"));
    assert!(txn.trim_end().ends_with("COMMIT TRANSACTION;"));
    // No content is interpolated (F-002): subjects/paths/author names only
    // appear as $vars.
    assert!(!txn.contains("Commit 42"), "subject leaked into SQL");
}

#[tokio::test]
async fn skipped_lines_do_not_count_toward_flush_threshold() {
    // E-A17: empty/malformed lines never count toward the 1000-op
    // threshold (`continue skips batchCount++). 999 consumed lines +
    // 500 blank lines must NOT trigger the interim flush; if blanks counted,
    // two transactions would appear.
    let mut input = String::new();
    for i in 0..499 {
        input.push_str(&format!(
            "COMMIT|h{i:03}|p{i:03}|A|2024-01-01T00:00:00Z|Commit {i}\n"
        ));
        input.push_str(&format!("{i}\t0\tfile{i}.txt\n"));
    }
    input.push_str(&"empty line\n".repeat(500));
    // 999th consumed line.
    input.push_str("COMMIT|hz|pz|A|2024-01-01T00:00:00Z|Last commit\n");
    input.push_str("1\t0\tlast.txt\n");

    let mock_db = Arc::new(MockDb::default());
    let lines: Vec<String> = input.lines().map(str::to_string).collect();
    process_git_log_lines(
        lines,
        mock_db.clone(),
        None,
        "repo:test",
        "test-repo",
        1,
        &[],
    )
    .await
    .expect("processGitLogStream failed");

    let executes = mock_db
        .entries()
        .iter()
        .filter(|(sql, _)| sql.contains("BEGIN TRANSACTION"))
        .count();
    assert_eq!(
        executes, 1,
        "blank lines must not advance the flush threshold"
    );
}
