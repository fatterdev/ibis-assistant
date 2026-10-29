//! threski-ingest-redmine — legacy Redmine HTTP client + on-demand issue
//! graph ingest.
//!
//! The client is consumed today by the wave-3 ticketing lane (
//! `ticketing/providers/redmine` wraps it); [`Client::ingest_issue`] is the
//! on-demand graph updater the git history ingest triggers through the
//! [`Ingester`] boundary.
//!
//! Parity notes / deltas applied:
//! - **Bound variables** for every graph write (F-002 — the `IngestIssue
//!   already bound; kept) and **`UPSERT` instead of `UPDATE`** for the
//!   tracker/author/issue rows (E-A30: `UPDATE` does not create
//!   missing records on SurrealDB 3.x), wrapped in one transaction (E-A12).
//! - **Explicit API-key override parameter** replaces the original context-value
//!   lookup (`auth.RedmineKeyContextKey`) — the precedence
//!   `override → client config` is preserved and visible in the signatures
//!   (E-A13).
//! - **Error bodies are capped** (64 KiB) before inclusion in error messages
//!   (E-A11).
//! - Date validation accepts the exact layouts: strict `YYYY-MM-DD`
//!   (zero-padded, calendar-validated) or RFC3339 (E-A14).

pub mod client;
pub mod error;
pub mod ingest;
pub mod types;

pub use client::Client;
pub use error::RedmineError;
pub use ingest::{IngestIssueError, Ingester};
pub use types::{
    Issue, NamedObj, SearchIssuesParams, SearchIssuesResult, UpdateIssueParams, User, UsersResponse,
};

#[cfg(test)]
pub(crate) mod test_support {
    /// Binds a local axum server on a random loopback port and returns its
    /// base URL (same pattern as the threski-ai test suite).
    pub async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }
}
