//! Identity/credential values and loopback-trust actor semantics
//! In Rust the request context is an explicit struct.

/// Values carried by authenticated HTTP requests (trusted only as specified:
/// X-Threski-Actor from loopback peers only; provider credentials from headers).
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub redmine_key: Option<String>,
    pub jira_email: Option<String>,
    pub jira_api_token: Option<String>,
    pub azure_devops_pat: Option<String>,
    pub azure_devops_org: Option<String>,
    pub azure_devops_project: Option<String>,
    pub azure_devops_repo: Option<String>,
    pub git_token: Option<String>,
    pub actor: Option<String>,
    /// Marks requests whose peer is not loopback: personal-memory
    /// authorization must ignore claimed identity on those requests.
    pub remote_untrusted: bool,
}

pub trait TokenProvider: Send + Sync {
    fn get_git_token(&self, origin_url: &str) -> String;
}

/// Reports whether the direct peer address is loopback. X-Forwarded-For is
/// ignored so proxies cannot mint a trusted actor.
pub fn is_loopback_peer(remote_addr: &str) -> bool {
    let host = remote_addr.trim();
    // Strip port (host:port), keeping bare IPv6 literals intact when unbracketed.
    let host = split_peer_host(host);
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

fn split_peer_host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
    }
    match addr.rfind(':') {
        Some(idx)
            if !addr[idx + 1..].is_empty()
                && addr[..idx]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ':') =>
        {
            &addr[..idx]
        }
        _ => addr,
    }
}

/// Returns the request actor, then the explicit fallback, then "unknown".
/// Use for attribution on writes. Prefer `resolve_trusted_actor` for personal
/// visibility and personal-memory governance.
pub fn resolve_actor(actor: Option<&str>, explicit: &str) -> String {
    if let Some(actor) = actor.map(str::trim).filter(|a| !a.is_empty()) {
        return actor.to_string();
    }
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    "unknown".to_string()
}

/// Returns an identity that may authorize personal memories. Remote
/// (non-loopback) HTTP peers always get "" (team-only). Local/stdio and
/// loopback HTTP still resolve header then explicit actor; bare "unknown"
/// becomes "" so anonymous callers never match personal rows.
///
/// Team-memory mutations that must preserve the anonymous-local owner
/// "unknown" should keep using `resolve_actor` instead.
pub fn resolve_trusted_actor(
    actor: Option<&str>,
    explicit: &str,
    remote_untrusted: bool,
) -> String {
    if remote_untrusted {
        return String::new();
    }
    let actor = resolve_actor(actor, explicit);
    if actor == "unknown" {
        String::new()
    } else {
        actor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_peers() {
        assert!(is_loopback_peer("127.0.0.1:52344"));
        assert!(is_loopback_peer("[::1]:8080"));
        assert!(is_loopback_peer("127.0.0.1"));
        assert!(!is_loopback_peer("192.168.1.10:1234"));
        assert!(!is_loopback_peer("example.com:80"));
        assert!(!is_loopback_peer(""));
    }

    #[test]
    fn actor_resolution() {
        assert_eq!(resolve_actor(Some("alice"), "bob"), "alice");
        assert_eq!(resolve_actor(Some("  "), "bob"), "bob");
        assert_eq!(resolve_actor(None, ""), "unknown");
    }

    #[test]
    fn trusted_actor_remote_untrusted() {
        assert_eq!(resolve_trusted_actor(Some("alice"), "", true), "");
        assert_eq!(resolve_trusted_actor(Some("alice"), "", false), "alice");
        // bare "unknown" becomes "" so anonymous callers never match personal rows
        assert_eq!(resolve_trusted_actor(None, "", false), "");
        assert_eq!(resolve_trusted_actor(None, "carol", false), "carol");
    }
}
