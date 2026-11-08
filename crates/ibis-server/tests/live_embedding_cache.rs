//! Live B6 `DbEmbeddingCacheStore` integration tests (SurrealDB sidecar
//! required).
//!
//! `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`, root/root);
//! scratch database per test, removed at the end. With `IBIS_LIVE_REQUIRED=1`
//! an unreachable server FAILS instead of skipping.
//!
//! Covered: put/get roundtrip, unknown-key miss, dimension-mismatch miss,
//! TTL prune (stale row dropped, fresh row and idempotency preserved), and
//! the UNIQUE index on the cache key.

use std::sync::Arc;

use ibis_ai::EmbeddingCacheStore;
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::schema;
use ibis_server::assembly::DbEmbeddingCacheStore;

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";
const TTL_30D: u64 = 30 * 24 * 60 * 60;

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("emb_cache_live_{}", uuid::Uuid::new_v4().simple());
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

#[tokio::test]
async fn embedding_cache_roundtrip_miss_mismatch_and_prune() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbEmbeddingCacheStore::new(exec.clone(), "m".into(), 2);

    // Roundtrip: put → get returns the exact vector.
    let key = "m:2:roundtrip";
    store.put(key, vec![0.25, -0.75]).await;
    let got = store.get(key).await.expect("roundtrip hit");
    assert_eq!(got, vec![0.25, -0.75]);

    // Unknown key → None (the SELECT on a missing record id returns empty).
    assert!(
        store.get("m:2:never-written").await.is_none(),
        "miss is None"
    );

    // Dimension mismatch reads as a miss, never as a poison.
    let wide = DbEmbeddingCacheStore::new(exec.clone(), "m".into(), 3);
    let wide_key = "m:3:wide";
    wide.put(wide_key, vec![0.1, 0.2, 0.3]).await;
    assert!(
        store.get(wide_key).await.is_none(),
        "a 3-float row must miss under a dim-2 adapter"
    );
    assert!(
        wide.get(wide_key).await.is_some(),
        "matching dim still hits"
    );

    // Prune: seed a stale row (40d old) next to the fresh one; only the
    // stale row drops and the prune is idempotent.
    exec.smart_query(
        "UPSERT embedding_cache:⟨stale⟩ SET key = 'stale', model = 'm', dim = 1, embedding = [0.5], created_at = time::now() - 40d;",
        Vars::new(),
    )
    .await
    .expect("seed stale row");
    let pruned = store.prune(TTL_30D).await;
    assert_eq!(pruned, 1, "only the stale row prunes");
    assert!(
        store.get(key).await.is_some(),
        "fresh row survives the prune"
    );
    let pruned_again = store.prune(TTL_30D).await;
    assert_eq!(pruned_again, 0, "prune is idempotent");

    cleanup(&exec, &db_name).await;
}

/// The UNIQUE index on `key` rejects a second row carrying the same key —
/// the deterministic record id normally makes collisions impossible; the
/// index is the server-side backstop.
#[tokio::test]
async fn unique_key_index_backstop() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");

    exec.smart_query(
        "UPSERT embedding_cache:⟨a⟩ SET key = 'dup', model = 'm', dim = 1, embedding = [0.1], created_at = time::now();",
        Vars::new(),
    )
    .await
    .expect("first row");
    let conflict = exec
        .smart_query(
            "UPSERT embedding_cache:⟨b⟩ SET key = 'dup', model = 'm', dim = 1, embedding = [0.2], created_at = time::now();",
            Vars::new(),
        )
        .await;
    assert!(conflict.is_err(), "second row with the same key must fail");

    cleanup(&exec, &db_name).await;
}
