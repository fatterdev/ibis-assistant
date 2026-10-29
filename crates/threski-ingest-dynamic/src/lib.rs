//! threski-ingest-dynamic — server-mode owned clones + the per-project
//! ingestion queue.
//!
//! - [`workspace::sync_workspace`](crate::workspace::sync_workspace): resolves
//!   the workspace (live tree in personal/plugin — read-only HEAD probe; owned
//!   clone under `discovery_root/dynamic/<name>` in server mode), resolves git
//!   credentials (request token → DB by URL/host → legacy config token), then
//!   clones (missing) or clean/reset/fetch (existing) and checks out the
//!   requested commit/branch, reporting whether the tree is aligned.
//! - [`queue::ProjectIngestionManager`](crate::queue::ProjectIngestionManager): one worker per project
//!   over a bounded (100) channel; enqueues never block; the ingest phase runs
//!   under a 2h timeout; `Stop` cancels and joins.
//!
//! Reuse notes (audit-before-port):
//! - Host parsing, provider detection, auth-failure detection and the whole
//!   credential-resolution order are lane D's `threski_git` primitives
//!   (`get_host`, `detect_provider`, `is_auth_error`, `resolve_credential`) —
//!   nothing duplicated here (finding D-A15, E-A15).
//! - The owned-clone git command shapes are `threski_git::sync` primitives
//!   (`clone_repo`, `clean_untracked`, `reset_hard`, `ensure_remote_origin`,
//!   `fetch_origin`, `checkout`) routed through the credential-injecting
//!   `GitRunner` (D-A07).
//! - `git_credential` reads go through `threski_git::CredentialStore` bridged to
//!   the typed `threski_db::Executor` by [`db_bridge`] (decode via typed
//!   `SurrealValue` rows; native-shape bound values for writes).

pub mod db_bridge;
pub mod error;
pub mod queue;
pub mod workspace;

pub use db_bridge::ExecutorCredentialDb;
pub use error::{DynamicError, PROCESS_JOB_TIMEOUT};
pub use queue::{
    IngestionJob, IngestionResult, OnComplete, OnSyncDone, ProcessJobFn, ProjectIngestionManager,
    JOB_CHANNEL_CAP,
};
pub use workspace::{sync_workspace, SyncOutcome};
