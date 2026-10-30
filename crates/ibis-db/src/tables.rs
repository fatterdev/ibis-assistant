//! Typed row structs per table.
//!
//! Field names are the Surreal column names; numeric fields go through the
//! quirk-tolerant [`crate::value::F64`] / [`crate::value::I64`] codecs (Q1) and
//! embeddings through [`crate::value::Embedding`] (Q12). `Option<…>` fields
//! accept both absent keys and explicit `NONE` (Q2).
//!
//! `source_file` is **schemaless** on the server (DDL defines indexes only), so
//! its field list follows what the writers actually persist (code ingest writes
//! the full shape; git ingest writes path-only rows) — all its fields are optional.

use crate::value::{Embedding, F64, I64};
use surrealdb::types::{Datetime, RecordId, SurrealValue};

/// `memory.kind` (DDL ASSERT domain, default `fact`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SurrealValue, serde::Serialize, serde::Deserialize,
)]
#[surreal(untagged, rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Decision,
    Fix,
    Constraint,
    Rule,
    Gotcha,
    Design,
    Fact,
    WorkingState,
    Observation,
    Reasoning,
}

/// `memory.status` (default `current`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SurrealValue, serde::Serialize, serde::Deserialize,
)]
#[surreal(untagged, rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Current,
    Superseded,
    Archived,
    Stale,
}

/// `memory.visibility`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SurrealValue, serde::Serialize, serde::Deserialize,
)]
#[surreal(untagged, rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Team,
    Personal,
}

/// `memory.store` — source-of-truth marker (git claims vs surreal rows).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SurrealValue, serde::Serialize, serde::Deserialize,
)]
#[surreal(untagged, rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Store {
    Git,
    Surreal,
}

/// `batch_status` domain shared by `commit`, `file_chunk`, `commit_chunk`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, SurrealValue, serde::Serialize, serde::Deserialize,
)]
#[surreal(untagged, rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Pending,
    Completed,
    Failed,
}

/// `memory.provenance` — the object AND every member are `option<…>`; legacy
/// rows may lack the object entirely and writers may set only some members
/// (roundtrip fixtures cover absent and partial shapes).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub agent: Option<String>,
    pub session: Option<String>,
    pub tool: Option<String>,
    pub imported_from: Option<String>,
    pub source_hash: Option<String>,
    pub source_path: Option<String>,
    pub introduced_in_commit: Option<RecordId>,
    pub invalidated_at_commit: Option<RecordId>,
    pub dirty: Option<bool>,
    pub worktree_hash: Option<String>,
    pub promote_on_commit: Option<bool>,
    pub orphan_oid: Option<String>,
}

/// The `memory` row (the critical row).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct MemoryRow {
    pub id: RecordId,
    pub kind: MemoryKind,
    pub title: Option<String>,
    pub content: String,
    pub status: MemoryStatus,
    pub confidence: Option<F64>,
    pub visibility: Visibility,
    pub owner: Option<String>,
    pub tags: Option<Vec<String>>,
    pub branch: Option<String>,
    pub project: Option<RecordId>,
    pub valid_from: Option<Datetime>,
    pub valid_until: Option<Datetime>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
    pub store: Option<Store>,
    pub policy_applied: Option<String>,
    /// Export dedup marker (surfaced by `SELECT provenance.source_hash AS source_hash`).
    pub source_hash: Option<String>,
    /// Query-time score — never persisted, decoded when projected by search.
    pub score: Option<F64>,
    pub introduced_in_oid: Option<String>,
    pub invalidated_at_oid: Option<String>,
    pub provenance: Option<Provenance>,
    pub embedding: Option<Embedding>,
}

/// The `commit` row (`hash` UNIQUE; `repo` links `record<repo>`; `date` is a
/// typed datetime — D17 R7).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct CommitRow {
    pub id: RecordId,
    pub hash: String,
    pub date: Option<Datetime>,
    pub message: Option<String>,
    pub repo: Option<RecordId>,
    pub batch_status: Option<BatchStatus>,
    pub batch_error: Option<String>,
}

