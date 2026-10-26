//! Live SurrealDB integration tests.
//!
//! Requires a running SurrealDB server: `IBIS_SPIKE_SURREAL_URL`
//! (default `ws://127.0.0.1:38000`, root/root). Every test skips gracefully
//! (prints and returns) when the server is unreachable, so `cargo test` stays
//! green without the sidecar.
//!
//! The process-lifecycle test additionally honors `IBIS_SURREAL_BIN` (path to a
//! `surreal` binary) and is skipped when unset.

use chrono::Utc;
use surrealdb::types::{Object, RecordId, RecordIdKey, SurrealValue, ToSql, Value};

use ibis_db::executor::{vars, Executor, SurrealDbExecutor, Vars};
use ibis_db::migrate;
use ibis_db::schema;
use ibis_db::tables::{MemoryKind, MemoryRow, MemoryStatus, Visibility};
use ibis_db::value::{F64, I64};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

/// Connects to the shared dev server and selects a scratch database.
/// Returns `None` when the server is unreachable (test skips).
async fn connect_scratch() -> Option<(SurrealDbExecutor, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("laneb_{}", uuid::Uuid::new_v4().simple());
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
async fn fresh_schema_init_is_idempotent_and_spots_rows() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };

    // Full final-state DDL (migrations 1–5 folded in), executed twice to prove
    // the statements re-run safely.
    schema::init_schema(&exec).await.expect("init schema");
    schema::init_schema(&exec).await.expect("re-init schema");

    // Spot row: server-side defaults from the typed memory fields.
    exec.execute("CREATE memory:⟨t1⟩ SET content = 'spot';")
        .await
        .expect("create memory");
    let rows: Vec<MemoryRow> = exec
        .smart_query("SELECT * FROM memory:⟨t1⟩;", Vars::new())
        .await
        .expect("select memory")
        .rows("SELECT * FROM memory:⟨t1⟩;")
        .expect("decode memory");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id.key, RecordIdKey::String("t1".to_string()));
    assert_eq!(row.kind, MemoryKind::Fact);
    assert_eq!(row.status, MemoryStatus::Current);
    assert_eq!(row.visibility, Visibility::Team);
    assert_eq!(row.content, "spot");
    // Q1 live: the DDL default 0.7 arrives as a Decimal and coerces through F64.
    assert_eq!(row.confidence, Some(F64(0.7)));
    assert_eq!(row.tags.as_ref().map(|t| t.len()), Some(0));
    assert!(row.created_at.into_inner() > Utc::now() - chrono::Duration::minutes(1));

    // Spot indexes: vector indexes exist on all five embedding tables.
    for table in [
        "file_chunk",
        "error_type",
        "memory",
        "reasoning",
        "commit_chunk",
    ] {
        let info = exec
            .execute(&format!("INFO FOR TABLE {table};"))
            .await
            .expect("info for table");
        let text = format!("{:?}", info.first());
        assert!(
            text.contains("embedding") || text.contains("HNSW"),
            "table {table} must expose its embedding index: {text}"
        );
    }

    // Edges exist: RELATE through the union-typed contains edge (repo→source_file).
    exec.execute("CREATE repo:⟨r1⟩ SET name = 'r'; CREATE source_file:⟨r1_f1⟩ SET rel_path = 'f1'; RELATE repo:⟨r1⟩->contains->source_file:⟨r1_f1⟩;")
        .await
        .expect("relate contains");
    let edge = exec
        .smart_query("SELECT in, out FROM contains;", Vars::new())
        .await
        .expect("select contains");
    let rows = edge
        .rows::<ContainsEdge>("SELECT in, out FROM contains;")
        .expect("decode edge");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].in_.table.as_str(), "repo");
    assert_eq!(rows[0].out.table.as_str(), "source_file");

    cleanup(&exec, &db_name).await;
}

#[derive(Debug, surrealdb::types::SurrealValue)]
struct ContainsEdge {
    #[surreal(rename = "in")]
    in_: RecordId,
    #[surreal(rename = "out")]
    out: RecordId,
}

