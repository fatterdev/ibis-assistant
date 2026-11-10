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
/// Returns `None` when the server is unreachable (test skips) — unless
/// `IBIS_LIVE_REQUIRED=1` (CI live gate): then a failed connection FAILS the
/// test instead of skipping it, and skipped live tests can never pass CI.
async fn connect_scratch() -> Option<(SurrealDbExecutor, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("laneb_{}", uuid::Uuid::new_v4().simple());
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
    // WQ-7 + F02: source_file ids are arrays `[repo, file_path]` — a scalar
    // id fails the typed id field coercion.
    exec.execute("CREATE repo:⟨r1⟩ SET name = 'r'; CREATE source_file:[repo:⟨r1⟩, 'f1'] SET rel_path = 'f1'; RELATE repo:⟨r1⟩->contains->source_file:[repo:⟨r1⟩, 'f1'];")
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
    // The data path is the SurrealDB *file*: keeping it inside the tempdir
    // also isolates the sidecar ledger (recorded beside the data file).
    let db_file = data_dir.path().join("db");
    // Grab a free port, then release it for the server.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("port").port();
    drop(probe);

    let mut manager = ibis_db::process::ProcessManager::start_with_bin(
        std::path::Path::new(&bin),
        "root",
        "root",
        db_file.to_str().expect("utf8 path"),
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

/// Layer 3 reconciliation e2e: the ledger records the embedded instance;
/// an owner crash is simulated by rewriting the recorded owner identity to
/// a dead pid; the restart's reconciliation must terminate the stale
/// instance and come up clean on the same data file and port.
#[tokio::test]
async fn reconciliation_terminates_stale_embedded_instance() {
    let Some(bin) = std::env::var("IBIS_SURREAL_BIN").ok() else {
        eprintln!("live: IBIS_SURREAL_BIN not set, skipping reconciliation test");
        return;
    };
    if !std::path::Path::new(&bin).is_file() {
        eprintln!("live: IBIS_SURREAL_BIN={bin} not a file, skipping");
        return;
    }

    let data_dir = tempfile::tempdir().expect("tempdir");
    let db_file = data_dir.path().join("db");
    // Grab a free port, then release it for the server.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("port").port();
    drop(probe);

    let ledger_dir = data_dir.path().to_path_buf();
    assert!(
        !ibis_core::sidecar_ledger::ledger_path(&ledger_dir).exists(),
        "no ledger before the first start"
    );

    // 1. First instance: registers itself in the ledger.
    let manager1 = ibis_db::process::ProcessManager::start_with_bin(
        std::path::Path::new(&bin),
        "root",
        "root",
        db_file.to_str().expect("utf8 db path"),
        port,
    )
    .await
    .expect("first embedded start");

    let entries = ibis_core::sidecar_ledger::load(&ledger_dir);
    assert_eq!(entries.len(), 1, "the first instance registered itself");
    let entry1 = entries[0].clone();
    assert_eq!(entry1.name, "surreal");
    assert_eq!(entry1.port, port);
    assert_eq!(entry1.data_file, db_file.display().to_string());
    assert!(
        ibis_core::sidecar_ledger::entry_alive(&entry1),
        "the recorded instance is alive"
    );
    let pid1 = entry1.pid;

    // Crash simulation without stopping: dropping the manager does not kill
    // the child (tokio kill_on_drop is off) — the SurrealDB instance stays
    // bound to the data file and port, owned by nobody.
    drop(manager1);

    // The recorded owner becomes a process that no longer exists — exactly
    // what reconciliation observes after a hard kill of the owner ibis.
    let dead_owner = {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit", "0"]);
            c
        } else {
            std::process::Command::new("true")
        };
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn dead owner");
        let pid = child.id();
        let _ = child.wait();
        pid
    };
    let mut orphaned = vec![entry1.clone()];
    orphaned[0].owner_pid = dead_owner;
    orphaned[0].owner_start_time = 1;
    ibis_core::sidecar_ledger::save(&ledger_dir, &orphaned);

    // 2. Restart: reconciliation terminates the stale instance before the
    // fresh one spawns.
    let mut manager2 = ibis_db::process::ProcessManager::start_with_bin(
        std::path::Path::new(&bin),
        "root",
        "root",
        db_file.to_str().expect("utf8 db path"),
        port,
    )
    .await
    .expect("second embedded start after reconciliation");

    assert!(
        !ibis_core::sidecar_ledger::process_alive(pid1, entry1.start_time),
        "the stale instance must have been terminated"
    );

    let entries = ibis_core::sidecar_ledger::load(&ledger_dir);
    assert_eq!(entries.len(), 1, "only the fresh instance is registered");
    assert_ne!(entries[0].pid, pid1, "the ledger tracks the fresh pid");

    // The fresh instance serves real traffic.
    let exec = SurrealDbExecutor::connect(
        &format!("ws://127.0.0.1:{port}"),
        "reconciliation",
        "probe",
        "root",
        "root",
    )
    .await
    .expect("connect after reconciliation");
    exec.execute("DEFINE TABLE _recon SCHEMALESS; CREATE _recon:1 SET ok = true;")
        .await
        .expect("ddl on the fresh instance");

    manager2.stop().await.expect("stop the fresh instance");
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

/// F02 live: lossless `source_file` array ids — the paths that collided
/// under `sanitize_id` coexist as distinct records, special characters
/// round-trip, and the canonical rendering survives decode → re-render.
#[tokio::test]
async fn source_file_ids_are_lossless_and_collision_free() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    let repo_ref = format!("repo:⟨{}⟩", uuid::Uuid::new_v4().simple());
    let paths = [
        "src/a-b.rs",
        "src/a/b.rs",
        "Foo.rs",
        "foo.rs",
        "with space's file.rs",
        "uni-code_éà漢字.rs",
        "back\\slash.rs",
        "a\nb.rs",
        "he said \"hi\".rs",
        "close'; DELETE repo; --",
        "comma,file.rs",
        "brk[et].rs",
    ];
    let mut ids = Vec::new();
    for path in paths {
        let id = ibis_db::ids::source_file_id(&repo_ref, path);
        assert!(
            ibis_db::ids::is_safe_record_id(&id),
            "canonical id must be interpolatable: {id}"
        );
        // Create via bound record (the ingest write path)…
        exec.smart_query(
            "CREATE $id SET rel_path = $rel;",
            vars([
                (
                    "id",
                    Value::RecordId(ibis_db::ids::source_file_record(&repo_ref, path)),
                ),
                ("rel", Value::from_t(path)),
            ]),
        )
        .await
        .unwrap_or_else(|e| panic!("create {path}: {e}"));
        ids.push((path, id));
    }

    // …and via the interpolated rendering (the read/prune path): every
    // canonical id resolves to exactly one record.
    for (path, id) in &ids {
        let sql = format!("SELECT rel_path FROM {id};");
        let rows: Vec<RelPathRow> = exec
            .smart_query(&sql, Vars::new())
            .await
            .unwrap_or_else(|e| panic!("select {id}: {e}"))
            .rows(&sql)
            .expect("decode");
        assert_eq!(rows.len(), 1, "{id} must exist exactly once");
        assert_eq!(rows[0].rel_path.as_deref(), Some(*path), "{id}");
    }

    // Payload that simulates literal closure plus a trailing statement: the
    // canonical id carries it as inert string content only.
    let hostile = "x']; CREATE memory:⟨pwned⟩ SET content = 'owned'; --";
    let hostile_id = ibis_db::ids::source_file_id(&repo_ref, hostile);
    assert!(ibis_db::ids::is_safe_record_id(&hostile_id));
    exec.smart_query(
        "CREATE $id SET rel_path = $rel;",
        vars([
            (
                "id",
                Value::RecordId(ibis_db::ids::source_file_record(&repo_ref, hostile)),
            ),
            ("rel", Value::from_t(hostile)),
        ]),
    )
    .await
    .expect("create hostile path");

    // No side effect escaped the literal.
    let memories = exec
        .execute("SELECT VALUE id FROM memory;")
        .await
        .expect("select memories");
    let text = format!("{:?}", memories.first());
    assert!(
        !text.contains("pwned"),
        "injection attempt must not create records: {text}"
    );

    // Decode → re-render round-trip: `format_record` (the prune path) yields
    // the same canonical id the writer built.
    let rows_sql = "SELECT id, rel_path FROM source_file;";
    let rows: Vec<SourceFileIdRow> = exec
        .smart_query(rows_sql, Vars::new())
        .await
        .expect("select files")
        .rows(rows_sql)
        .expect("decode files");
    assert_eq!(rows.len(), paths.len() + 1, "one record per distinct path");
    for row in &rows {
        let rendered = ibis_db::ids::format_record(&row.id);
        assert!(
            ids.iter().any(|(_, id)| *id == rendered) || rendered == hostile_id,
            "re-rendered id must match the canonical form: {rendered}"
        );
        assert!(
            ibis_db::ids::is_safe_record_id(&rendered),
            "decoded rendering must stay interpolatable: {rendered}"
        );
    }

    cleanup(&exec, &db_name).await;
}

#[derive(Debug, surrealdb::types::SurrealValue)]
struct RelPathRow {
    rel_path: Option<String>,
}

#[derive(Debug, surrealdb::types::SurrealValue)]
struct SourceFileIdRow {
    id: RecordId,
    #[allow(dead_code)]
    rel_path: Option<String>,
}

/// A2 live: `commit_chunk.updated_at` is declared (F07) — the writer's full
/// UPSERT (content/hash/date/updated_at) persists every bound value, and a
/// second schema initialization after seeding stays coherent.
#[tokio::test]
async fn commit_chunk_upsert_persists_all_values_and_schema_reinit_stays_coherent() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    exec.smart_query(
        "UPSERT $id SET content = $content, batch_status = 'pending', commit_hash = $hash, commit_date = $date, updated_at = time::now();",
        vars([
            ("id", Value::RecordId(RecordId::new("commit_chunk", "live_f07"))),
            ("content", Value::from_t("meaning body")),
            ("hash", Value::from_t("abc123")),
            ("date", Value::from_t("2026-11-05T10:00:00Z")),
        ]),
    )
    .await
    .expect("upsert commit_chunk");

    let sql = "SELECT content, commit_hash, commit_date, updated_at FROM commit_chunk:⟨live_f07⟩;";
    let outcome = exec.smart_query(sql, Vars::new()).await.expect("select");
    let text = format!("{:?}", outcome.first());
    assert!(
        text.contains("meaning body"),
        "content must persist: {text}"
    );
    assert!(text.contains("abc123"), "commit_hash must persist: {text}");
    assert!(
        text.contains("2026-11-05"),
        "commit_date must persist: {text}"
    );
    assert!(
        !text.contains("updated_at: None") && !text.contains("\"updated_at\": None"),
        "updated_at must be set (declared field), got: {text}"
    );

    // Re-run the DDL after seeding: endpoints and relations must stay
    // coherent (characterization probe for the REMOVE FIELD relation note —
    // no new behavior asserted, only idempotency).
    schema::init_schema(&exec).await.expect("re-init schema");
    let re = exec.smart_query(sql, Vars::new()).await.expect("re-select");
    assert_eq!(
        format!("{:?}", re.first()),
        text,
        "re-init must not alter rows"
    );

    cleanup(&exec, &db_name).await;
}

