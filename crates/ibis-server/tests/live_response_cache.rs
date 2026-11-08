//! Live D12 `DbResponseCacheStore` integration tests (SurrealDB sidecar
//! required).
//!
//! `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`, root/root);
//! scratch database per test, removed at the end. With `IBIS_LIVE_REQUIRED=1`
//! an unreachable server FAILS instead of skipping.
//!
//! Covered: put/get roundtrip, unknown-key miss, empty-response miss, TTL
//! prune (stale row dropped, fresh row and idempotency preserved), and the
//! UNIQUE index on the cache key.

use std::sync::Arc;

use ibis_ai::{CachedResponse, ResponseCacheStore};
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_server::assembly::DbResponseCacheStore;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";
const TTL_7D: u64 = 7 * 24 * 60 * 60;

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("resp_cache_live_{}", uuid::Uuid::new_v4().simple());
    // A fresh server can reject concurrent namespace/database creation with a
    // retryable transaction conflict (parallel live suites); retry only that
    // class of error so an unreachable server still skips immediately.
    let mut last_err = None;
    for attempt in 0..5 {
        match SurrealDbExecutor::connect(&url, "ibis_db_tests", &db_name, "root", "root").await {
            Ok(exec) => return Some((Arc::new(exec) as Arc<dyn Executor>, db_name)),
            Err(err) => {
                let retryable = {
                    let lower = err.to_string().to_lowercase();
                    lower.contains("conflict") || lower.contains("retry the transaction")
                };
                last_err = Some(err);
                if !retryable {
                    break;
                }
                if attempt < 4 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
    let err = last_err.expect("at least one attempt");
    if std::env::var("IBIS_LIVE_REQUIRED").as_deref() == Ok("1") {
        panic!("IBIS_LIVE_REQUIRED=1 but SurrealDB is unreachable: {err}");
    }
    eprintln!("live: SurrealDB not reachable ({err}), skipping");
    None
}

async fn cleanup(exec: &Arc<dyn Executor>, db_name: &str) {
    let _ = exec
        .execute(&format!("REMOVE DATABASE IF EXISTS {db_name};"))
        .await;
}

#[tokio::test]
async fn response_cache_roundtrip_miss_prune_and_unique_key() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbResponseCacheStore::new(exec.clone());

    // Roundtrip: put → get returns the exact response and finish reason.
    let key = "log_analyze:hybrid[local=m;cloud=none]:abc";
    store
        .put(
            key,
            CachedResponse {
                text: "[{\"category\":\"X\"}]".to_string(),
                finish_reason: "STOP".to_string(),
            },
        )
        .await;
    let got = store.get(key).await.expect("roundtrip hit");
    assert_eq!(got.text, "[{\"category\":\"X\"}]");
    assert_eq!(got.finish_reason, "STOP");

    // Unknown key → None (the SELECT on a missing record id returns empty).
    assert!(
        store.get("log_analyze:never:written").await.is_none(),
        "miss is None"
    );

    // An empty response is never a hit: the wrapper refuses to write one,
    // and the store treats a stray empty row as a miss.
    store
        .put(
            "empty",
            CachedResponse {
                text: String::new(),
                finish_reason: "STOP".to_string(),
            },
        )
        .await;
    assert!(
        store.get("empty").await.is_none(),
        "an empty response must miss"
    );

    // The UNIQUE index on `key` rejects a second row carrying the same key —
    // the deterministic record id normally makes collisions impossible; the
    // index is the server-side backstop.
    exec.smart_query(
        "UPSERT llm_response_cache:⟨a⟩ SET key = 'dup', response = 'one', finish_reason = 'STOP', created_at = time::now();",
        Vars::new(),
    )
    .await
    .expect("first row");
    let conflict = exec
        .smart_query(
            "UPSERT llm_response_cache:⟨b⟩ SET key = 'dup', response = 'two', finish_reason = 'STOP', created_at = time::now();",
            Vars::new(),
        )
        .await;
    assert!(conflict.is_err(), "second row with the same key must fail");

    // Prune: seed a stale row (40d old) next to the fresh one; only the
    // stale row drops and the prune is idempotent.
    exec.smart_query(
        "UPSERT llm_response_cache:⟨stale⟩ SET key = 'stale', response = 'x', finish_reason = 'STOP', created_at = time::now() - 40d;",
        Vars::new(),
    )
    .await
    .expect("seed stale row");
    let pruned = store.prune(TTL_7D).await;
    assert_eq!(pruned, 1, "only the stale row prunes");
    assert!(store.get(key).await.is_some(), "fresh row survives");
    assert_eq!(store.prune(TTL_7D).await, 0, "prune is idempotent");

    cleanup(&exec, &db_name).await;
}
