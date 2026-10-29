use serde::{Deserialize, Serialize};

pub const PROVIDER_GITHUB: &str = "github";
pub const PROVIDER_GITLAB: &str = "gitlab";
pub const PROVIDER_AZURE_DEVOPS: &str = "azure_devops";
pub const PROVIDER_GENERIC: &str = "generic";

/// Loose string-typed auth mode (parity with `gitrepo.AuthType): unknown
/// values roundtrip through the database and are ignored by the runner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthType(pub String);

impl AuthType {
    pub fn token() -> Self {
        Self("token".to_string())
    }

    pub fn basic() -> Self {
        Self("basic".to_string())
    }

    pub fn ssh() -> Self {
        Self("ssh".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored git credential. JSON field names and `omitempty` semantics match
/// the original struct (`target`, `provider`, `auth_type`, `token`, `username`,
/// `ssh_private_key`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub auth_type: AuthType,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_private_key: String,
}

impl Credential {
    pub fn token_credential(
        target: impl Into<String>,
        provider: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Credential {
            target: target.into(),
            provider: provider.into(),
            auth_type: AuthType::token(),
            token: token.into(),
            username: String::new(),
            ssh_private_key: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_matches_reference_tags() {
        let cred = Credential {
            target: "github.com".to_string(),
            provider: "github".to_string(),
            auth_type: AuthType::token(),
            token: "tok".to_string(),
            username: String::new(),
            ssh_private_key: String::new(),
        };
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "target": "github.com",
                "provider": "github",
                "auth_type": "token",
                "token": "tok"
            })
        );
        let back: Credential = serde_json::from_value(json).unwrap();
        assert_eq!(back, cred);
    }

    #[test]
    fn unknown_auth_type_roundtrips() {
        let cred: Credential =
            serde_json::from_str(r#"{"target":"x","provider":"p","auth_type":"weird"}"#).unwrap();
        assert_eq!(cred.auth_type.as_str(), "weird");
    }
}