#[tokio::test]
async fn memory_roundtrip_with_bound_vars() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    // F-002: the record id is inlined (identifier only, built via format_record_id
    // semantics over a safe uuid key); every content-bearing value is bound (the
    // Scenario: EscapeSQL path would have been fragile here — content carries quotes,
    // backslashes and newlines).
    let content = "It's a \"quoted\" C:\\path\nwith newlines\tand tabs";
    let key = uuid::Uuid::new_v4().simple().to_string();
    let id = format!("memory:⟨{key}⟩");
    let repo_key = uuid::Uuid::new_v4().simple().to_string();
    let statement = format!(
        "CREATE {id} SET kind = 'decision', title = $title, content = $content, \
        status = 'current', visibility = 'personal', owner = $owner, tags = $tags, \
        project = $project, store = 'git', introduced_in_oid = $oid, \
        provenance = $prov, embedding = $embedding;"
    );
    let mut prov = Object::new();
    prov.insert("agent", "cursor");
    prov.insert("session", "session-42");
    prov.insert("tool", "memory_add");
    // Record links are bound as Value::RecordId — the server rejects string
    // coercion on typed record fields (live finding, Q3 write-path).
    prov.insert(
        "introduced_in_commit",
        RecordId::new("commit", "0123456789abcdef"),
    );
    prov.insert("dirty", true);
    let binding: Vec<Value> = {
        // HNSW memory_embedding enforces DIMENSION 768 (live finding).
        let mut v = vec![Value::from_t(0.25f64)];
        v.push(Value::from_t(-1.5f64));
        v.extend(std::iter::repeat_n(Value::from_t(0.0f64), 766));
        v
    };
    let v: Vars = vars([
        ("title", Value::from_t("Roundtrip memory")),
        ("content", Value::from_t(content)),
        ("owner", Value::from_t("alice")),
        ("tags", Value::from_t(vec!["rust", "port"])),
        ("project", Value::RecordId(RecordId::new("repo", repo_key))),
        ("oid", Value::from_t("0123456789abcdef")),
        ("prov", Value::Object(prov)),
        ("embedding", Value::from_t(binding)),
    ]);
    exec.smart_query(&statement, v)
        .await
        .expect("create memory");

    // Raw SELECT → decode → compare.
    let sql = format!("SELECT * FROM {id};");
    let rows: Vec<MemoryRow> = exec
        .smart_query(&sql, Vars::new())
        .await
        .expect("select")
        .rows(&sql)
        .expect("decode");
    assert_eq!(rows.len(), 1, "expected exactly one memory row");
    let row = &rows[0];
    assert_eq!(row.id.key, RecordIdKey::String(key.clone()));
    assert_eq!(row.kind, MemoryKind::Decision);
    assert_eq!(row.status, MemoryStatus::Current);
    assert_eq!(row.visibility, Visibility::Personal);
    assert_eq!(row.owner.as_deref(), Some("alice"));
    assert_eq!(
        row.tags.as_deref(),
        Some(&["rust".to_string(), "port".to_string()][..])
    );
    assert_eq!(
        row.content, content,
        "bound-var content must round-trip verbatim"
    );
    assert_eq!(row.store, Some(ibis_db::tables::Store::Git));
    assert_eq!(row.introduced_in_oid.as_deref(), Some("0123456789abcdef"));
    let project = row.project.as_ref().expect("project link");
    assert_eq!(project.table.as_str(), "repo");
    // Q6: created_at/updated_at decode as datetimes.
    assert!(row.created_at.into_inner() > Utc::now() - chrono::Duration::minutes(1));
    assert!(row.updated_at.into_inner() > Utc::now() - chrono::Duration::minutes(1));
    // Q12: embedding round-trips at f32 precision (768 dims per HNSW contract).
    let emb = row.embedding.as_ref().expect("embedding");
    assert_eq!(emb.0.len(), 768);
    assert_eq!(emb.0[0], 0.25f32);
    assert_eq!(emb.0[1], -1.5f32);
    // Provenance decodes with partial members.
    let prov = row.provenance.as_ref().expect("provenance");
    assert_eq!(prov.agent.as_deref(), Some("cursor"));
    assert_eq!(prov.dirty, Some(true));
    assert_eq!(
        prov.introduced_in_commit.as_ref().map(|r| r.to_sql()),
        Some("commit:0123456789abcdef".to_string())
    );

    // Q7: SELECT VALUE id → canonical id strings.
    let ids = exec
        .select_value_ids("SELECT VALUE id FROM memory;")
        .await
        .expect("value ids");
    assert!(
        ids.contains(&id),
        "value ids must contain {id}, got {ids:?}"
    );

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn q5_numeric_issue_ids_stay_unwrapped() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    exec.smart_query(
        "CREATE issue:42 SET provider = $p, external_key = $k;",
        vars([("p", "redmine"), ("k", "42")]),
    )
    .await
    .expect("create issue");

    let sql = "SELECT * FROM issue:42;";
    let rows: Vec<ibis_db::tables::IssueRow> = exec
        .smart_query(sql, Vars::new())
        .await
        .expect("select issue")
        .rows(sql)
        .expect("decode issue");
    assert_eq!(rows.len(), 1);
    // Q5: numeric record ids stay unwrapped in the canonical form.
    assert_eq!(ibis_db::ids::format_record(&rows[0].id), "issue:42");
    assert_eq!(rows[0].provider.as_deref(), Some("redmine"));

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn migration_framework_live_upsert_semantics() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };

    // Empty history: migrate touches only the version record definition.
    let version = migrate::migrate(&exec).await.expect("migrate");
    assert_eq!(version, 0);

    // Fresh DB: no migration:current record exists yet.
    assert_eq!(migrate::current_version(&exec).await.expect("current"), 0);

    // UPSERT-only persistence: record twice — the record is created then
    // overwritten, never duplicated, never an UPDATE/CREATE error.
    migrate::record_version(&exec, 1).await.expect("record 1");
    migrate::record_version(&exec, 1)
        .await
        .expect("record 1 again");
    assert_eq!(migrate::current_version(&exec).await.expect("current"), 1);
    migrate::record_version(&exec, 2).await.expect("record 2");
    assert_eq!(migrate::current_version(&exec).await.expect("current"), 2);

    // The framework applies only versions above the current one.
    let applied = [migrate::Migration {
        id: 3,
        sql: "DEFINE TABLE _probe_migration3 SCHEMALESS;",
    }];
    let version = migrate::migrate_with(&exec, &applied)
        .await
        .expect("migrate with");
    assert_eq!(version, 3);
    assert_eq!(migrate::current_version(&exec).await.expect("current"), 3);
    // Already applied → no-op on re-run.
    let version = migrate::migrate_with(&exec, &applied)
        .await
        .expect("re-migrate");
    assert_eq!(version, 3);

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn retry_and_single_result_semantics() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    // Single-statement responses unwrap to the statement result (Q8); count
    // over an empty table returns an empty row set (SurrealDB 3.3.4).
    let outcome = exec
        .execute("SELECT count() FROM memory;")
        .await
        .expect("count");
    match outcome.first() {
        Some(Value::Array(rows)) => assert!(
            rows.is_empty() || rows.len() == 1,
            "count() returns zero or one rows, got {rows:?}"
        ),
        other => panic!("unexpected count() shape: {other:?}"),
    }

    // Multi-statement responses keep per-statement results (Q8/Q10).
    let outcome = exec
        .execute("SELECT * FROM memory; SELECT count() FROM memory; SELECT VALUE id FROM memory;")
        .await
        .expect("multi");
    assert_eq!(
        outcome.statements().len(),
        3,
        "per-statement results preserved"
    );

    // DEFINE re-runs: the DDL uses IF NOT EXISTS (schema module), but a bare
    // re-DEFINE surfaces the "already exists" error to the caller — the client
    // demotes it to a debug log while still returning it
    exec.execute("DEFINE TABLE _retry_probe SCHEMALESS;")
        .await
        .expect("define");
    let again = exec.execute("DEFINE TABLE _retry_probe SCHEMALESS;").await;
    let err = again.expect_err("re-DEFINE must surface the already-exists error");
    assert!(
        err.to_string().to_lowercase().contains("already exists"),
        "unexpected error: {err}"
    );

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn embedded_process_lifecycle() {
    let Some(bin) = std::env::var("IBIS_SURREAL_BIN").ok() else {
        eprintln!("live: IBIS_SURREAL_BIN not set, skipping embedded process test");
        return;
    };
    if !std::path::Path::new(&bin).is_file() {
        eprintln!("live: IBIS_SURREAL_BIN={bin} not a file, skipping");
        return;
    }

    let data_dir = tempfile::tempdir().expect("tempdir");
    // Grab a free port, then release it for the server.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("port").port();
    drop(probe);

    let mut manager = ibis_db::process::ProcessManager::start_with_bin(
        std::path::Path::new(&bin),
        "root",
        "root",
        data_dir.path().to_str().expect("utf8 path"),
        port,
    )
    .await
    .expect("embedded start");
    assert_eq!(manager.port(), port);

    // The instance accepts a full client session.
    let exec = SurrealDbExecutor::connect(
        &format!("ws://127.0.0.1:{port}"),
        "lifecycle",
        "probe",
        "root",
        "root",
    )
    .await
    .expect("connect embedded");
    exec.execute("DEFINE TABLE _lifecycle SCHEMALESS; CREATE _lifecycle:1 SET ok = true;")
        .await
        .expect("ddl on embedded instance");

    manager.stop().await.expect("stop embedded");
}

