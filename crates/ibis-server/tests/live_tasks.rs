//! E16 live task-store tests (SurrealDB 4.0.0 sidecar required).
//!
//! `IBIS_SPIKE_SURREAL_URL` (default `ws://127.0.0.1:38000`, root/root);
//! scratch database per test, removed at the end. With `IBIS_LIVE_REQUIRED=1`
//! an unreachable server FAILS instead of skipping.

use std::sync::Arc;

use ibis_db::executor::{Executor, SurrealDbExecutor};
use ibis_db::ids::record_id_to_plain;
use ibis_db::schema;
use ibis_optimization::tasks::{TaskCheckpoint, TaskPolicy, TaskStatus};
use ibis_server::tasks::{CreateOutcome, DbTaskStore, NewTask, TaskStore};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

async fn connect_scratch() -> Option<(Arc<dyn Executor>, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("task_live_{}", uuid::Uuid::new_v4().simple());
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

fn policy() -> TaskPolicy {
    TaskPolicy::new(30, 10, 3, 30)
}

async fn create_task(store: &DbTaskStore) -> String {
    let created = store
        .create_or_get(NewTask::memory_consolidate("proj", 20, 10, "test").unwrap())
        .await
        .expect("create task");
    let row = match created {
        CreateOutcome::Created(row) | CreateOutcome::Existing(row) => row,
    };
    record_id_to_plain(&row.id)
}

#[tokio::test]
async fn claim_reclaim_fencing_and_checkpoint() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbTaskStore::new(exec.clone());
    let id = create_task(&store).await;

    let first = store
        .claim(&id, "owner-a", &policy())
        .await
        .expect("claim a")
        .expect("first claim wins");
    assert_eq!(first.lease_epoch, 1);
    assert!(store
        .heartbeat(&id, "owner-a", &policy(), 1)
        .await
        .expect("heartbeat a"));
    assert!(store
        .checkpoint(
            &id,
            "owner-a",
            1,
            &TaskCheckpoint {
                items_done: 1,
                ..Default::default()
            },
        )
        .await
        .expect("checkpoint a"));

    // A live lease is not claimable by a second owner.
    assert!(store
        .claim(&id, "owner-b", &policy())
        .await
        .expect("claim b")
        .is_none());

    // Expire the lease; B reclaims and the epoch moves.
    exec.execute(&format!(
        "UPDATE {id} SET lease_expires_at = time::now() - 1s;"
    ))
    .await
    .expect("expire lease");
    let second = store
        .claim(&id, "owner-b", &policy())
        .await
        .expect("reclaim b")
        .expect("expired lease is reclaimable");
    assert_eq!(second.lease_epoch, 2);

    // Fencing: A can no longer heartbeat, checkpoint or finish.
    assert!(!store
        .heartbeat(&id, "owner-a", &policy(), 1)
        .await
        .expect("stale heartbeat"));
    assert!(!store
        .checkpoint(&id, "owner-a", 1, &TaskCheckpoint::default())
        .await
        .expect("stale checkpoint"));

    // The checkpoint written by A survived the reclaim.
    let current = store.get(&id).await.expect("get").expect("row");
    assert_eq!(current.checkpoint.as_ref().map(|cp| cp.items_done), Some(1));

    let final_checkpoint = current.checkpoint.clone().unwrap_or_default();
    assert!(store
        .finish(
            &id,
            "owner-b",
            2,
            TaskStatus::Completed,
            None,
            &final_checkpoint,
        )
        .await
        .expect("finish b"));
    let done = store.get(&id).await.expect("get done").expect("row");
    assert_eq!(done.status, "completed");
    assert_eq!(done.active_key, None);

    // The slot is free again: a new task can be created for the same project.
    assert!(matches!(
        store
            .create_or_get(NewTask::memory_consolidate("proj", 20, 10, "test").unwrap())
            .await
            .expect("create after release"),
        CreateOutcome::Created(_)
    ));

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn recovery_requeues_then_fails_after_max_recoveries() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbTaskStore::new(exec.clone());
    let id = create_task(&store).await;

    // Three reclaims (recoveries 1..=3), then the fourth expiry fails.
    for expected in 1..=3 {
        store
            .claim(&id, "owner-a", &policy())
            .await
            .expect("claim")
            .expect("claimable");
        exec.execute(&format!(
            "UPDATE {id} SET lease_expires_at = time::now() - 1s;"
        ))
        .await
        .expect("expire lease");
        let report = store.recover_expired(3).await.expect("recovery");
        assert_eq!(report.requeued, 1, "requeue {expected}");
        let row = store.get(&id).await.expect("get").expect("row");
        assert_eq!(row.status, "queued");
        assert_eq!(row.recoveries, expected);
        assert_eq!(row.active_key.as_deref(), Some("memory_consolidate:proj"));
    }

    store
        .claim(&id, "owner-a", &policy())
        .await
        .expect("claim")
        .expect("claimable");
    exec.execute(&format!(
        "UPDATE {id} SET lease_expires_at = time::now() - 1s;"
    ))
    .await
    .expect("expire lease");
    let report = store.recover_expired(3).await.expect("recovery");
    assert_eq!(report.failed, 1, "out of budget");
    let failed = store.get(&id).await.expect("get").expect("row");
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.error.as_deref(), Some("lease_expired"));
    assert_eq!(failed.active_key, None);
    assert!(failed.recoveries >= 3);

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn shutdown_requeue_resumes_without_a_recovery() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbTaskStore::new(exec.clone());
    let id = create_task(&store).await;

    store
        .claim(&id, "owner-a", &policy())
        .await
        .expect("claim")
        .expect("claimable");
    assert!(store.requeue(&id, "owner-a", 1).await.expect("requeue"));
    let queued = store.get(&id).await.expect("get").expect("row");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.recoveries, 0);
    assert_eq!(
        queued.active_key.as_deref(),
        Some("memory_consolidate:proj")
    );

    let resumed = store
        .claim(&id, "owner-b", &policy())
        .await
        .expect("resume")
        .expect("queued task is claimable");
    assert_eq!(resumed.lease_epoch, 2);

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn prune_terminal_tasks_by_retention() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    schema::init_schema(exec.as_ref())
        .await
        .expect("init schema");
    let store = DbTaskStore::new(exec.clone());

    let old = create_task(&store).await;
    store
        .claim(&old, "owner-a", &policy())
        .await
        .expect("claim")
        .expect("claimable");
    assert!(store
        .finish(
            &old,
            "owner-a",
            1,
            TaskStatus::Completed,
            None,
            &TaskCheckpoint::default(),
        )
        .await
        .expect("finish old"));
    exec.execute(&format!(
        "UPDATE {old} SET finished_at = time::now() - 60d;"
    ))
    .await
    .expect("age old task");

    let recent = create_task(&store).await;
    store
        .claim(&recent, "owner-a", &policy())
        .await
        .expect("claim")
        .expect("claimable");
    assert!(store
        .finish(
            &recent,
            "owner-a",
            1,
            TaskStatus::Completed,
            None,
            &TaskCheckpoint::default(),
        )
        .await
        .expect("finish recent"));

    assert_eq!(store.prune(30).await.expect("prune"), 1);
    assert!(store.get(&old).await.expect("get old").is_none());
    assert!(store.get(&recent).await.expect("get recent").is_some());

    cleanup(&exec, &db_name).await;
}
