//! Typed errors for the repopr service and providers ( returned
//! `fmt.Errorf` strings; the messages below preserve the original formats).
//!
//! Error response bodies are capped at [`MAX_ERROR_BODY`] before inclusion
//! (REP-A01: read the body unbounded).

use thiserror::Error;

/// Maximum bytes of an error response body included in error messages
/// (REP-A01: `io.ReadAll was unbounded).
pub const MAX_ERROR_BODY: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum RepoprError {
    #[error("repo provider '{0}' not registered")]
    ProviderNotRegistered(String),
    #[error("azure devops project and repository are required")]
    AzureDevOpsProjectRepoRequired,
    #[error("source_branch is required")]
    SourceBranchRequired,
    #[error("pr_id is required")]
    PrIdRequired,
    /// REP-A03 (D15): sent `Authorization: Basic base64(":")`
    /// when no PAT resolved and surfaced the server's 401 body instead; the
    /// port fails fast with this typed error so wave 4 can map it to a
    /// credentials-required response like the git lane does.
    #[error(
        "azure devops PAT is required (configure ticketing.providers.azure_devops.pat or send the X-Azure-DevOps-PAT header)"
    )]
    CredentialsRequired,
    #[error("azure devops create pr error {status}: {body}")]
    CreateApi { status: u16, body: String },
    #[error("azure devops complete pr error {status}: {body}")]
    CompleteApi { status: u16, body: String },
    #[error("azure devops request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// Covers both payload encode ( swallowed the error — REP-A04) and
    /// response decode ( returned the raw `json.Decode` error).
    #[error("azure devops json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Capped, lossy rendering of a non-2xx response body (same helper as the
/// redmine client, E-A11).
pub(crate) fn error_body(bytes: &[u8]) -> String {
    let capped = &bytes[..bytes.len().min(MAX_ERROR_BODY)];
    let mut text = String::from_utf8_lossy(capped).into_owned();
    if bytes.len() > MAX_ERROR_BODY {
        text.push_str("…(truncated)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_reference_formats() {
        let e = RepoprError::ProviderNotRegistered("gitlab".into());
        assert_eq!(e.to_string(), "repo provider 'gitlab' not registered");
        let e = RepoprError::AzureDevOpsProjectRepoRequired;
        assert_eq!(
            e.to_string(),
            "azure devops project and repository are required"
        );
        let e = RepoprError::SourceBranchRequired;
        assert_eq!(e.to_string(), "source_branch is required");
        let e = RepoprError::PrIdRequired;
        assert_eq!(e.to_string(), "pr_id is required");
        let e = RepoprError::CreateApi {
            status: 403,
            body: "denied".into(),
        };
        assert_eq!(e.to_string(), "azure devops create pr error 403: denied");
        let e = RepoprError::CompleteApi {
            status: 404,
            body: "missing".into(),
        };
        assert_eq!(e.to_string(), "azure devops complete pr error 404: missing");
    }

    #[test]
    fn error_body_caps_long_responses() {
        let long = vec![b'a'; MAX_ERROR_BODY + 10];
        let text = error_body(&long);
        assert!(text.len() <= MAX_ERROR_BODY + "…(truncated)".len());
        assert!(text.ends_with("…(truncated)"));
        assert_eq!(error_body(b"short"), "short");
    }
}
