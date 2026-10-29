//! threski-db — SurrealDB layer for Threski Studio.
//!
//! Architecture:
//! - [`executor`]: the `Executor` trait (`execute` / `smart_query`) + retry policy.
//! - [`value`]: the SurrealValue codec boundary — the one place raw responses become
//!   typed values (quirks Q1–Q12 from encoded exactly once).
//! - [`ids`]: record-ID formatting/sanitization/escaping helpers.
//! - [`schema`]: final-state DDL (migrations 1–5 folded in per D16).
//! - [`migrate`]: migration framework starting empty (version record + UPSERT-only).
//! - [`process`]: binary install/upgrade + embedded SurrealDB process lifecycle.
//! - [`tables`]: typed row structs per table, consumed by the search/ingest lanes.

pub mod error;
pub mod executor;
pub mod ids;
pub mod migrate;
pub mod process;
pub mod schema;
pub mod tables;
pub mod value;

pub use error::{DbError, ProcessError};
pub use executor::{Executor, QueryOutcome, SurrealDbExecutor, Vars};