/// F13 live: `migrate_with` applies an unsorted `[3, 2]` list in id order
/// and ends at the highest version; a re-run is a no-op.
#[tokio::test]
async fn migrations_apply_in_id_order_live() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };

    let m2 = migrate::Migration {
        id: 2,
        sql: "DEFINE TABLE IF NOT EXISTS _live_m2 SCHEMALESS;",
    };
    let m3 = migrate::Migration {
        id: 3,
        sql: "DEFINE TABLE IF NOT EXISTS _live_m3 SCHEMALESS;",
    };
    let version = migrate::migrate_with(&exec, &[m3, m2])
        .await
        .expect("migrate");
    assert_eq!(version, 3);
    assert_eq!(migrate::current_version(&exec).await.expect("current"), 3);
    for table in ["_live_m2", "_live_m3"] {
        exec.execute(&format!("SELECT * FROM {table};"))
            .await
            .unwrap_or_else(|e| panic!("table {table} must exist: {e}"));
    }
    // Re-run: already applied → no-op.
    let version = migrate::migrate_with(&exec, &[m3, m2])
        .await
        .expect("re-migrate");
    assert_eq!(version, 3);

    // Duplicate ids are rejected outright.
    let err = migrate::migrate_with(&exec, &[m3, m3])
        .await
        .expect_err("duplicate");
    assert!(err.to_string().contains("duplicate migration id"), "{err}");

    cleanup(&exec, &db_name).await;
}

