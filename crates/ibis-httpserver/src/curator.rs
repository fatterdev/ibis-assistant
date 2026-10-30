//! F0.7 — the MCP App "Ibis Assistant Memory Curator" asset
//! ([`CURATOR_URI`]) and its metadata shapes.
//!
//! The app is a single self-contained HTML document (SEP-1865,
//! `io.modelcontextprotocol/ui` extension) embedded at compile time and
//! served by `ibis-server` as the `ui://ibis/memory-curator` resource
//! with MIME `text/html;profile=mcp-app`. The document speaks the MCP Apps
//! postMessage bridge directly (the `@modelcontextprotocol/ext-apps` wire
//! contract), carries no external references and pins its own CSP to
//! loopback-only (`CURATOR_CSP`); the same loopback policy is mirrored in
//! the `_meta.ui.csp` resource metadata ([`ui_resource_meta`]) so
//! spec-compliant hosts enforce it in the sandbox as well.
//!
//! Degradation contract: hosts without MCP Apps support never render the
//! resource and rely on the memory tools' always-present textual output
//! (the `_meta.ui.resourceUri` linkage on the tool objects and results is
//! additive metadata those hosts ignore).

use rmcp::model::MetaObject;
use serde_json::json;

/// The MCP Apps resource URI for the Memory Curator app.
pub const CURATOR_URI: &str = "ui://ibis/memory-curator";

/// SEP-1865 UI resource MIME type.
pub const CURATOR_MIME: &str = "text/html;profile=mcp-app";

/// Display name of the app (resource name/title).
pub const CURATOR_NAME: &str = "Ibis Assistant Memory Curator";

/// Short description surfaced on `resources/list`.
pub const CURATOR_DESCRIPTION: &str = "Interactive view over the typed project memory: search, \
     expand and follow the knowledge graph behind the memory tools";

/// The document's embedded Content-Security-Policy — loopback-only
/// connect-src, no external origins anywhere (the app talks to the host
/// through the postMessage bridge, never to the network). The HTML asset
/// carries this verbatim; [`curator_html`] tests keep the two in lockstep.
pub const CURATOR_CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; \
     style-src 'unsafe-inline'; img-src data:; font-src 'none'; \
     connect-src http://localhost:* http://127.0.0.1:* http://[::1]:*; \
     frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

/// Loopback origins declared as `_meta.ui.csp.connectDomains` (mirrors
/// [`CURATOR_CSP`]; empty `resourceDomains`/`frameDomains` keep the
/// spec's restrictive defaults).
pub const CURATOR_CONNECT_DOMAINS: &[&str] =
    &["http://localhost:*", "http://127.0.0.1:*", "http://[::1]:*"];

/// The embedded Memory Curator app document.
pub fn curator_html() -> &'static str {
    include_str!("../assets/memory-curator.html")
}

/// `_meta` for the UI resource (declaration and contents): the loopback CSP
/// policy plus a visible border preference.
pub fn ui_resource_meta() -> MetaObject {
    let value = json!({
        "ui": {
            "csp": {
                "connectDomains": CURATOR_CONNECT_DOMAINS,
                "resourceDomains": [],
                "frameDomains": [],
                "baseUriDomains": [],
            },
            "prefersBorder": true,
        }
    });
    MetaObject::from(
        value
            .as_object()
            .expect("ui resource meta is an object")
            .clone(),
    )
}

