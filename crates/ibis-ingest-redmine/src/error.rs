//! Typed errors for the Redmine client ( returned `fmt.Errorf` strings).

use thiserror::Error;

/// Maximum bytes of an error response body included in error messages
/// (E-A11: read the body unbounded).
pub const MAX_ERROR_BODY: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum RedmineError {
    #[error("redmine request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("redmine error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("redmine search error {status}: {body}")]
    SearchApi { status: u16, body: String },
    #[error("redmine update error {status}: {body}")]
    UpdateApi { status: u16, body: String },
    #[error("failed to decode redmine response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("no update fields provided")]
    NoUpdateFields,
    #[error("{0}")]
    Invalid(String),
}

/// Capped, lossy rendering of a non-2xx response body.
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
    fn messages_match_reference_strings() {
        let e = RedmineError::Api {
            status: 500,
            body: "boom".into(),
        };
        assert_eq!(e.to_string(), "redmine error 500: boom");
        let e = RedmineError::SearchApi {
            status: 403,
            body: "forbidden".into(),
        };
        assert_eq!(e.to_string(), "redmine search error 403: forbidden");
        let e = RedmineError::UpdateApi {
            status: 422,
            body: "bad".into(),
        };
        assert_eq!(e.to_string(), "redmine update error 422: bad");
    }

    #[test]
    fn error_body_caps_long_responses() {
        let long = vec![b'a'; MAX_ERROR_BODY + 10];
        let text = error_body(&long);
        assert!(text.len() <= MAX_ERROR_BODY + "...(truncated)".len());
        assert!(text.ends_with("…(truncated)"));
        assert_eq!(error_body(b"short"), "short");
    }
}