/// E16 live probe: `task_active_key` UNIQUE blocks a second active task while
/// multiple terminal rows with `active_key = NONE` coexist on 4.0.
#[tokio::test]
async fn unique_active_key_blocks_second_active_task() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(&exec).await.expect("init schema");

    exec.execute(
        "CREATE task:⟨a1⟩ SET kind = 'memory_consolidate', status = 'queued', \
         project = 'p', active_key = 'memory_consolidate:p';",
    )
    .await
    .expect("create first active task");

    let err = exec
        .execute(
            "CREATE task:⟨a2⟩ SET kind = 'memory_consolidate', status = 'queued', \
             project = 'p', active_key = 'memory_consolidate:p';",
        )
        .await
        .expect_err("second active task must violate the UNIQUE index");
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("unique") || text.contains("already contains"),
        "unexpected UNIQUE error: {err}"
    );

    // A terminal transition clears the slot; a new active task can take it.
    exec.execute(
        "UPDATE task:⟨a1⟩ SET status = 'completed', active_key = NONE, \
         finished_at = time::now();",
    )
    .await
    .expect("complete first task");
    exec.execute(
        "CREATE task:⟨a2⟩ SET kind = 'memory_consolidate', status = 'queued', \
         project = 'p', active_key = 'memory_consolidate:p';",
    )
    .await
    .expect("the released slot accepts a new active task");

    // Two more terminal rows with NONE: UNIQUE must ignore NONE on 4.0.
    exec.execute(
        "UPDATE task:⟨a2⟩ SET status = 'failed', active_key = NONE, \
         finished_at = time::now();",
    )
    .await
    .expect("fail second task");
    exec.execute(
        "CREATE task:⟨a3⟩ SET kind = 'memory_consolidate', status = 'completed', \
         project = 'p', active_key = NONE, finished_at = time::now();",
    )
    .await
    .expect("terminal NONE row 1");
    exec.execute(
        "CREATE task:⟨a4⟩ SET kind = 'memory_consolidate', status = 'failed', \
         project = 'p', active_key = NONE, finished_at = time::now();",
    )
    .await
    .expect("terminal NONE row 2");

    let outcome = exec
        .execute("SELECT VALUE id FROM task WHERE active_key = NONE;")
        .await
        .expect("select terminal rows");
    match outcome.first() {
        Some(Value::Array(rows)) => {
            assert_eq!(rows.len(), 4, "four terminal rows with NONE must coexist")
        }
        other => panic!("unexpected SELECT outcome: {other:?}"),
    }

    cleanup(&exec, &db_name).await;
}
