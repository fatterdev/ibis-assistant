//! H0 spike (a): surrealdb crate ↔ surreal v3 binary pairing.
//!
//! Probes the value quirks catalog from (Q1–Q12) over a live
//! WebSocket connection. Requires a running SurrealDB server; configured via
//! `THRESKI_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`). Skipped (ok) when
//! the server is unreachable so `cargo test` stays green without the sidecar.

use surrealdb::engine::any::{self, Any};
use surrealdb::types::{Datetime, Number, RecordId, SurrealValue, ToSql, Value};
use surrealdb::Surreal;

async fn connect() -> Option<(Surreal<Any>, String)> {
    let url = std::env::var("THRESKI_SPIKE_SURREAL_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:38000".to_string());
    match any::connect(url.as_str()).await {
        Ok(db) => Some((db, url)),
        Err(err) => {
            eprintln!("spike: SurrealDB not reachable ({err}), skipping");
            None
        }
    }
}

async fn raw_json(db: &Surreal<Any>, sql: &str) -> serde_json::Value {
    let mut result = db.query(sql).await.expect("query");
    let value: Vec<Value> = result.take(0).expect("take row set");
    serde_json::to_value(&value).expect("serialize Value")
}

#[tokio::test]
async fn spike_surreal_roundtrip_and_quirks() {
    let Some((db, url)) = connect().await else {
        return;
    };
    eprintln!("spike: connected to {url}");

    db.signin(surrealdb::opt::auth::Root {
        username: "root".into(),
        password: "root".into(),
    })
    .await
    .expect("signin root");
    db.use_ns("spike")
        .use_db("quirks")
        .await
        .expect("use ns/db");

    // Clean slate.
    db.query("REMOVE TABLE IF EXISTS probe_issue; REMOVE TABLE IF EXISTS probe_memory; REMOVE TABLE IF EXISTS probe_commit; REMOVE TABLE IF EXISTS probe_repo; REMOVE TABLE IF EXISTS probe_edge;")
        .await
        .expect("cleanup");

    // --- Q5: numeric issue IDs stay unwrapped.
    db.query(
        "CREATE probe_issue SET id = probe_issue:42, provider = 'redmine', external_key = '99'",
    )
    .await
    .expect("create numeric issue");
    let q5 = raw_json(&db, "SELECT * FROM probe_issue;").await;
    eprintln!("Q5 numeric id: {}", q5);

    // --- Q4: angle-bracket IDs.
    db.query("CREATE probe_commit:⟨abc123⟩ SET hash = 'abc123', batch_status = 'pending'")
        .await
        .expect("create angled record");
    let q4 = raw_json(&db, "SELECT * FROM probe_commit:⟨abc123⟩;").await;
    eprintln!("Q4 angled id: {}", q4);

    // --- Q1: numbers (Int vs Float on the typed path).
    db.query("DEFINE TABLE probe_memory SCHEMALESS; CREATE probe_memory:⟨ws_1⟩ SET confidence = 0.7, hits = 12;")
        .await
        .expect("memory row");
    let q1 = raw_json(&db, "SELECT * FROM probe_memory;").await;
    eprintln!("Q1 numbers: {}", q1);

    // --- Q2: option fields — absent key vs explicit NONE.
    db.query("UPDATE probe_memory:⟨ws_1⟩ SET title = NONE; CREATE probe_memory:⟨ws_2⟩ SET confidence = NONE;")
        .await
        .expect("option probes");
    let q2 = raw_json(&db, "SELECT * FROM probe_memory;").await;
    eprintln!("Q2 options: {}", q2);

    // --- Q3: record links — RELATE row id shape.
    db.query("DEFINE TABLE probe_edge TYPE RELATION IN probe_repo OUT probe_commit; CREATE probe_repo:⟨r1⟩ SET name = 'r'; RELATE probe_repo:⟨r1⟩->probe_edge->probe_commit:⟨abc123⟩")
        .await
        .expect("relate");
    let q3 = raw_json(&db, "SELECT * FROM probe_edge;").await;
    eprintln!("Q3 relate row: {}", q3);

    // --- Q6: datetimes.
    db.query("UPDATE probe_memory:⟨ws_1⟩ SET valid_from = time::now();")
        .await
        .expect("datetime");
    let q6 = raw_json(&db, "SELECT valid_from FROM probe_memory:⟨ws_1⟩;").await;
    eprintln!("Q6 datetime: {}", q6);

    // --- Q7: SELECT VALUE id returns bare array.
    let q7 = raw_json(&db, "SELECT VALUE id FROM probe_memory;").await;
    eprintln!("Q7 select value ids: {}", q7);

    // --- Q8/Q10: multi-statement execute + BEGIN/COMMIT transaction arrays.
    let multi = raw_json(
        &db,
        "BEGIN; CREATE probe_memory:⟨ws_3⟩ SET confidence = 1; COMMIT;",
    )
    .await;
    eprintln!("Q10 transaction result: {}", multi);
    assert!(multi.is_array());

    // --- typed struct decode through the crate (what the codec builds on).
    #[derive(Debug, SurrealValue)]
    struct ProbeMemory {
        id: RecordId,
        confidence: Option<Number>,
        hits: Option<Number>,
        title: Option<String>,
        valid_from: Option<Datetime>,
    }
    let rows: Vec<ProbeMemory> = db.select("probe_memory").await.expect("typed select");
    eprintln!("typed rows: {rows:?}");
    assert!(rows.len() >= 3);
    let ws1 = rows
        .iter()
        .find(|r| r.id.to_sql().contains("ws_1"))
        .expect("ws_1 row");
    // Q1: 0.7 arrives as Decimal (SurrealQL literal), 12 as Int — the codec coerces.
    assert_eq!(ws1.confidence.as_ref().and_then(Number::to_f64), Some(0.7));
    assert_eq!(ws1.hits.as_ref().and_then(Number::to_int), Some(12));
    assert_eq!(ws1.title, None);
    assert!(ws1.valid_from.is_some());
    eprintln!(
        "valid_from rendered: {:?}",
        ws1.valid_from.map(|d| d.into_inner().to_rfc3339())
    );

    // --- RELATE + graph projection (Q9): in/out keys.
    let q9 = raw_json(&db, "SELECT id, in, out FROM probe_edge;").await;
    eprintln!("Q9 graph projection: {}", q9);
    let row = q9
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_default();
    // Value serializes as a tagged enum ({"Object": {...}}) — unwrap it.
    let obj = row.get("Object").unwrap_or(&row);
    assert!(
        obj.get("in").is_some(),
        "graph projection must expose in/out"
    );
    assert!(
        obj.get("out").is_some(),
        "graph projection must expose in/out"
    );

    // --- Q12: embedding array roundtrip.
    db.query("UPDATE probe_memory:⟨ws_1⟩ SET embedding = [0.1, 0.2, 0.3];")
        .await
        .expect("embedding");
    let q12 = raw_json(&db, "SELECT embedding FROM probe_memory:⟨ws_1⟩;").await;
    eprintln!("Q12 embedding: {}", q12);
    let row12 = q12
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_default();
    let obj12 = row12.get("Object").unwrap_or(&row12);
    assert!(obj12
        .get("embedding")
        .and_then(|e| e.get("Array"))
        .map(|a| a.as_array().map(|a| a.len() == 3).unwrap_or(false))
        .unwrap_or(false));

    db.query("REMOVE TABLE probe_issue; REMOVE TABLE probe_memory; REMOVE TABLE probe_commit; REMOVE TABLE probe_repo; REMOVE TABLE probe_edge;")
        .await
        .expect("cleanup end");
}