#[tokio::test]
async fn smart_query_decodes_key_usage_ints() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    // Scenario: key_usage writer shape : bound ints, time::now datetimes;
    // the record id is an identifier built inline.
    let id = "key_usage:01234567_20261025";
    let mut create_vars: Vars = Vars::new();
    create_vars.insert("req".to_string(), Value::from_t(12i64));
    create_vars.insert("owner".to_string(), Value::from_t("gemini-primary"));
    exec.smart_query(
        "CREATE key_usage:01234567_20261025 SET requests = $req, owner = $owner, date = time::now();",
        create_vars,
    )
    .await
    .expect("create key usage");
    let mut update_vars: Vars = Vars::new();
    update_vars.insert("req".to_string(), Value::from_t(15i64));
    exec.smart_query(
        "UPDATE key_usage:01234567_20261025 SET requests = $req, last_updated = time::now();",
        update_vars,
    )
    .await
    .expect("update key usage");

    let sql = "SELECT * FROM key_usage:01234567_20261025;";
    let rows: Vec<ibis_db::tables::KeyUsageRow> = exec
        .smart_query(sql, vars([("id", id)]))
        .await
        .expect("select usage")
        .rows(sql)
        .expect("decode usage");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, Some(I64(15)));
    assert_eq!(rows[0].owner.as_deref(), Some("gemini-primary"));
    assert!(rows[0].last_updated.is_some());

    cleanup(&exec, &db_name).await;
}
