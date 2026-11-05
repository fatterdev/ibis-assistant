//! Live SurrealDB integration tests for the log pipeline.
//!
//! Requires the shared dev sidecar `IBIS_SPIKE_SURREAL_URL`
//! (default `ws://127.0.0.1:38000`, root/root). Tests skip gracefully when
//! the server is unreachable so `cargo test` stays green without it.
//!
//! Pinned behavior:
//! - constructor upserts project/log_file + has_log edge, loads known patterns;
//! - the AI stage upserts `error_type` by content hash, supersedes categories
//!   by hiding old rows, and writes the log_entry edge;
//! - offset roundtrip on the `log_file` record;
//! - LogAlign promotion persists a `log_template` row with origin='ai' and the
//!   promoted pattern classifies subsequent lines statically.

use std::sync::Arc;

use ibis_db::executor::{Executor, SurrealDbExecutor};
use ibis_db::schema;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

/// Fake AI provider: functional, replies a fixed classification payload.
struct LiveFakeAI {
    reply: String,
}

#[async_trait::async_trait]
impl ibis_ai::AIProvider for LiveFakeAI {
    fn name(&self) -> &str {
        "live-fake-ai"
    }
    async fn is_functional(&self) -> bool {
        true
    }
    async fn generate_content(
        &self,
        _contents: &[ibis_ai::types::Content],
        _config: ibis_ai::types::GenerationConfig,
    ) -> std::result::Result<ibis_ai::types::Candidate, ibis_ai::AiError> {
        Ok(ibis_ai::types::Candidate {
            content: ibis_ai::types::Content {
                parts: vec![ibis_ai::types::Part {
                    text: self.reply.clone(),
                }],
                ..Default::default()
            },
            ..Default::default()
        })
    }
}

fn fake_ai(reply: &str) -> Arc<ibis_ai::Client> {
    let provider = Arc::new(LiveFakeAI {
        reply: reply.to_string(),
    }) as Arc<dyn ibis_ai::AIProvider>;
    Arc::new(ibis_ai::Client::new(
        Some(provider.clone()),
        Some(provider),
        None,
    ))
}

/// Connects to the shared dev server and selects a scratch database.
/// Returns `None` when the server is unreachable (test skips).
async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("laneg_{}", uuid::Uuid::new_v4().simple());
    let exec =
        match SurrealDbExecutor::connect(&url, "ibis_db_tests", &db_name, "root", "root").await {
            Ok(exec) => exec,
            Err(err) => {
                eprintln!("live: SurrealDB not reachable ({err}), skipping");
                return None;
            }
        };
    schema::init_schema(&exec).await.expect("init schema");
    Some((Arc::new(exec), db_name))
}

async fn cleanup(exec: &Arc<dyn Executor>, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

#[tokio::test]
async fn analyzer_constructor_upserts_and_loads_known_patterns() {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("laneg_{}", uuid::Uuid::new_v4().simple());
    let exec =
        match SurrealDbExecutor::connect(&url, "ibis_db_tests", &db_name, "root", "root").await {
            Ok(exec) => exec,
            Err(err) => {
                eprintln!("live: SurrealDB not reachable ({err}), skipping");
                return;
            }
        };
    schema::init_schema(&exec).await.expect("init schema");
    let db: Arc<dyn Executor> = Arc::new(exec.clone());

    // Seed one known error category with a <VAR> mask template.
    exec.execute(
        "UPSERT error_type:⟨seed⟩ SET hash = 'seed', category = 'LIVE_CONN_REFUSED', stack_trace = '', severity = 5, template = 'ERROR connection refused host=<VAR>', created_at = time::now(), hidden = false;",
    )
    .await
    .expect("seed error_type");

    let analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        "logs/live-proj/app.log",
        "live-proj",
        None,
        Some(db),
        None,
    )
    .await;

    // Constructor persisted project + log_file records and the has_log edge.
    let outcome = exec
        .execute("SELECT name FROM project:⟨live_proj⟩;")
        .await
        .expect("project row");
    let text = format!("{:?}", outcome.first());
    assert!(
        text.contains("live-proj"),
        "project record must carry the name: {text}"
    );
    let outcome = exec
        .execute("SELECT path FROM log_file:⟨app_log⟩;")
        .await
        .expect("log_file row");
    let text = format!("{:?}", outcome.first());
    assert!(
        text.contains("app.log"),
        "log_file must record the path: {text}"
    );

    // Constructor loaded the seeded known pattern (usable for masking).
    let filtered = analyzer.filter_known_errors("ERROR connection refused host=db7");
    assert!(
        filtered.contains("[KNOWN_ERROR: LIVE_CONN_REFUSED]"),
        "seeded known pattern must mask: {filtered:?}"
    );

    let db: Arc<dyn Executor> = Arc::new(exec.clone());
    cleanup(&db, &db_name).await;
}

