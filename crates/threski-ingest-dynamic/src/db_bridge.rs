//! `threski_db::Executor` → `threski_git::CredentialDb` bridge.
//!
//! The credential store (lane D) speaks loose `serde_json::Value`s so it can
//! be faked cheaply in tests; the live graph database speaks the typed
//! `surrealdb` value codec. This adapter joins the two:
//!
//! - **Read path** (`get_credential` → `SELECT * FROM git_credential:<id>`):
//!   rows decode through a typed `SurrealValue` struct ( —
//!   decode typed, never hand-dig values), then re-serialize in the exact
//!   `Credential` JSON shape the store expects.
//! - **Write path** (`save_credential` → `UPSERT … CONTENT $cred`): the bound
//!   `$cred` variable is converted recursively from JSON into **native**
//!   `Value` shapes (the serde JSON view of `Value` is a tagged enum and must
//!   never be bound directly — see the H0 spike notes).

use std::sync::Arc;

use async_trait::async_trait;
use surrealdb::types::{Array, Object, SurrealValue};

use threski_db::error::DbError;
use threski_db::executor::{Executor, Vars};
use threski_db::value::decode_row_list;
use threski_git::{Credential, CredentialDb, GitError};
use surrealdb::types::Value;

/// Decodes one `git_credential` row (all fields optional: schemaless table,
/// rows may predate later fields).
#[derive(SurrealValue)]
struct GitCredentialRow {
    target: Option<String>,
    provider: Option<String>,
    auth_type: Option<String>,
    token: Option<String>,
    username: Option<String>,
    ssh_private_key: Option<String>,
}

impl From<GitCredentialRow> for Credential {
    fn from(row: GitCredentialRow) -> Self {
        Credential {
            target: row.target.unwrap_or_default(),
            provider: row.provider.unwrap_or_default(),
            auth_type: threski_git::AuthType(row.auth_type.unwrap_or_default()),
            token: row.token.unwrap_or_default(),
            username: row.username.unwrap_or_default(),
            ssh_private_key: row.ssh_private_key.unwrap_or_default(),
        }
    }
}

/// Adapter turning any [`Executor`] into the credential store's DB seam.
pub struct ExecutorCredentialDb(pub Arc<dyn Executor>);

#[async_trait]
impl CredentialDb for ExecutorCredentialDb {
    async fn execute(&self, sql: &str) -> Result<serde_json::Value, GitError> {
        let outcome = self
            .0
            .execute(sql)
            .await
            .map_err(db_error("credential query"))?;
        credential_rows_to_json(outcome.first().unwrap_or(&Value::None), sql)
    }

    async fn smart_query(
        &self,
        sql: &str,
        vars: &serde_json::Value,
    ) -> Result<serde_json::Value, GitError> {
        let serde_json::Value::Object(map) = vars else {
            return Err(GitError::other("credential db vars must be a JSON object"));
        };
        let mut bound = Vars::new();
        for (key, value) in map {
            bound.insert(key.clone(), json_to_value(value).map_err(GitError::other)?);
        }
        let outcome = self
            .0
            .smart_query(sql, bound)
            .await
            .map_err(db_error("credential query"))?;
        value_to_json(outcome.first().unwrap_or(&Value::None))
    }
}

fn db_error(context: &'static str) -> impl Fn(DbError) -> GitError {
    move |e| GitError::other(format!("{context}: {e}"))
}

fn credential_rows_to_json(first: &Value, sql: &str) -> Result<serde_json::Value, GitError> {
    let rows: Vec<GitCredentialRow> =
        decode_row_list(first, sql).map_err(db_error("decode credential row"))?;
    match rows.into_iter().next() {
        Some(row) => serde_json::to_value(Credential::from(row))
            .map_err(|e| GitError::other(format!("encode credential: {e}"))),
        None => Ok(serde_json::Value::Null),
    }
}

/// Recursive native-shape conversion JSON → `Value` (write direction).
pub(crate) fn json_to_value(value: &serde_json::Value) -> Result<Value, String> {
    Ok(match value {
        serde_json::Value::Null => Value::None,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(surrealdb::types::Number::Int(i))
            } else if let Some(f) = n.as_f64() {
                Value::Number(surrealdb::types::Number::Float(f))
            } else {
                return Err(format!("unsupported number {n}"));
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_value(item)?);
            }
            Value::Array(Array::from(out))
        }
        serde_json::Value::Object(map) => {
            let mut obj = Object::new();
            for (key, item) in map {
                obj.insert(key.clone(), json_to_value(item)?);
            }
            Value::Object(obj)
        }
    })
}

