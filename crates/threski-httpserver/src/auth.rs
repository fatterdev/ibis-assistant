//! HTTP auth: loopback-trust actor + provider credentials —
//! on top of `threski_core::auth` (the loopback-trust
//! semantics live there; this module only extracts headers into the request
//! context).
//!
//! `X-Threski-Actor` is trusted **only from loopback peers**; remote peers are
//! marked remote-untrusted so personal-memory authorization stays team-only
//! Credential headers
//! (`X-Redmine-API-Key`, `X-Jira-Email` + `X-Jira-API-Token`,
//! `X-Azure-DevOps-PAT/-Org/-Project/-Repo`, `X-Git-Token`/`X-Git-PAT`) are
//! extracted from any peer.
//!
//! Rust replaces the context values with an explicit
//! [`threski_core::auth::Credentials`] value stored in the http request
//! `Extensions` (and forwarded to MCP handlers inside
//! `http::request::Parts` by rmcp).

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use threski_core::auth::Credentials;
use std::net::SocketAddr;

/// Builds the credentials context from the peer address and HTTP headers
/// The peer address must be `host:port` (or `[v6]:port`)
/// as produced by the accept loop; `X-Forwarded-For` is deliberately ignored
/// so proxies cannot mint a trusted actor.
pub fn inject_auth(peer_addr: &str, headers: &HeaderMap) -> Credentials {
    let mut creds = Credentials::default();
    if threski_core::auth::is_loopback_peer(peer_addr) {
        creds.actor = trim_header(headers, "x-threski-actor");
    } else {
        creds.remote_untrusted = true;
    }
    creds.redmine_key = trim_header(headers, "x-redmine-api-key");
    creds.jira_email = trim_header(headers, "x-jira-email");
    creds.jira_api_token = trim_header(headers, "x-jira-api-token");
    creds.azure_devops_pat = trim_header(headers, "x-azure-devops-pat");
    creds.azure_devops_org = trim_header(headers, "x-azure-devops-org");
    creds.azure_devops_project = trim_header(headers, "x-azure-devops-project");
    creds.azure_devops_repo = trim_header(headers, "x-azure-devops-repo");
    // X-Git-Token wins; X-Git-PAT is the fallback
    creds.git_token = match trim_header(headers, "x-git-token") {
        Some(token) => Some(token),
        None => trim_header(headers, "x-git-pat"),
    };
    creds
}

fn trim_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Returns the injected HTTP actor, then the explicit fallback, then
/// `"unknown"`.
pub fn resolve_actor(creds: &Credentials, explicit: &str) -> String {
    threski_core::auth::resolve_actor(creds.actor.as_deref(), explicit)
}

/// Returns an identity allowed to authorize personal memories (
/// `httpserver.ResolveTrustedActor`): remote peers always get `""`.
pub fn resolve_trusted_actor(creds: &Credentials, explicit: &str) -> String {
    threski_core::auth::resolve_trusted_actor(creds.actor.as_deref(), explicit, creds.remote_untrusted)
}

/// Reports whether the direct peer address is loopback (
/// `httpserver.IsLoopbackRequest`).
pub fn is_loopback_request(peer_addr: &str) -> bool {
    threski_core::auth::is_loopback_peer(peer_addr)
}

/// Axum middleware that runs `inject_auth` for every route (`Auth:
/// wraps the whole mux). The peer address comes from the
/// `ConnectInfo<SocketAddr>` extension inserted by the accept loop
/// ([`crate::mux::serve`]); without it (oneshot test clients) the peer is
/// treated as remote, mirroring the empty `RemoteAddr` being non-loopback.
pub async fn auth_middleware(req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.to_string())
        .unwrap_or_default();
    let creds = inject_auth(&peer, req.headers());
    let mut req = req;
    req.extensions_mut().insert(creds);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    /// Scenario: TestResolveActorPrefersHeaderOverExplicitActorOnLoopback.
    #[test]
    fn header_actor_wins_on_loopback() {
        let creds = inject_auth("127.0.0.1:54321", &headers(&[("X-Threski-Actor", " alice ")]));
        assert_eq!(creds.actor.as_deref(), Some("alice"));
        assert_eq!(resolve_actor(&creds, "bob"), "alice");
        assert!(!creds.remote_untrusted);
        assert_eq!(resolve_trusted_actor(&creds, "bob"), "alice");
    }

    /// Scenario: TestInjectAuthIgnoresActorHeaderOnRemotePeer.
    #[test]
    fn remote_peer_is_untrusted() {
        let creds = inject_auth("203.0.113.10:443", &headers(&[("X-Threski-Actor", "alice")]));
        assert_eq!(creds.actor, None, "remote peer must not inject actor");
        assert!(creds.remote_untrusted, "expected remote-untrusted marker");
        assert_eq!(
            resolve_trusted_actor(&creds, "alice"),
            "",
            "remote claimed actor must not authorize personal memory"
        );
        assert_eq!(
            resolve_actor(&creds, "alice"),
            "alice",
            "attribution fallback should still see explicit actor"
        );
    }

    /// Scenario: TestResolveActorFallsBackToExplicitActorThenUnknown.
    #[test]
    fn actor_fallback_chain() {
        let creds = Credentials::default();
        assert_eq!(resolve_actor(&creds, " bob "), "bob");
        assert_eq!(resolve_actor(&creds, " "), "unknown");
        assert_eq!(resolve_trusted_actor(&creds, " "), "");
    }

    #[test]
    fn credential_headers_extracted_from_any_peer() {
        let creds = inject_auth(
            "10.1.2.3:8080",
            &headers(&[
                ("X-Redmine-API-Key", " rk "),
                ("X-Jira-Email", " a@b.c "),
                ("X-Jira-API-Token", " jt "),
                ("X-Azure-DevOps-PAT", " pat "),
                ("X-Azure-DevOps-Org", " org "),
                ("X-Azure-DevOps-Project", " proj "),
                ("X-Azure-DevOps-Repo", " repo "),
                ("X-Git-PAT", " gpat "),
            ]),
        );
        assert_eq!(creds.redmine_key.as_deref(), Some("rk"));
        assert_eq!(creds.jira_email.as_deref(), Some("a@b.c"));
        assert_eq!(creds.jira_api_token.as_deref(), Some("jt"));
        assert_eq!(creds.azure_devops_pat.as_deref(), Some("pat"));
        assert_eq!(creds.azure_devops_org.as_deref(), Some("org"));
        assert_eq!(creds.azure_devops_project.as_deref(), Some("proj"));
        assert_eq!(creds.azure_devops_repo.as_deref(), Some("repo"));
        // X-Git-Token preferred over X-Git-PAT.
        assert_eq!(creds.git_token.as_deref(), Some("gpat"));
        assert!(creds.remote_untrusted);
    }

    #[test]
    fn git_token_takes_precedence_over_pat() {
        let creds = inject_auth(
            "127.0.0.1:9",
            &headers(&[("X-Git-Token", "tok"), ("X-Git-PAT", "pat")]),
        );
        assert_eq!(creds.git_token.as_deref(), Some("tok"));
        // Empty values are dropped (`if trimmed != ""`).
        let empty = inject_auth("127.0.0.1:9", &headers(&[("X-Git-Token", "   ")]));
        assert_eq!(empty.git_token, None);
    }
}