#[tokio::test]
async fn ai_stage_upserts_error_type_and_supersedes() {
    let Some((db, db_name)) = connect_scratch().await else {
        return;
    };

    // Seed an old category to be superseded (hidden) by the AI classification.
    db.execute(
        "UPSERT error_type:⟨old⟩ SET hash = 'old', category = 'OLD_CASCADE', stack_trace = '', severity = 3, template = 'OLD', created_at = time::now(), hidden = false;",
    )
    .await
    .expect("seed old");

    let analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        "logs/live-proj/app.log",
        "live-proj",
        None,
        Some(db.clone()),
        Some(fake_ai(
            r#"[{"category":"NEW_CASCADE","stack_trace":"ERROR x id=1","file":"","line":0,"symbol":"","cause":"test","severity":7,"template":"ERROR x id=<VAR>","supersedes":["OLD_CASCADE"]}]"#,
        )),
    )
    .await;

    let (out, err) = analyzer
        .process_batch_sync(&["ERROR x id=42".to_string()])
        .await;
    assert!(err.is_none(), "batch must not error: {err:?}");
    assert_eq!(out.len(), 1, "one classified anomaly: {out:?}");
    assert_eq!(out[0].category, "NEW_CASCADE");

    // error_type row exists with the exact category; OLD_CASCADE is hidden.
    let outcome = db
        .smart_query(
            "SELECT category, hidden FROM error_type WHERE category IN [$a, $b];",
            ibis_db::executor::vars([
                ("a", ibis_db::value::Value::from_t("NEW_CASCADE")),
                ("b", ibis_db::value::Value::from_t("OLD_CASCADE")),
            ]),
        )
        .await
        .expect("select error_type");
    let text = format!("{:?}", outcome.first());
    assert!(text.contains("NEW_CASCADE"), "ingested row present: {text}");
    assert!(
        text.contains("true"),
        "OLD_CASCADE must be hidden by supersedes: {text}"
    );

    // log_entry edge exists (is_type_of).
    let outcome = db
        .execute("SELECT count() AS c FROM log_entry GROUP ALL;")
        .await
        .expect("log_entry count");
    let text = format!("{:?}", outcome.first());
    assert!(text.contains('1'), "log_entry row written: {text}");

    // Known pattern learned from the AI template now masks repeats.
    let filtered = analyzer.filter_known_errors("ERROR x id=99");
    assert!(
        filtered.contains("[KNOWN_ERROR: NEW_CASCADE]"),
        "AI template must be learned as known pattern: {filtered:?}"
    );

    cleanup(&db, &db_name).await;
}

#[tokio::test]
async fn offset_roundtrip_on_log_file_record() {
    let Some((db, db_name)) = connect_scratch().await else {
        return;
    };

    let analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        "logs/live-proj/offset.log",
        "live-proj",
        None,
        Some(db.clone()),
        None,
    )
    .await;

    assert_eq!(
        analyzer.get_offset().await,
        0,
        "fresh log_file has no offset"
    );
    analyzer.update_offset(4242).await;
    assert_eq!(analyzer.get_offset().await, 4242, "offset roundtrip");

    cleanup(&db, &db_name).await;
}

