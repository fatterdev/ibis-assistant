use std::sync::Arc;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;

use crate::error::GitError;
use crate::types::Credential;

pub const GIT_CREDENTIAL_TABLE: &str = "git_credential";

static INVALID_ID_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-z0-9_]").expect("valid static regex"));
static MULTI_UNDERSCORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_+").expect("valid static regex"));

/// Makes a string safe as a SurrealDB record-ID suffix (
/// `db.SanitizeID`: lowercase, strip quotes, replace invalid chars, collapse
/// and trim underscores).
pub fn sanitize_record_key(s: &str) -> String {
    let s = s.to_lowercase();
    let s = s.replace(['\'', '"', '`'], "");
    let s = INVALID_ID_CHARS.replace_all(&s, "_").into_owned();
    let s = MULTI_UNDERSCORE.replace_all(&s, "_").into_owned();
    s.trim_matches('_').to_string()
}

/// Minimal DB surface the credential store needs (`db.Executor`). The
/// SurrealDB-backed implementation is wired by the ibis-db lane.
#[async_trait]
pub trait CredentialDb: Send + Sync {
    async fn smart_query(&self, sql: &str, vars: &Value) -> Result<Value, GitError>;
    async fn execute(&self, sql: &str) -> Result<Value, GitError>;
}

/// Git credential storage helpers. Credentials
/// live under `git_credential:<sanitized target>`; resolution order
/// (request-context token → DB by URL then host → legacy config) lives in
/// [`crate::resolve::resolve_credential`].
pub struct CredentialStore {
    db: Option<Arc<dyn CredentialDb>>,
}

impl CredentialStore {
    pub fn new(db: Option<Arc<dyn CredentialDb>>) -> Self {
        Self { db }
    }

    pub async fn save_credential(&self, cred: &Credential) -> Result<(), GitError> {
        let Some(db) = self.db.as_ref() else {
            return Err(GitError::other("database client not initialized"));
        };
        let id = format!(
            "{GIT_CREDENTIAL_TABLE}:{}",
            sanitize_record_key(cred.target.trim())
        );
        let query = format!("UPSERT {id} CONTENT $cred;");
        let vars = serde_json::json!({ "cred": cred });
        db.smart_query(&query, &vars).await.map(|_| ())
    }

    pub async fn get_credential(&self, target: &str) -> Result<Option<Credential>, GitError> {
        let Some(db) = self.db.as_ref() else {
            return Ok(None);
        };
        let target = target.trim();
        let safe_id = sanitize_record_key(target);
        if safe_id.is_empty() {
            return Ok(None);
        }
        let id = format!("{GIT_CREDENTIAL_TABLE}:{safe_id}");
        let query = format!("SELECT * FROM {id};");
        let res = db.execute(&query).await?;
        let raw = match res {
            Value::Array(items) => items.into_iter().next(),
            Value::Null => None,
            value => Some(value),
        };
        let Some(raw) = raw else {
            return Ok(None);
        };
        if !raw.is_object() {
            return Ok(None);
        }
        serde_json::from_value(raw)
            .map(Some)
            .map_err(|e| GitError::other(format!("decode credential: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_reference_semantics() {
        assert_eq!(sanitize_record_key("github.com"), "github_com");
        assert_eq!(sanitize_record_key("Git`Hub\"COM'"), "githubcom");
        assert_eq!(sanitize_record_key("a--b  c"), "a_b_c");
        assert_eq!(sanitize_record_key("__x__"), "x");
        assert_eq!(
            sanitize_record_key("dev.azure.com/org"),
            "dev_azure_com_org"
        );
    }
}
