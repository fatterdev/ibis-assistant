//! threski-httpserver — the HTTP mux (
//! 392 LOC across , , ).
//!
//! Surface (parity unless noted):
//! - `GET /` — discovery, content-negotiated JSON / markdown / HTML
//!   ([`discovery`]), with the endpoints map **without** the `sse_legacy`
//!   entry (F-005/D11).
//! - `POST|GET|DELETE /mcp` and `/mcp/` — Streamable HTTP MCP via an injected
//!   rmcp [`rmcp::transport::streamable_http_server::tower::StreamableHttpService`]
//!   (30s SSE keep-alive heartbeat; credentials injected from HTTP headers
//!   into request extensions, visible to MCP handlers through
//!   `rmcp::service::RequestContext::extensions` → `http::request::Parts`).
//! - `POST /api/v1/logs/upload` — the log upload endpoint with the full
//!   405/403/401/413 contract ([`upload`]), synchronous analysis through the
//!   `threski-ingest-logs` analyzer seam.
//! - `/api/v1/settings` and `/settings/` — mount seams for the settings lane:
//!   the API `Router` is **merged** (no prefix strip, mirroring 's
//!   `mux.Handle("/api/v1/settings", settingsAPI)` which passed full paths),
//!   the UI `Router` is **nested** (mirroring 's
//!   `http.StripPrefix("/settings/", settingsUI)`).
//! - Origin gate (DNS-rebinding protection) on `/mcp` only — loopback browser
//!   origins or an exact concrete bind host ([`origin`]); X-Threski-Actor trusted
//!   only from loopback peers ([`auth`]).
//!
//! Deltas vs (D15/D11): the legacy `/sse` + `/message` routes, their
//! deprecation headers and the `endpoints.sse_legacy` discovery entry are
//! removed; the guide text no longer advertises SSE; install text speaks Rust
//! instead of the original toolchain.

pub mod auth;
pub mod discovery;
pub mod mux;
pub mod origin;
pub mod upload;

pub use auth::{auth_middleware, inject_auth, resolve_actor, resolve_trusted_actor};
pub use discovery::{
    guide_markdown, protocol_versions, DiscoveryState, ToolMeta, ToolsProvider, GUIDE_URI,
    SERVER_NAME, SERVER_VERSION,
};
pub use mux::{bind_host, handler, serve, streamable_http_config, HttpServeError};
pub use origin::origin_allowed;
pub use upload::{log_upload_handler, IngestAnalyzer, LogUploadAnalyzer};
