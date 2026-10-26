//! Live SurrealDB regression test for the credential bridge (db_bridge):
//! a real `git_credential` row is decoded through the typed-row adapter and
//! resolved by the lane-D credential resolution order. Skips gracefully when
//! the sidecar (`ws://127.0.0.1:38000`, root/root) is unreachable.

use std::sync::Arc;

use ibis_core::config::Config;
use ibis_db::executor::{Executor, SurrealDbExecutor, Vars};
use ibis_db::schema::names::GIT_CREDENTIAL;
use ibis_db::value::{Object, RecordId, Value};
use ibis_git::{resolve_credential, CredentialStore};

use ibis_ingest_dynamic::{sync_workspace, DynamicError, ExecutorCredentialDb};

const DEFAULT_URL: &str = "ws://127.0.0.1:38000";

async fn connect_scratch() -> Option<(SurrealDbExecutor, String)> {
    let url = std::env::var("IBIS_SPIKE_SURREAL_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let db_name = format!("lanee_dyn_{}", uuid::Uuid::new_v4().simple());
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
async fn live_credential_resolution_through_bridge() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };
    ibis_db::schema::init_schema(&exec)
        .await
        .expect("init schema");

    // Seed one credential row for the github.com host.
    let cred_id = format!("{GIT_CREDENTIAL}:⟨github_com⟩");
    let mut cred_obj = Object::new();
    cred_obj.insert("target", Value::String("github.com".into()));
    cred_obj.insert("provider", Value::String("generic".into()));
    cred_obj.insert("auth_type", Value::String("token".into()));
    cred_obj.insert("token", Value::String("live-token".into()));
    let mut vars = Vars::new();
    vars.insert("cred".to_string(), Value::Object(cred_obj));
    exec.smart_query(&format!("UPSERT {cred_id} CONTENT $cred;"), vars)
        .await
        .expect("seed credential");

    let db: Arc<dyn Executor> = Arc::new(exec.clone());
    let store = CredentialStore::new(Some(Arc::new(ExecutorCredentialDb(db))));
    let cred = resolve_credential(None, Some(&store), "https://github.com/x/y", None)
        .await
        .expect("credential must resolve by host");
    assert_eq!(cred.token, "live-token");
    assert_eq!(cred.target, "github.com");
    assert_eq!(cred.auth_type.as_str(), "token");

    cleanup(&exec, &db_name).await;
}

#[tokio::test]
async fn live_owned_clone_syncs_and_reports_head() {
    let Some((exec, db_name)) = connect_scratch().await else {
        return;
    };

    // Local source repo + bare remote.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().display().to_string();
    let src = std::path::Path::new(&root)
        .join("src")
        .display()
        .to_string();
    let remote = std::path::Path::new(&root)
        .join("remote.git")
        .display()
        .to_string();
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let runner = ibis_git::GitRunner::anonymous();
    runner
        .run(&remote, &["init", "-q", "--bare"])
        .await
        .unwrap();
    runner.run(&src, &["init", "-q"]).await.unwrap();
    runner
        .run(&src, &["config", "user.email", "t@e.com"])
        .await
        .unwrap();
    runner
        .run(&src, &["config", "user.name", "T"])
        .await
        .unwrap();
    std::fs::write(std::path::Path::new(&src).join("f.txt"), "x").unwrap();
    runner.run(&src, &["add", "."]).await.unwrap();
    runner
        .run(&src, &["commit", "-q", "-m", "c1"])
        .await
        .unwrap();
    runner
        .run(&src, &["remote", "add", "origin", &remote])
        .await
        .unwrap();
    let branch = String::from_utf8_lossy(
        &runner
            .run(&src, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap(),
    )
    .trim()
    .to_string();
    runner
        .run(&src, &["push", "-q", "origin", &branch])
        .await
        .unwrap();
    let head = String::from_utf8_lossy(&runner.run(&src, &["rev-parse", "HEAD"]).await.unwrap())
        .trim()
        .to_string();

    let discovery = tempfile::tempdir().unwrap();
    let cfg = Config {
        runtime_mode: "server".to_string(),
        discovery_root: discovery.path().display().to_string(),
        ..Default::default()
    };

    let out = sync_workspace(
        &cfg,
        Some(Arc::new(exec.clone()) as Arc<dyn Executor>),
        None,
        "demo",
        &remote,
        "",
        "",
    )
    .await
    .expect("sync failed");
    assert!(out.is_aligned);
    assert_eq!(out.actual_commit, head);
    assert_eq!(
        out.repo_path,
        std::path::Path::new(&discovery.path().display().to_string())
            .join("dynamic")
            .join("demo")
            .display()
            .to_string()
    );

    // Re-sync over the existing clone: clean/reset/fetch path.
    std::fs::write(
        std::path::Path::new(&out.repo_path).join("stray.txt"),
        "dirty",
    )
    .unwrap();
    let out2 = sync_workspace(
        &cfg,
        Some(Arc::new(exec.clone()) as Arc<dyn Executor>),
        None,
        "demo",
        &remote,
        "",
        "",
    )
    .await
    .expect("re-sync failed");
    assert_eq!(out2.actual_commit, head);
    assert!(
        !std::path::Path::new(&out2.repo_path)
            .join("stray.txt")
            .exists(),
        "clean -fd must remove the stray file"
    );

    // The ingest flow's upstream repo upsert runs against the live DB to
    // prove the executor handle is usable end to end.
    let mut vars = Vars::new();
    vars.insert("name".to_string(), Value::from_t("live-repo"));
    exec.smart_query("UPSERT repo:⟨live_repo⟩ SET name = $name;", vars)
        .await
        .expect("repo upsert");
    let mut probe = Vars::new();
    probe.insert(
        "ids".to_string(),
        Value::from_t(vec![Value::RecordId(RecordId::new("repo", "live_repo"))]),
    );
    let rows = exec
        .smart_query("SELECT VALUE id FROM repo WHERE id IN $ids", probe)
        .await
        .expect("probe");
    let ids = rows
        .value_ids("SELECT VALUE id FROM repo WHERE id IN $ids")
        .unwrap();
    assert_eq!(ids, vec!["repo:⟨live_repo⟩"]);

    cleanup(&exec, &db_name).await;
}

#[test]
fn credentials_required_is_matchable() {
    // The server lane branches on this shape ( errors.As).
    let err = DynamicError::credentials_required("https://github.com/x", "msg");
    assert!(matches!(err, DynamicError::CredentialsRequired { .. }));
}