/// Recursive native-shape conversion `Value` → JSON (read direction, used by
/// the write-path response which the store discards).
pub(crate) fn value_to_json(value: &Value) -> Result<serde_json::Value, GitError> {
    Ok(match value {
        Value::None | Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => match n {
            surrealdb::types::Number::Int(i) => serde_json::json!(i),
            other => {
                let f = other
                    .to_f64()
                    .ok_or_else(|| GitError::other(format!("cannot render number {other}")))?;
                serde_json::json!(f)
            }
        },
        Value::String(s) => serde_json::json!(s),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(value_to_json(item)?);
            }
            serde_json::Value::Array(out)
        }
        Value::Object(obj) => {
            let mut map = serde_json::Map::new();
            for (key, item) in obj.iter() {
                map.insert(key.clone(), value_to_json(item)?);
            }
            serde_json::Value::Object(map)
        }
        other => {
            return Err(GitError::other(format!(
                "credential bridge cannot render kind {}",
                other.kind()
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_value_native_shapes() {
        let json = serde_json::json!({
            "target": "github.com",
            "provider": "generic",
            "auth_type": "token",
            "token": "tok",
            "n": 42,
            "f": 1.5,
            "b": true,
            "arr": ["a", 1],
            "nested": {"k": "v"},
            "nul": null
        });
        let value = json_to_value(&json).unwrap();
        let Value::Object(obj) = &value else {
            panic!("expected object, got {value:?}");
        };
        assert!(matches!(obj.get("target"), Some(Value::String(s)) if s == "github.com"));
        assert!(matches!(
            obj.get("n"),
            Some(Value::Number(surrealdb::types::Number::Int(42)))
        ));
        assert!(matches!(
            obj.get("f"),
            Some(Value::Number(surrealdb::types::Number::Float(_)))
        ));
        assert!(matches!(obj.get("b"), Some(Value::Bool(true))));
        assert!(matches!(obj.get("nul"), Some(Value::None)));
        let Value::Array(arr) = obj.get("arr").unwrap() else {
            panic!("expected array");
        };
        assert_eq!(arr.len(), 2);
        let Value::Object(nested) = obj.get("nested").unwrap() else {
            panic!("expected nested object");
        };
        assert!(matches!(nested.get("k"), Some(Value::String(_))));
    }

    #[test]
    fn value_to_json_roundtrip() {
        let mut obj = Object::new();
        obj.insert("a", Value::String("x".into()));
        obj.insert("n", Value::Number(surrealdb::types::Number::Int(7)));
        let json = value_to_json(&Value::Object(obj)).unwrap();
        assert_eq!(json, serde_json::json!({"a": "x", "n": 7}));
        assert_eq!(
            value_to_json(&Value::None).unwrap(),
            serde_json::Value::Null
        );
    }

    /// Capturing fake executor (mirrors the original MockDB fakes).
    struct FakeExecutor {
        captured: tokio::sync::Mutex<Vec<(String, Vars)>>,
    }

    #[async_trait]
    impl Executor for FakeExecutor {
        async fn execute(&self, sql: &str) -> Result<threski_db::executor::QueryOutcome, DbError> {
            self.captured
                .lock()
                .await
                .push((sql.to_string(), Vars::new()));
            // `SELECT * FROM git_credential:x` → one row.
            let mut obj = Object::new();
            obj.insert("target", Value::String("github.com".into()));
            obj.insert("provider", Value::String("generic".into()));
            obj.insert("auth_type", Value::String("token".into()));
            obj.insert("token", Value::String("tok".into()));
            // Extra unknown field (like `id`) must be tolerated.
            obj.insert("id", Value::String("git_credential:github_com".into()));
            Ok(threski_db::executor::QueryOutcome::Single(Value::Array(
                Array::from(vec![Value::Object(obj)]),
            )))
        }

        async fn smart_query(
            &self,
            sql: &str,
            vars: Vars,
        ) -> Result<threski_db::executor::QueryOutcome, DbError> {
            self.captured.lock().await.push((sql.to_string(), vars));
            Ok(threski_db::executor::QueryOutcome::Single(Value::None))
        }
    }

    #[tokio::test]
    async fn read_path_decodes_typed_rows() {
        let fake = Arc::new(FakeExecutor {
            captured: tokio::sync::Mutex::new(Vec::new()),
        });
        let db: Arc<dyn Executor> = fake.clone();
        let adapter = ExecutorCredentialDb(db);
        let raw = adapter
            .execute("SELECT * FROM git_credential:github_com;")
            .await
            .expect("execute");
        let cred: Credential = serde_json::from_value(raw).expect("credential shape");
        assert_eq!(cred.target, "github.com");
        assert_eq!(cred.provider, "generic");
        assert_eq!(cred.auth_type.as_str(), "token");
        assert_eq!(cred.token, "tok");
        let captured = fake.captured.lock().await;
        assert!(captured[0].0.contains("git_credential"));
    }

    #[tokio::test]
    async fn write_path_binds_native_values() {
        let fake = Arc::new(FakeExecutor {
            captured: tokio::sync::Mutex::new(Vec::new()),
        });
        let db: Arc<dyn Executor> = fake.clone();
        let adapter = ExecutorCredentialDb(db);
        let vars = serde_json::json!({"cred": {"target": "x", "provider": "generic"}});
        adapter
            .smart_query("UPSERT git_credential:x CONTENT $cred;", &vars)
            .await
            .expect("smart_query");

        let captured = fake.captured.lock().await;
        let (_, bound) = &captured[0];
        let Value::Object(cred) = bound.get("cred").expect("cred var") else {
            panic!("cred must be a native object, got {bound:?}");
        };
        assert!(matches!(cred.get("target"), Some(Value::String(s)) if s == "x"));
    }
}
