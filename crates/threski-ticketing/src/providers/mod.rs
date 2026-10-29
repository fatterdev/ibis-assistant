//! Per-provider clients.

pub mod azuredevops;
pub mod jira;
pub mod redmine;

/// Shared HTTP plumbing for the raw provider calls (the legacy Redmine
/// client has its own — E-A11/E-A13 patterns mirrored here).
pub(crate) mod net {
    /// `http.Client{Timeout: 30 * time.Second} (the legacy Redmine client
    /// default; jira/ado used a timeout-less client originally — TI-12
    /// adds the same 30s bound here).
    pub const DEFAULT_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// Maximum bytes of an error response body included in error messages
    /// (E-A11 pattern: read bodies unbounded).
    pub const MAX_ERROR_BODY: usize = 64 * 1024;

    /// Capped, lossy rendering of a non-2xx response body.
    pub fn error_body(bytes: &[u8]) -> String {
        let capped = &bytes[..bytes.len().min(MAX_ERROR_BODY)];
        let mut text = String::from_utf8_lossy(capped).into_owned();
        if bytes.len() > MAX_ERROR_BODY {
            text.push_str("…(truncated)");
        }
        text
    }

    /// Percent-encodes a URL path segment. Strictly
    /// encodes everything outside the RFC 3986 unreserved set — more
    /// conservative than the `PathEscape` (which leaves `:@&=+$,;` raw
    /// inside segments); servers decode both identically (TI-13).
    pub fn path_escape(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    pub fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(DEFAULT_HTTP_TIMEOUT)
            .build()
            .expect("reqwest client")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn path_escape_encodes_separators_and_spaces() {
            assert_eq!(path_escape("ABC-100"), "ABC-100");
            assert_eq!(path_escape("a/b c"), "a%2Fb%20c");
            assert_eq!(path_escape("a%b"), "a%25b");
            assert_eq!(path_escape("1.2_3~x-y"), "1.2_3~x-y");
        }

        #[test]
        fn error_body_caps_long_responses() {
            let long = vec![b'a'; MAX_ERROR_BODY + 10];
            let text = error_body(&long);
            assert!(text.ends_with("…(truncated)"));
            assert_eq!(error_body(b"short"), "short");
        }
    }
}
