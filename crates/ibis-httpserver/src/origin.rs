//! Origin gate (DNS-rebinding protection).
//!
//! Missing/`null` Origin is allowed (CLI / many MCP clients); non-http(s)
//! schemes are allowed; browser http(s) origins pass only when the origin
//! host is loopback (`localhost`, `127.0.0.1`, `::1`) or **exactly equals a
//! concrete, non-wildcard bind host** — wildcard binds (`0.0.0.0`/`::`)
//! therefore allow only loopback browser origins.
//!
//! Parsing mirrors `url.Parse + `URL.Hostname` leniency: a value without a
//! `scheme://` separator parses as a relative reference (empty scheme →
//! allowed, same as ); a malformed authority (unbalanced `[`, non-numeric
//! port) fails the gate like a parse error.

/// Reports whether an HTTP request with this `Origin` header may pass.
pub fn origin_allowed(origin: &str, bind_host: &str) -> bool {
    let origin = origin.trim();
    if origin.is_empty() || origin.eq_ignore_ascii_case("null") {
        return true;
    }
    let Some((scheme, rest)) = split_scheme(origin) else {
        // No "scheme://" — the url.Parse yields an empty scheme, which is not
        // http/https, so the gate allows it.
        return true;
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return true;
    }
    let Ok(host) = origin_host(rest) else {
        // Malformed authority: mirrors a url.Parse error (gate fails closed).
        return false;
    };
    let host = host.to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    let bind_host = bind_host.trim().to_ascii_lowercase();
    if !bind_host.is_empty() && bind_host != "0.0.0.0" && bind_host != "::" && host == bind_host {
        return true;
    }
    false
}

/// Splits `scheme://rest` mirroring the RFC 3986 scheme grammar
/// (`[a-zA-Z][a-zA-Z0-9+.-]*`); `None` when the value has no valid scheme
/// separator (relative reference).
fn split_scheme(origin: &str) -> Option<(&str, &str)> {
    let idx = origin.find("://")?;
    let scheme = &origin[..idx];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }
    Some((scheme, &origin[idx + 3..]))
}

/// Extracts the hostname from an authority, mirroring `url.Parse (last
/// `@` starts the host) and `URL.Hostname`/`splitHostPort` (bracketed IPv6 →
/// inner literal; otherwise cut at the last `:`), with this crate validation
/// (`:` must be followed by digits or end the authority).
fn origin_host(authority: &str) -> Result<String, ()> {
    let end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    let mut authority = &authority[..end];
    if let Some(at) = authority.rfind('@') {
        authority = &authority[at + 1..];
    }
    if authority.starts_with('[') {
        let Some(close) = authority.find(']') else {
            // "missing ']' in host" → parse error.
            return Err(());
        };
        let host = &authority[1..close];
        let rest = &authority[close + 1..];
        // Port after the bracket must be empty or ":" + digits.
        if !rest.is_empty() && !valid_port(rest) {
            return Err(());
        }
        return Ok(host.to_string());
    }
    if let Some(colon) = authority.rfind(':') {
        if !valid_port(&authority[colon..]) {
            return Err(());
        }
        return Ok(authority[..colon].to_string());
    }
    Ok(authority.to_string())
}

/// `validOptionalPort: empty or `:` followed by ASCII digits.
fn valid_port(rest: &str) -> bool {
    let Some(port) = rest.strip_prefix(':') else {
        return false;
    };
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: TestOriginAllowed — same matrix.
    #[test]
    fn origin_gate_matrix() {
        let cases = [
            ("", "", true),
            ("null", "", true),
            ("http://localhost:1234", "", true),
            ("http://127.0.0.1", "", true),
            ("https://evil.example", "", false),
            ("http://10.0.0.5:3030", "10.0.0.5", true),
            ("vscode-file://vscode-app", "", true),
        ];
        for (origin, bind, ok) in cases {
            assert_eq!(
                origin_allowed(origin, bind),
                ok,
                "OriginAllowed({origin:?}, {bind:?})"
            );
        }
    }

    #[test]
    fn wildcard_binds_only_allow_loopback_browser_origins() {
        // 0.0.0.0/:: are excluded as bind-host matches
        assert!(!origin_allowed("http://10.0.0.5:3030", "0.0.0.0"));
        assert!(!origin_allowed("http://evil.example", "0.0.0.0"));
        assert!(origin_allowed("http://localhost:99", "0.0.0.0"));
        assert!(!origin_allowed("http://10.0.0.5", "::"));
        assert!(origin_allowed("http://127.0.0.1", "::"));
        // Null-bind host behaves like an unset one.
        assert!(!origin_allowed("http://evil.example", ""));
    }

    #[test]
    fn loopback_forms_and_case() {
        assert!(origin_allowed("HTTP://LOCALHOST:1", ""));
        assert!(origin_allowed("http://LOCALHOST", ""));
        assert!(origin_allowed("http://[::1]:3030", ""));
        assert!(origin_allowed("https://127.0.0.1:8443", ""));
        // A concrete ::1 bind host matches too.
        assert!(origin_allowed("http://[::1]:80", "::1"));
    }

    #[test]
    fn non_http_schemes_and_relative_values() {
        assert!(origin_allowed("chrome-extension://abc", ""));
        assert!(origin_allowed("vscode-file://vscode-app", ""));
        // No scheme:// → relative reference → allows (empty scheme).
        assert!(origin_allowed("weird", ""));
        assert!(origin_allowed("localhost:3030", ""));
    }

    #[test]
    fn malformed_authorities_fail_closed() {
        // Non-numeric port is a url.Parse error → 403.
        assert!(!origin_allowed("http://10.0.0.5:abc", ""));
        // Unbalanced bracket → parse error.
        assert!(!origin_allowed("http://[::1", ""));
        // Userinfo is stripped likewise; the host after the last @ counts.
        assert!(origin_allowed("http://user@localhost:3030", ""));
        assert!(!origin_allowed("http://user@evil.example", ""));
        // Port mismatch on the bind host is still an exact-host compare (no port).
        assert!(origin_allowed("http://10.0.0.5:9999", "10.0.0.5"));
    }
}
