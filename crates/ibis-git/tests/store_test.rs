//! Store tests plus the credential
//! resolution-order tests (request-context token → DB by URL then host →
//! legacy config).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use ibis_git::{
    resolve_credential, sanitize_record_key, AuthType, Credential, CredentialDb, CredentialStore,
    GitError,
};

#[derive(Default)]
struct MockDb {
    save_calls: Mutex<Vec<(String, Value)>>,
    exec_calls: Mutex<Vec<String>>,
    rows: Mutex<HashMap<String, Value>>,
}

impl MockDb {
    fn set_row(&self, record_id: &str, row: Value) {
        self.rows.lock().unwrap().insert(record_id.to_string(), row);
    }
}

#[async_trait]
impl CredentialDb for MockDb {
    async fn smart_query(&self, sql: &str, vars: &Value) -> Result<Value, GitError> {
        self.save_calls
            .lock()
            .unwrap()
            .push((sql.to_string(), vars.clone()));
        Ok(Value::Null)
    }

    async fn execute(&self, sql: &str) -> Result<Value, GitError> {
        self.exec_calls.lock().unwrap().push(sql.to_string());
        let record_id = sql
            .trim()
            .trim_start_matches("SELECT * FROM ")
            .trim_end_matches(';');
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(record_id)
            .cloned()
            .unwrap_or(Value::Array(vec![])))
    }
}

#[tokio::test]
async fn test_credential_store_save_and_get() {
    let db = Arc::new(MockDb::default());
    db.set_row(
        "git_credential:github_com",
        json!({
            "target": "github.com",
            "provider": "github",
            "auth_type": "token",
            "token": "ghp_test123"
        }),
    );
    let store = CredentialStore::new(Some(db.clone()));

    let cred = Credential {
        target: "github.com".to_string(),
        provider: "github".to_string(),
        auth_type: AuthType::token(),
        token: "ghp_test123".to_string(),
        username: String::new(),
        ssh_private_key: String::new(),
    };
    store.save_credential(&cred).await.unwrap();

    let (sql, vars) = {
        let saves = db.save_calls.lock().unwrap();
        assert_eq!(saves.len(), 1);
        (saves[0].0.clone(), saves[0].1.clone())
    };
    assert_eq!(sql, "UPSERT git_credential:github_com CONTENT $cred;");
    assert_eq!(vars["cred"]["token"], "ghp_test123");
    assert_eq!(vars["cred"]["auth_type"], "token");

    let retrieved = store.get_credential("github.com").await.unwrap();
    let retrieved = retrieved.expect("expected to retrieve a credential");
    assert_eq!(retrieved.token, "ghp_test123");

    let execs = db.exec_calls.lock().unwrap();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0], "SELECT * FROM git_credential:github_com;");
}

#[tokio::test]
async fn test_credential_store_get_not_found() {
    let db = Arc::new(MockDb::default());
    let store = CredentialStore::new(Some(db.clone()));

    let retrieved = store.get_credential("github.com").await.unwrap();
    assert!(retrieved.is_none(), "expected None result");
}

#[tokio::test]
async fn test_credential_store_nil_db() {
    let store = CredentialStore::new(None);
    let err = store
        .save_credential(&Credential::default())
        .await
        .expect_err("expected error on disabled store save");
    assert!(err.to_string().contains("database client not initialized"));

    let res = store.get_credential("github.com").await.unwrap();
    assert!(res.is_none(), "expected None result on disabled store get");
}

#[tokio::test]
async fn save_trims_target_before_sanitizing() {
    let db = Arc::new(MockDb::default());
    db.set_row(
        "git_credential:github_com",
        json!({"target": "github.com", "provider": "github", "auth_type": "token", "token": "t"}),
    );
    let store = CredentialStore::new(Some(db.clone()));
    let cred = Credential {
        target: "  GitHub.COM  ".to_string(),
        provider: "github".to_string(),
        auth_type: AuthType::token(),
        token: "t".to_string(),
        username: String::new(),
        ssh_private_key: String::new(),
    };
    store.save_credential(&cred).await.unwrap();
    let sql = db.save_calls.lock().unwrap()[0].0.clone();
    assert_eq!(sql, "UPSERT git_credential:github_com CONTENT $cred;");
    // save/get now agree on the record id ( saved untrimmed targets)
    assert!(store.get_credential("github.com").await.unwrap().is_some());
}

#[tokio::test]
async fn get_with_empty_sanitize_result_is_none() {
    let db = Arc::new(MockDb::default());
    let store = CredentialStore::new(Some(db.clone()));
    assert!(store.get_credential("''").await.unwrap().is_none());
}

#[tokio::test]
async fn test_resolution_order_request_token_wins() {
    let got = resolve_credential(
        Some("ctxtok"),
        None,
        "https://x.org/repo",
        Some("legacytok"),
    )
    .await
    .unwrap();
    assert_eq!(got.token, "ctxtok");
    assert_eq!(got.provider, "generic");
    assert_eq!(got.target, "https://x.org/repo");
    assert_eq!(got.auth_type, AuthType::token());
}

#[tokio::test]
async fn test_resolution_order_empty_request_token_falls_through() {
    let got = resolve_credential(Some(""), None, "https://x.org/repo", Some("legacytok"))
        .await
        .unwrap();
    assert_eq!(got.token, "legacytok");
}

#[tokio::test]
async fn test_resolution_order_db_url_then_host() {
    let db = Arc::new(MockDb::default());
    let origin = "https://x.org/repo";
    let url_id = format!("git_credential:{}", sanitize_record_key(origin));
    let host_id = format!("git_credential:{}", sanitize_record_key("x.org"));
    assert_eq!(url_id, "git_credential:https_x_org_repo");
    assert_eq!(host_id, "git_credential:x_org");

    db.set_row(
        &url_id,
        json!({"target": origin, "provider": "generic", "auth_type": "token", "token": "urltok"}),
    );
    db.set_row(
        &host_id,
        json!({"target": "x.org", "provider": "generic", "auth_type": "token", "token": "hosttok"}),
    );
    let store = CredentialStore::new(Some(db));

    // URL-specific match wins over host match.
    let got = resolve_credential(None, Some(&store), origin, None)
        .await
        .unwrap();
    assert_eq!(got.token, "urltok");
}

#[tokio::test]
async fn test_resolution_order_host_fallback() {
    let db = Arc::new(MockDb::default());
    let origin = "https://x.org/repo";
    db.set_row(
        "git_credential:x_org",
        json!({"target": "x.org", "provider": "generic", "auth_type": "token", "token": "hosttok"}),
    );
    let store = CredentialStore::new(Some(db));
    let got = resolve_credential(None, Some(&store), origin, None)
        .await
        .unwrap();
    assert_eq!(got.token, "hosttok");
}

#[tokio::test]
async fn test_resolution_order_legacy_fallback_and_none() {
    let db = Arc::new(MockDb::default());
    let store = CredentialStore::new(Some(db));
    let got = resolve_credential(None, Some(&store), "https://x.org/repo", Some("legacytok"))
        .await
        .unwrap();
    assert_eq!(got.token, "legacytok");

    assert!(resolve_credential(None, None, "https://x.org/repo", None)
        .await
        .is_none());
    // Empty origin URL never consults the DB.
    let db = Arc::new(MockDb::default());
    db.set_row(
        "git_credential:x_org",
        json!({"target": "x.org", "provider": "generic", "auth_type": "token", "token": "t"}),
    );
    let store = CredentialStore::new(Some(db));
    assert!(resolve_credential(None, Some(&store), "   ", None)
        .await
        .is_none());
}
