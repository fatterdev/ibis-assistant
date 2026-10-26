use std::sync::LazyLock;

use tracing::debug;

use crate::store::CredentialStore;
use crate::types::{Credential, PROVIDER_GENERIC};

static AUTH_ERROR_MARKERS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec![
        "authentication failed",
        "terminal prompts disabled",
        "could not read username",
        "fatal: could not read from remote repository",
        "permission denied (publickey)",
        "could not read passphrase",
        "the requested url returned error: 403",
        "the requested url returned error: 401",
        "invalid username or password",
        "repository not found",
        "access denied",
    ]
});

/// Extracts the host of a git remote URL, supporting scp-like
/// `git@host:path` syntax and URL forms with userinfo/port.
pub fn get_host(raw_url: &str) -> Option<String> {
    if raw_url.starts_with("git@") {
        let after = raw_url.split('@').nth(1)?;
        return Some(after.split(':').next().unwrap_or("").to_string());
    }
    let authority = raw_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(raw_url);
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let host = authority.split(':').next().unwrap_or("");
    Some(host.to_string())
}

/// Heuristic auth-failure detection over (already masked) git output
/// drives `CredentialsRequired` responses.
pub fn is_auth_error(output: &[u8]) -> bool {
    let lowered = String::from_utf8_lossy(output).to_lowercase();
    AUTH_ERROR_MARKERS.iter().any(|m| lowered.contains(m))
}

/// Maps a remote URL to a provider name.
pub fn detect_provider(raw_url: &str) -> String {
    let lowered = raw_url.to_lowercase();
    if lowered.contains("github.com") {
        return "github".to_string();
    }
    if lowered.contains("gitlab.com") {
        return "gitlab".to_string();
    }
    if lowered.contains("dev.azure.com") || lowered.contains("visualstudio.com") {
        return "azure_devops".to_string();
    }
    PROVIDER_GENERIC.to_string()
}

/// Credential resolution for git network operations, in the original order
///: request-context token override → DB
/// credential by full URL then by host → legacy config token.
pub async fn resolve_credential(
    request_token: Option<&str>,
    store: Option<&CredentialStore>,
    origin_url: &str,
    legacy_token: Option<&str>,
) -> Option<Credential> {
    if let Some(token) = request_token.filter(|t| !t.is_empty()) {
        debug!("Using Git runtime token override from context");
        return Some(Credential::token_credential(
            origin_url,
            PROVIDER_GENERIC,
            token,
        ));
    }

    if let Some(store) = store {
        if !origin_url.trim().is_empty() {
            if let Ok(Some(cred)) = store.get_credential(origin_url).await {
                debug!("Using Git credential from DB (URL specific match): {origin_url}");
                return Some(cred);
            }
            if let Some(host) = get_host(origin_url).filter(|h| !h.is_empty()) {
                if let Ok(Some(cred)) = store.get_credential(&host).await {
                    debug!("Using Git credential from DB (host match): {host}");
                    return Some(cred);
                }
            }
        }
    }

    if let Some(token) = legacy_token.filter(|t| !t.is_empty()) {
        debug!("Using legacy config Git token fallback");
        return Some(Credential::token_credential(
            origin_url,
            PROVIDER_GENERIC,
            token,
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_host_forms() {
        assert_eq!(
            get_host("git@github.com:org/repo.git"),
            Some("github.com".to_string())
        );
        assert_eq!(
            get_host("https://oauth2:tok@github.com/a/b"),
            Some("github.com".to_string())
        );
        assert_eq!(
            get_host("https://github.com:8443/a"),
            Some("github.com".to_string())
        );
        assert_eq!(
            get_host("ssh://git@gitlab.com:2222/a"),
            Some("gitlab.com".to_string())
        );
        assert_eq!(
            get_host("https://github.com/x"),
            Some("github.com".to_string())
        );
        assert_eq!(get_host("notaurl"), Some("notaurl".to_string()));
    }

    #[test]
    fn auth_error_markers() {
        assert!(is_auth_error(
            b"fatal: Authentication failed for 'https://x'"
        ));
        assert!(is_auth_error(b"fatal: terminal prompts disabled"));
        assert!(is_auth_error(b"remote: Repository not found."));
        assert!(!is_auth_error(b"fatal: not a git repository"));
        assert!(!is_auth_error(b""));
    }

    #[test]
    fn detect_provider_urls() {
        assert_eq!(detect_provider("https://github.com/a/b"), "github");
        assert_eq!(detect_provider("https://gitlab.com/a/b"), "gitlab");
        assert_eq!(detect_provider("https://dev.azure.com/a/b"), "azure_devops");
        assert_eq!(
            detect_provider("https://visualstudio.com/a"),
            "azure_devops"
        );
        assert_eq!(detect_provider("https://example.com/a"), "generic");
    }
}
