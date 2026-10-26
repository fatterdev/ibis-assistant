//! ibis-ingest-git — git history ingest.
//!
//! Walks the full commit history of a repository into the knowledge graph:
//! commits, authors, `parent_of`/`authored` edges, per-file `changed` edges
//! with impact, `coupled_with` co-change edges, and ticket references parsed
//! from commit subjects (fed to an [`ibis_ticketing::Ingester`] worker pool or
//! written as bare `issue` rows).
//!
//! Parity notes / deltas applied:
//! - **All git access routes through `GitRunner`** (D-A07): the two streaming
//!   `git log` spawns of the original implementation are served by
//!   `ibis_git::sync::{history_oids, ingest_log_batch}` — one `--no-walk
//!   --stdin --numstat` invocation per 500-hash batch (E-A07: the
//!   1000-op transaction accumulator persists across batches, so DB batching
//!   semantics are unchanged; the trade-off is `N/500` process spawns instead
//!   of two long-running processes).
//! - **Bound variables everywhere** (F-002, E-A01): every
//!   content-bearing statement (author names, subjects, dates, paths) is bound
//!   via `Vars`; only record identifiers (sanitized/hex) are inlined.
//! - **`UPSERT` instead of `UPDATE`** for node rows (E-A30): on
//!   SurrealDB 3.x `UPDATE` does not create missing records — the
//!   statements silently no-op'd every new commit/author/file on the spec
//!   baseline stack (live-verified on the 3.3.4 sidecar).
//! - **Record-bound `id IN $ids`** for the existing-commit check (
//!   E-A31): string-form ids never match record comparisons on SurrealDB 3.x,
//!   so the original dedupe never fired and every ingest re-walked all commits.
//! - Dead `skipping` flag and dead log10 impact term dropped (E-A02/E-A03).
//! - Only consumed lines count toward the 1000-op flush threshold — 's
//!   `continue` paths skip `batchCount++` for empty/malformed lines (E-A17).
//! - benchmarks pin the
//!   `SELECT VALUE id` pre-load path performance — that path is the
//!   implementation here; the scenarios are recorded, omitted as bench
//!   targets.
//!
//! Cancellation: dropping the future aborts in-flight git children
//! (`kill_on_drop`) and closes the ticket fan-out channel, mirroring the
//! `context.Context` behavior.

pub mod error;
pub mod ingest;
pub mod issue_parser;

pub use error::IngestError;
pub use ingest::{ingest_repo, process_git_log_lines, HASH_BATCH_SIZE, JOB_CHANNEL_CAP, TXN_OPS};
pub use issue_parser::extract_issue_refs;