/// The `source_file` row — **schemaless**: field list follows the writers
/// (code ingest: full shape; git ingest: path-only rows; ID
/// `<repo>_<relpath>_<sha256>`).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct SourceFileRow {
    pub id: RecordId,
    pub path: Option<String>,
    pub rel_path: Option<String>,
    pub hash: Option<String>,
    pub is_head: Option<bool>,
    /// Written by code ingest (there is no `repo` record field on this table).
    pub repo_name: Option<String>,
    pub updated_at: Option<Datetime>,
}

/// The `file_chunk` row (`file` links `record<source_file>`; HNSW `vector_embedding`).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct FileChunkRow {
    pub id: RecordId,
    pub file: Option<RecordId>,
    pub hash: Option<String>,
    pub content: Option<String>,
    pub symbol_name: Option<String>,
    pub kind: Option<String>,
    pub start_line: Option<I64>,
    pub end_line: Option<I64>,
    pub embedding: Option<Embedding>,
    pub batch_status: Option<BatchStatus>,
    pub batch_id: Option<String>,
}

/// The `commit_chunk` row (`commit_hash` links the commit; `commit_date` string).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct CommitChunkRow {
    pub id: RecordId,
    pub content: Option<String>,
    pub embedding: Option<Embedding>,
    pub batch_status: Option<BatchStatus>,
    pub batch_id: Option<String>,
    pub commit_hash: Option<String>,
    pub commit_date: Option<String>,
}

/// The `error_type` row (category+stack SHA-256 key; `hidden` marks superseded
/// templates; embeddings behind `error_embedding`; `created_at` typed datetime).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct ErrorTypeRow {
    pub id: RecordId,
    pub hash: Option<String>,
    pub category: Option<String>,
    pub stack_trace: Option<String>,
    pub severity: Option<I64>,
    pub template: Option<String>,
    pub created_at: Option<Datetime>,
    pub hidden: Option<bool>,
    pub embedding: Option<Embedding>,
}

/// The `log_file` row (`offset` is the tail-resume cursor; `project` links
/// `record<project>` via the `has_log` edge).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct LogFileRow {
    pub id: RecordId,
    pub path: Option<String>,
    pub project: Option<RecordId>,
    pub offset: Option<I64>,
}

/// The `log_processed_file` row (content SHA-256 UNIQUE, remote-poll dedup;
/// `processed_at` typed datetime).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct LogProcessedFileRow {
    pub id: RecordId,
    pub path: Option<String>,
    pub hash: Option<String>,
    pub processed_at: Option<Datetime>,
}

/// The `issue` row (provider-agnostic ticketing; `external_key` UNIQUE per
/// provider; numeric redmine record IDs stay unwrapped, Q5).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct IssueRow {
    pub id: RecordId,
    pub provider: Option<String>,
    pub external_id: Option<String>,
    pub external_key: Option<String>,
    pub project_key: Option<String>,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    #[surreal(rename = "type")]
    #[serde(rename = "type")]
    pub issue_type: Option<String>,
    pub updated_on: Option<String>,
    pub url: Option<String>,
}

/// The `git_credential` row (`target` UNIQUE; whole-content upsert via `$cred`).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct GitCredentialRow {
    pub id: RecordId,
    pub target: Option<String>,
    pub provider: Option<String>,
    pub auth_type: Option<String>,
    pub token: Option<String>,
    pub username: Option<String>,
    pub ssh_private_key: Option<String>,
}

/// The `key_usage` row — per-key daily counters (`id = key_usage:<hash8>_<yyyymmdd>`).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct KeyUsageRow {
    pub id: RecordId,
    pub requests: Option<I64>,
    pub owner: Option<String>,
    pub date: Option<Datetime>,
    pub last_updated: Option<Datetime>,
}

/// The `workstream` row (`status = 'active'` feeds session hints).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct WorkstreamRow {
    pub id: RecordId,
    pub name: Option<String>,
    pub status: Option<String>,
}

/// The `migration` row — only `migration:current` exists (see [`crate::migrate`]).
#[derive(Debug, Clone, PartialEq, SurrealValue, serde::Serialize, serde::Deserialize)]
pub struct MigrationRow {
    pub id: RecordId,
    pub version: Option<I64>,
    pub applied_at: Option<Datetime>,
}