/// `_meta.ui.resourceUri` linking a tool (definition or result) to the
/// Memory Curator app.
pub fn ui_link_meta() -> MetaObject {
    let value = json!({ "ui": { "resourceUri": CURATOR_URI } });
    MetaObject::from(
        value
            .as_object()
            .expect("ui link meta is an object")
            .clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asset is a complete HTML5 document carrying the canonical CSP
    /// and the MCP Apps bridge surface.
    #[test]
    fn curator_asset_is_a_complete_mcp_app_document() {
        let html = curator_html();
        assert!(html.starts_with("<!DOCTYPE html>"), "doctype first: {html}");
        assert!(html.contains("</html>"), "document is closed");
        assert!(
            html.contains("<meta http-equiv=\"Content-Security-Policy\""),
            "CSP meta present"
        );
        assert!(
            html.contains(CURATOR_CSP),
            "asset CSP must match the canonical CURATOR_CSP const"
        );
        // The bridge implements the SEP-1865 methods the app relies on.
        for method in [
            "ui/initialize",
            "ui/notifications/initialized",
            "ui/notifications/tool-input",
            "ui/notifications/tool-result",
            "ui/notifications/size-changed",
            "tools/call",
        ] {
            assert!(html.contains(method), "bridge method {method} present");
        }
        // Textual degradation lives inside the document too.
        assert!(
            html.contains("id=\"fallback\""),
            "static fallback panel present"
        );
        assert!(
            html.contains("Ibis Assistant Memory Curator"),
            "app names itself"
        );
    }

    /// The document's own CSP is loopback-only: every declared network
    /// origin is localhost/127.0.0.1/[::1], and no external reference of
    /// any kind is reachable.
    #[test]
    fn curator_csp_pins_loopback_only() {
        let csp = CURATOR_CSP;
        assert!(
            csp.contains("default-src 'none'"),
            "restrictive default: {csp}"
        );
        let connect = csp
            .split(';')
            .find(|d| d.trim().starts_with("connect-src"))
            .expect("connect-src directive present");
        let origins: Vec<&str> = connect
            .trim()
            .strip_prefix("connect-src")
            .expect("prefix")
            .split_whitespace()
            .collect();
        assert_eq!(
            origins, CURATOR_CONNECT_DOMAINS,
            "connect domains match the meta"
        );
        for origin in origins {
            let host = origin
                .strip_prefix("http://")
                .unwrap_or_else(|| panic!("loopback origin {origin} is plain http"));
            assert!(
                host == "localhost:*"
                    || host.starts_with("127.0.0.1:")
                    || host.starts_with("[::1]:"),
                "non-loopback origin leaked into the CSP: {origin}"
            );
        }
        let html = curator_html();
        for needle in [
            "<script src=",
            "src=\"http",
            "src='http",
            "url(http",
            "@import",
            "https://",
        ] {
            assert!(
                !html.contains(needle),
                "external reference leaked into the app document: {needle}"
            );
        }
    }

    /// Resource metadata shape: `_meta.ui.csp` mirrors the document CSP and
    /// the border preference is explicit.
    #[test]
    fn ui_resource_meta_declares_loopback_csp() {
        let meta = ui_resource_meta();
        let ui = meta.get("ui").expect("ui meta object");
        let csp = ui.get("csp").expect("csp object");
        let connect = csp
            .get("connectDomains")
            .and_then(|v| v.as_array())
            .expect("connectDomains array");
        let domains: Vec<&str> = connect
            .iter()
            .map(|v| v.as_str().expect("string domain"))
            .collect();
        assert_eq!(domains, CURATOR_CONNECT_DOMAINS);
        for (key, expected) in [
            ("resourceDomains", 0),
            ("frameDomains", 0),
            ("baseUriDomains", 0),
        ] {
            let list = csp.get(key).and_then(|v| v.as_array()).expect(key);
            assert_eq!(list.len(), expected, "{key} keeps the restrictive default");
        }
        assert_eq!(
            ui.get("prefersBorder").and_then(|v| v.as_bool()),
            Some(true),
            "explicit border preference"
        );
    }

    /// `_meta.ui.resourceUri` points at the app.
    #[test]
    fn ui_link_meta_points_at_the_curator() {
        let meta = ui_link_meta();
        let uri = meta
            .get("ui")
            .and_then(|ui| ui.get("resourceUri"))
            .and_then(|v| v.as_str())
            .expect("ui.resourceUri present");
        assert_eq!(uri, CURATOR_URI);
    }

    /// The wire constants follow SEP-1865 exactly.
    #[test]
    fn constants_follow_the_mcp_apps_spec() {
        assert!(CURATOR_URI.starts_with("ui://"), "ui:// scheme required");
        assert_eq!(CURATOR_MIME, "text/html;profile=mcp-app");
        assert_eq!(CURATOR_CONNECT_DOMAINS.len(), 3, "three loopback origins");
    }
}