/// C2/F09 live: the anomaly's file link lands on the canonical
/// `source_file:[repo, file_path]` record (the same one code/git ingest
/// writes), with repo_name set so the recent_failures traversal filters it,
/// and colliding paths coexist — no scalar duplicate rows.
#[tokio::test]
async fn anomaly_links_canonical_source_file() {
    let Some((db, db_name)) = connect_scratch().await else {
        return;
    };

    let analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        "logs/live-proj/link.log",
        "live-proj",
        None,
        Some(db.clone()),
        Some(fake_ai(
            r#"[{"category":"CANON_LINK","stack_trace":"boom in lib","file":"src/lib.rs","line":3,"symbol":"","cause":"c","severity":6,"template":"boom in lib <VAR>","supersedes":[]},
                {"category":"CANON_LINK","stack_trace":"boom in spaced","file":"my dir/my file.rs","line":9,"symbol":"","cause":"c","severity":6,"template":"boom in spaced <VAR>","supersedes":[]}]"#,
        )),
    )
    .await;

    let (out, err) = analyzer
        .process_batch_sync(&["boom in lib 7".to_string(), "boom in spaced 11".to_string()])
        .await;
    assert!(err.is_none(), "batch must not error: {err:?}");
    assert_eq!(out.len(), 2, "two classified anomalies: {out:?}");

    // Canonical records exist with rel_path + repo_name; no scalar legacy row.
    for (file, rel) in [
        ("src/lib.rs", "src/lib.rs"),
        ("my dir/my file.rs", "my dir/my file.rs"),
    ] {
        let id = ibis_db::ids::source_file_id("repo:⟨live_proj⟩", file);
        let sql = format!("SELECT rel_path, repo_name FROM {id};");
        let outcome = db
            .smart_query(&sql, ibis_db::executor::Vars::new())
            .await
            .unwrap_or_else(|e| panic!("canonical record {id}: {e}"));
        let text = format!("{:?}", outcome.first());
        assert!(text.contains(rel), "{id} carries rel_path: {text}");
        assert!(text.contains("live-proj"), "{id} carries repo_name: {text}");
    }
    // No scalar legacy rows: exactly the two canonical records exist.
    let scalar = db
        .execute("SELECT count() FROM source_file GROUP ALL;")
        .await
        .expect("count source_file");
    let scalar_text = format!("{:?}", scalar.first());
    assert!(
        scalar_text.contains("2"),
        "exactly two canonical source_file rows (no scalar duplicates): {scalar_text}"
    );

    // The related_to edge from the error_type to the canonical record.
    let edges = db
        .execute("SELECT in, out FROM related_to;")
        .await
        .expect("related_to edges");
    let text = format!("{:?}", edges.first());
    assert!(
        text.contains("src/lib.rs") && text.contains("my dir/my file.rs"),
        "both canonical files are related_to targets: {text}"
    );

    // The failures surface through the recent_failures traversal
    // (project filter reads repo_name).
    let service = ibis_search::search::SearchService::new(Some(db.clone()), None);
    let failures = service
        .recent_failures(&ibis_search::memory::queries::RecentFailuresParams {
            project_name: "live-proj".into(),
            area: String::new(),
            kinds: vec!["gotcha".into()],
            since: "1w".into(),
            limit: 10,
        })
        .await
        .expect("recent failures");
    assert!(
        failures
            .iter()
            .any(|m| m.title.contains("CANON_LINK") || m.content.contains("boom in lib")),
        "log failure present in recent_failures(live-proj): {failures:?}"
    );
    // Personal-style area filter: the spaced path matches through rel_path.
    let failures = service
        .recent_failures(&ibis_search::memory::queries::RecentFailuresParams {
            project_name: "live-proj".into(),
            area: "my dir".into(),
            kinds: vec!["gotcha".into()],
            since: "1w".into(),
            limit: 10,
        })
        .await
        .expect("recent failures area");
    assert!(
        failures
            .iter()
            .any(|m| m.content.contains("boom in spaced")),
        "area filter reaches the spaced-path record: {failures:?}"
    );

    // A second run classifies the same lines again (template learned): the
    // canonical ids stay unique — no duplicate rows.
    let _ = analyzer
        .process_batch_sync(&["boom in lib 8".to_string()])
        .await;
    let count = db
        .execute("SELECT count() FROM source_file GROUP ALL;")
        .await
        .expect("count files");
    let text = format!("{:?}", count.first());
    assert!(text.contains("2"), "exactly two source_file rows: {text}");

    cleanup(&db, &db_name).await;
}

#[tokio::test]
async fn logalign_promotion_persists_log_template_row() {
    let Some((db, db_name)) = connect_scratch().await else {
        return;
    };

    let analyzer = ibis_ingest_logs::LogAnalyzer::new_with_project(
        "logs/live-proj/promote.log",
        "live-proj",
        None,
        Some(db.clone()),
        None,
    )
    .await;

    let promoted = analyzer
        .promote_ai_errors_to_log_align(&[ibis_ingest_logs::analyzer::SemanticError {
            category: "LIVE_DB_TIMEOUT".into(),
            template: "CRITICAL: SurrealDB connection lost for <VAR>".into(),
            stack_trace: "CRITICAL: SurrealDB connection lost for node-3".into(),
            file: "internal/db/client.rs".into(),
            line: 88,
            ..Default::default()
        }])
        .await;
    assert_eq!(promoted, 1, "one template must be promoted");

    let outcome = db
        .smart_query(
            "SELECT format_string, regex, source_file, source_line, origin FROM log_template WHERE category = $cat;",
            ibis_db::executor::vars([(
                "cat",
                ibis_db::value::Value::from_t("LIVE_DB_TIMEOUT"),
            )]),
        )
        .await
        .expect("select log_template");
    let text = format!("{:?}", outcome.first());
    assert!(
        text.contains("origin") && text.to_lowercase().contains("ai"),
        "promoted template persisted with origin=ai: {text}"
    );

    // The in-memory static pattern now classifies matching lines deterministically.
    let (out, err) = analyzer
        .process_batch_sync(&["CRITICAL: SurrealDB connection lost for node-9".to_string()])
        .await;
    assert!(err.is_none(), "batch must not error: {err:?}");
    assert!(
        out.iter().any(|e| e.category.starts_with("Static Log:")),
        "promoted template must classify statically: {out:?}"
    );

    cleanup(&db, &db_name).await;
}
