//! Final-state database DDL, **SCHEMAFULL-typed**, written for a datastore
//! born on **SurrealDB 4.0** (WQ-7 greenfield: compatibility level 4 from the
//! first boot — no 3.x upgrade, no `MIGRATE EXISTING ROWS` here; every later
//! field change on a populated table must add one).
//!
//! Rules encoded here:
//! - every field any writer persists is `DEFINE FIELD IF NOT EXISTS`-declared;
//!   undeclared writes fail loudly instead of silently accepting typos;
//! - mandatory (non-`option`) only where **every** writer of the field always
//!   supplies it;
//! - domain unions (`batch_status`, `memory.kind/status/visibility`) are named
//!   with `DEFINE TYPE` — the union order is the sort order (4.0) — and the
//!   fields reference the type name; no `ASSERT $value IN [...]` remains;
//! - composite keys are **array record ids with named parts** (4.0):
//!   `source_file` is `[repo, file_path]` and `file_chunk` is
//!   `[file_id, line]`; conditions on the named parts become key-range scans
//!   and the secondary indexes that duplicated the key (`file_path`,
//!   `file_version`) are gone;
//! - semantic datetimes are `datetime`-typed; verbatim API strings stay
//!   `string`;
//! - embeddings are `array<f16>` (live-probed: DiskANN accepts the field and
//!   f32 vectors coerce on write) behind **DiskANN** indexes — the engine
//!   walks 8-bit quantised codes and reranks exactly, which absorbs B5;
//! - full-text search is a first-class channel: `FULLTEXT ANALYZER … BM25`
//!   on `memory.title`, `memory.content` and `file_chunk.content`, ranked by
//!   `search::score(1)`;
//! - every graph edge the read path crosses carries `REFERENCE` and
//!   `MATERIALIZE INTO` a `READONLY` back-reference list on each endpoint, so
//!   `recent_failures`, `why_changed`, blast radius and dead code read the
//!   list on the record instead of scanning the edge table. Edges with a
//!   union-typed endpoint cannot materialize (the engine requires the field
//!   to reference exactly one table): they keep the traversal on that side
//!   (`contains`, `authored`/`related_to`/`derived_from` out sides);
//! - tables with no writer yet (`batch_job`, `error_state`, `community`,
//!   `process`, `branch`) are declared without fields: their future lanes add
//!   their own `DEFINE FIELD` lines.
//!
//! Everything is born fresh; statements are string literals (identifiers
//! only — no content interpolation, F-002) and may be re-run safely
//! (`IF NOT EXISTS` everywhere, LB-07).
//!
//! Materialization requires declaration order: the `READONLY` list fields are
//! defined together with their table, **before** the edge statements that
//! fill them.

use crate::error::DbError;
use crate::executor::Executor;

/// Node and edge table names (`internal/schema constants).
pub mod names {
    // Nodes
    pub const REPO: &str = "repo";
    pub const BRANCH: &str = "branch";
    pub const COMMIT: &str = "commit";
    pub const AUTHOR: &str = "author";
    pub const SOURCE_FILE: &str = "source_file";
    pub const FILE_CHUNK: &str = "file_chunk";
    pub const COMMIT_CHUNK: &str = "commit_chunk";
    pub const ISSUE: &str = "issue";
    pub const TRACKER: &str = "tracker";
    pub const BATCH_JOB: &str = "batch_job";
    pub const PROJECT: &str = "project";
    pub const LOG_FILE: &str = "log_file";
    pub const LOG_ENTRY: &str = "log_entry";
    pub const ERROR_TYPE: &str = "error_type";
    pub const ERROR_STATE: &str = "error_state";
    pub const KEY_USAGE: &str = "key_usage";
    pub const MEMORY: &str = "memory";
    pub const REASONING: &str = "reasoning";
    pub const WORKSTREAM: &str = "workstream";
    pub const LOG_TEMPLATE: &str = "log_template";
    pub const SYSTEM: &str = "system";
    pub const GIT_CREDENTIAL: &str = "git_credential";
    pub const LOG_PROCESSED_FILE: &str = "log_processed_file";
    pub const LOG_PENDING_NOTIFICATION: &str = "log_pending_notification";
    pub const FILE_AT_COMMIT: &str = "file_at_commit";
    pub const SYMBOL: &str = "symbol";
    pub const COMMUNITY: &str = "community";
    pub const PROCESS: &str = "process";

    // Edges
    pub const CONTAINS: &str = "contains";
    pub const PARENT_OF: &str = "parent_of";
    pub const POINTED_TO: &str = "pointed_to";
    pub const CHANGED: &str = "changed";
    pub const AUTHORED: &str = "authored";
    pub const IMPLEMENTS: &str = "implements";
    pub const PART_OF: &str = "part_of";
    pub const HAS_REPO: &str = "has_repo";
    pub const HAS_LOG: &str = "has_log";
    pub const HAS_ENTRY: &str = "has_entry";
    pub const IS_TYPE_OF: &str = "is_type_of";
    pub const RELATED_TO: &str = "related_to";
    pub const HAS_MEMORY: &str = "has_memory";
    pub const HAS_REASONING: &str = "has_reasoning";
    pub const HAS_COMMIT_CHUNK: &str = "has_commit_chunk";
    pub const EMITS_LOG: &str = "emits_log";
    pub const SUPERSEDES: &str = "supersedes";
    pub const DERIVED_FROM: &str = "derived_from";
    pub const PART_OF_WORKSTREAM: &str = "part_of_workstream";
    pub const CALLS: &str = "calls";
    pub const USES_TYPE: &str = "uses_type";
    pub const EXTENDS: &str = "extends";
    pub const COUPLED_WITH: &str = "coupled_with";
    pub const MEMBER_OF: &str = "member_of";
}

pub use names as table_names;

/// Every DDL statement, in definition order (`schema.Definition, SCHEMAFULL
/// per). Order matters for `MATERIALIZE INTO`: the `READONLY` list field must
/// exist before the edge field that fills it.
pub const INIT_STATEMENTS: &[&str] = &[
    // ── Named types (4.0): the union order is the sort order ────────────────
    "DEFINE TYPE IF NOT EXISTS batch_status AS 'pending' | 'completed' | 'failed';",
    "DEFINE TYPE IF NOT EXISTS memory_kind AS 'decision' | 'fix' | 'constraint' | 'rule' | 'gotcha' | 'design' | 'fact' | 'working_state' | 'observation' | 'reasoning';",
    "DEFINE TYPE IF NOT EXISTS memory_status AS 'current' | 'superseded' | 'archived' | 'stale';",
    "DEFINE TYPE IF NOT EXISTS memory_visibility AS 'team' | 'personal';",
    // ── Full-text analyzer (lexical channel; ranked by search::score) ────────
    "DEFINE ANALYZER IF NOT EXISTS ibis_text TOKENIZERS class FILTERS lowercase, ascii;",
    // ── Node tables ──────────────────────────────────────────────────────────
    "DEFINE TABLE IF NOT EXISTS repo SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE repo TYPE string;",
    // Materialized back-references on repo (filled by the edge definitions).
    "DEFINE FIELD IF NOT EXISTS has_memory_edges ON TABLE repo TYPE array<record<has_memory>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS has_reasoning_edges ON TABLE repo TYPE array<record<has_reasoning>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS branch SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS commit SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE commit TYPE string;",
    "DEFINE FIELD IF NOT EXISTS date ON TABLE commit TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS message ON TABLE commit TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS repo ON TABLE commit TYPE option<record<repo>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE commit TYPE option<batch_status>;",
    "DEFINE FIELD IF NOT EXISTS batch_error ON TABLE commit TYPE option<string>;",
    // Materialized back-references on commit.
    "DEFINE FIELD IF NOT EXISTS implements_edges ON TABLE commit TYPE array<record<implements>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS changed_edges ON TABLE commit TYPE array<record<changed>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS has_commit_chunk_edges ON TABLE commit TYPE array<record<has_commit_chunk>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS author SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE author TYPE option<string>;",
    // Materialized back-references on author.
    "DEFINE FIELD IF NOT EXISTS authored_edges ON TABLE author TYPE array<record<authored>> DEFAULT [] READONLY;",
    // Composite key: one row per (repo, repo-relative path). Git ingest and
    // code ingest write the same id, so their rows unify. The named components
    // read as fields and serve key-range scans (`WHERE repo = … AND
    // file_path = …`); the secondary path indexes are gone.
    "DEFINE TABLE IF NOT EXISTS source_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS id ON TABLE source_file TYPE [repo: record<repo>, file_path: string];",
    "DEFINE FIELD IF NOT EXISTS path ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS rel_path ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS is_head ON TABLE source_file TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS repo_name ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE source_file TYPE option<datetime>;",
    // Materialized back-references on source_file.
    "DEFINE FIELD IF NOT EXISTS changed_edges ON TABLE source_file TYPE array<record<changed>> DEFAULT [] READONLY;",
    // Composite key: `[file_id, line]` — the chunk location, not the file
    // version (the version lives in the `hash` field; re-ingest refreshes the
    // row in place and re-embeds it via `batch_status`). `file_id` equality
    // scans the table (engine limitation on array-valued record components:
    // only the direct key read / id range hits the key path).
    "DEFINE TABLE IF NOT EXISTS file_chunk SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS id ON TABLE file_chunk TYPE [file_id: record<source_file>, line: int];",
    "DEFINE FIELD IF NOT EXISTS file ON TABLE file_chunk TYPE option<record<source_file>>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS symbol_name ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS kind ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS start_line ON TABLE file_chunk TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS end_line ON TABLE file_chunk TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE file_chunk TYPE option<array<f16>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE file_chunk TYPE option<batch_status>;",
    "DEFINE FIELD IF NOT EXISTS batch_id ON TABLE file_chunk TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS commit_chunk SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE commit_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE commit_chunk TYPE option<array<f16>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE commit_chunk TYPE option<batch_status>;",
    "DEFINE FIELD IF NOT EXISTS batch_id ON TABLE commit_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS commit_hash ON TABLE commit_chunk TYPE option<string>;",
    // Adapter-supplied verbatim string (display field; not a query key).
    "DEFINE FIELD IF NOT EXISTS commit_date ON TABLE commit_chunk TYPE option<string>;",
    // Touch stamp of the batch writer (`apply_commit_txn` sets time::now() on
    // every chunk write; F07 — undeclared writes fail loudly on SCHEMAFULL).
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE commit_chunk TYPE option<datetime>;",
    // Materialized back-reference on commit_chunk.
    "DEFINE FIELD IF NOT EXISTS has_commit_chunk_edges ON TABLE commit_chunk TYPE array<record<has_commit_chunk>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS issue SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS provider ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS external_id ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS external_key ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS project_key ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS subject ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS description ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS status ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS type ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS updated_on ON TABLE issue TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS url ON TABLE issue TYPE option<string>;",
    // Materialized back-references on issue.
    "DEFINE FIELD IF NOT EXISTS implements_edges ON TABLE issue TYPE array<record<implements>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS tracker SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE tracker TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS batch_job SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS status ON TABLE batch_job TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS project SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE project TYPE string;",
    "DEFINE FIELD IF NOT EXISTS baseline_commit ON TABLE project TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS baseline_branch ON TABLE project TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS baseline_source ON TABLE project TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS log_file SCHEMAFULL;",
    // path stays optional: the cost tracker may UPSERT a tokens_used row before
    // the analyzer's first MERGE R1; UNIQUE ignores NONE.
    "DEFINE FIELD IF NOT EXISTS path ON TABLE log_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS project ON TABLE log_file TYPE option<record<project>>;",
    "DEFINE FIELD IF NOT EXISTS offset ON TABLE log_file TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS tokens_used ON TABLE log_file TYPE option<int>;",
    "DEFINE TABLE IF NOT EXISTS log_entry SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE log_entry TYPE option<datetime>;",
    // Materialized back-references on log_entry.
    "DEFINE FIELD IF NOT EXISTS is_type_of_edges ON TABLE log_entry TYPE array<record<is_type_of>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS error_type SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE error_type TYPE string;",
    "DEFINE FIELD IF NOT EXISTS category ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS stack_trace ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS severity ON TABLE error_type TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS template ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE error_type TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS hidden ON TABLE error_type TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE error_type TYPE option<array<f16>>;",
    // Materialized back-references on error_type.
    "DEFINE FIELD IF NOT EXISTS is_type_of_edges ON TABLE error_type TYPE array<record<is_type_of>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS related_to_edges ON TABLE error_type TYPE array<record<related_to>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS error_state SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS key_usage SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS requests ON TABLE key_usage TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS owner ON TABLE key_usage TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS date ON TABLE key_usage TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS last_updated ON TABLE key_usage TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS memory SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS kind ON TABLE memory TYPE memory_kind DEFAULT 'fact';",
    "DEFINE FIELD IF NOT EXISTS title ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE memory TYPE string;",
    "DEFINE FIELD IF NOT EXISTS status ON TABLE memory TYPE memory_status DEFAULT 'current';",
    "DEFINE FIELD IF NOT EXISTS confidence ON TABLE memory TYPE option<float> DEFAULT 0.7;",
    "DEFINE FIELD IF NOT EXISTS visibility ON TABLE memory TYPE memory_visibility DEFAULT 'team';",
    "DEFINE FIELD IF NOT EXISTS owner ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS tags ON TABLE memory TYPE option<array<string>> DEFAULT [];",
    "DEFINE FIELD IF NOT EXISTS branch ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS project ON TABLE memory TYPE option<record<repo>>;",
    "DEFINE FIELD IF NOT EXISTS valid_from ON TABLE memory TYPE option<datetime> DEFAULT time::now();",
    "DEFINE FIELD IF NOT EXISTS valid_until ON TABLE memory TYPE option<datetime>;",
    // provenance members are declared (option<…>); the object itself tolerates
    // partial writer shapes — free-form members land through the declared subfields.
    "DEFINE FIELD IF NOT EXISTS provenance ON TABLE memory TYPE option<object>;",
    "DEFINE FIELD IF NOT EXISTS provenance.agent ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.session ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.tool ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.imported_from ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.source_hash ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.source_path ON TABLE memory TYPE option<string>;",
    // Layer 3 git-bound claims: denormalized scalars for ancestry indexing + record
    // links for audit; dirty/orphan bookkeeping.
    "DEFINE FIELD IF NOT EXISTS introduced_in_oid ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS invalidated_at_oid ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.introduced_in_commit ON TABLE memory TYPE option<record<commit>>;",
    "DEFINE FIELD IF NOT EXISTS provenance.invalidated_at_commit ON TABLE memory TYPE option<record<commit>>;",
    "DEFINE FIELD IF NOT EXISTS provenance.dirty ON TABLE memory TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS provenance.worktree_hash ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provenance.promote_on_commit ON TABLE memory TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS provenance.orphan_oid ON TABLE memory TYPE option<string>;",
    // Claim store: SoT marker (git = derived from .ibis/claims/, tree is truth) + policy audit field.
    "DEFINE FIELD IF NOT EXISTS store ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS policy_applied ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE memory TYPE option<array<f16>>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE memory TYPE datetime DEFAULT time::now();",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE memory TYPE datetime DEFAULT time::now();",
    // Materialized back-references on memory.
    "DEFINE FIELD IF NOT EXISTS supersedes_edges ON TABLE memory TYPE array<record<supersedes>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS superseded_by_edges ON TABLE memory TYPE array<record<supersedes>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS derived_from_edges ON TABLE memory TYPE array<record<derived_from>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS has_memory_edges ON TABLE memory TYPE array<record<has_memory>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS reasoning SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS question ON TABLE reasoning TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS outcome ON TABLE reasoning TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE reasoning TYPE option<array<f16>>;",
    // Materialized back-references on reasoning.
    "DEFINE FIELD IF NOT EXISTS has_reasoning_edges ON TABLE reasoning TYPE array<record<has_reasoning>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS log_template SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS format_string ON TABLE log_template TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS regex ON TABLE log_template TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS source_file ON TABLE log_template TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS source_line ON TABLE log_template TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS origin ON TABLE log_template TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS category ON TABLE log_template TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE log_template TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS file_at_commit SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS oid ON TABLE file_at_commit TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS path ON TABLE file_at_commit TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE file_at_commit TYPE option<string>;",
    // Byte-exact code content (PR #35 hot cache); F-002: bound, never escaped.
    "DEFINE FIELD IF NOT EXISTS content ON TABLE file_at_commit TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE file_at_commit TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS system SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS tokens_used ON TABLE system TYPE option<int>;",
    "DEFINE TABLE IF NOT EXISTS git_credential SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS target ON TABLE git_credential TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS provider ON TABLE git_credential TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS auth_type ON TABLE git_credential TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS token ON TABLE git_credential TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS username ON TABLE git_credential TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS ssh_private_key ON TABLE git_credential TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS log_processed_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS path ON TABLE log_processed_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE log_processed_file TYPE string;",
    "DEFINE FIELD IF NOT EXISTS processed_at ON TABLE log_processed_file TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS log_pending_notification SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS project ON TABLE log_pending_notification TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS log_file ON TABLE log_pending_notification TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS errors_count ON TABLE log_pending_notification TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS severity ON TABLE log_pending_notification TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE log_pending_notification TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE log_pending_notification TYPE option<datetime>;",
    // Axon-like tables (no writer yet — future lanes declare their fields).
    "DEFINE TABLE IF NOT EXISTS symbol SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE symbol TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS file ON TABLE symbol TYPE option<record<source_file>>;",
    // Materialized back-references on symbol (blast radius + dead code read these).
    "DEFINE FIELD IF NOT EXISTS calls_made_edges ON TABLE symbol TYPE array<record<calls>> DEFAULT [] READONLY;",
    "DEFINE FIELD IF NOT EXISTS calls_received_edges ON TABLE symbol TYPE array<record<calls>> DEFAULT [] READONLY;",
    "DEFINE TABLE IF NOT EXISTS community SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS process SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS workstream SCHEMAFULL;",
    // F4 §A.3 workstream browse: the lane declares its fields (the table
    // shipped empty as an Axon-like placeholder). Additive, idempotent.
    "DEFINE FIELD IF NOT EXISTS name ON TABLE workstream TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS status ON TABLE workstream TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE workstream TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE workstream TYPE option<datetime>;",
    // ── Edge tables (written only via RELATE; extra writer fields declared) ──
    // `contains` keeps plain in/out: both endpoints are union-typed and the
    // engine refuses MATERIALIZE INTO for a field that references more than
    // one table (live probe). C7 may split the edge later.
    "DEFINE TABLE IF NOT EXISTS contains TYPE RELATION SCHEMAFULL;",
    // in may be repo (repo→branch/file) or source_file (file→symbol); out stays branch|source_file|symbol.
    "REMOVE FIELD IF EXISTS in ON TABLE contains;",
    "REMOVE FIELD IF EXISTS out ON TABLE contains;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE contains TYPE record<repo | source_file>;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE contains TYPE record<branch | source_file | symbol>;",
    "DEFINE TABLE IF NOT EXISTS parent_of TYPE RELATION IN commit OUT commit SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS pointed_to TYPE RELATION IN branch OUT commit SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS changed TYPE RELATION IN commit OUT source_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS impact ON TABLE changed TYPE option<float>;",
    "DEFINE FIELD IF NOT EXISTS added ON TABLE changed TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS deleted ON TABLE changed TYPE option<int>;",
    // Materialized: both endpoints single-typed.
    "REMOVE FIELD IF EXISTS in ON TABLE changed;",
    "REMOVE FIELD IF EXISTS out ON TABLE changed;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE changed TYPE record<commit> REFERENCE MATERIALIZE INTO commit.changed_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE changed TYPE record<source_file> REFERENCE MATERIALIZE INTO source_file.changed_edges;",
    // `authored` materializes the author side only (out is a union).
    "DEFINE TABLE IF NOT EXISTS authored TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE authored;",
    "REMOVE FIELD IF EXISTS out ON TABLE authored;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE authored TYPE record<author> REFERENCE MATERIALIZE INTO author.authored_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE authored TYPE record<commit | issue>;",
    "DEFINE TABLE IF NOT EXISTS implements TYPE RELATION IN commit OUT issue SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS confidence ON TABLE implements TYPE option<float>;",
    "DEFINE FIELD IF NOT EXISTS usage_weight ON TABLE implements TYPE option<float>;",
    "REMOVE FIELD IF EXISTS in ON TABLE implements;",
    "REMOVE FIELD IF EXISTS out ON TABLE implements;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE implements TYPE record<commit> REFERENCE MATERIALIZE INTO commit.implements_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE implements TYPE record<issue> REFERENCE MATERIALIZE INTO issue.implements_edges;",
    "DEFINE TABLE IF NOT EXISTS part_of TYPE RELATION IN issue OUT tracker SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_repo TYPE RELATION IN project OUT repo SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_log TYPE RELATION IN project OUT log_file SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_entry TYPE RELATION IN log_file OUT log_entry SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS is_type_of TYPE RELATION IN log_entry OUT error_type SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE is_type_of;",
    "REMOVE FIELD IF EXISTS out ON TABLE is_type_of;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE is_type_of TYPE record<log_entry> REFERENCE MATERIALIZE INTO log_entry.is_type_of_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE is_type_of TYPE record<error_type> REFERENCE MATERIALIZE INTO error_type.is_type_of_edges;",
    // `related_to` materializes the error_type side only (out is a union).
    "DEFINE TABLE IF NOT EXISTS related_to TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE related_to;",
    "REMOVE FIELD IF EXISTS out ON TABLE related_to;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE related_to TYPE record<error_type> REFERENCE MATERIALIZE INTO error_type.related_to_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE related_to TYPE record<source_file | file_chunk | issue>;",
    "DEFINE TABLE IF NOT EXISTS has_memory TYPE RELATION IN repo OUT memory SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE has_memory;",
    "REMOVE FIELD IF EXISTS out ON TABLE has_memory;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE has_memory TYPE record<repo> REFERENCE MATERIALIZE INTO repo.has_memory_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE has_memory TYPE record<memory> REFERENCE MATERIALIZE INTO memory.has_memory_edges;",
    "DEFINE TABLE IF NOT EXISTS has_reasoning TYPE RELATION IN repo OUT reasoning SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE has_reasoning;",
    "REMOVE FIELD IF EXISTS out ON TABLE has_reasoning;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE has_reasoning TYPE record<repo> REFERENCE MATERIALIZE INTO repo.has_reasoning_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE has_reasoning TYPE record<reasoning> REFERENCE MATERIALIZE INTO reasoning.has_reasoning_edges;",
    "DEFINE TABLE IF NOT EXISTS has_commit_chunk TYPE RELATION IN commit OUT commit_chunk SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE has_commit_chunk;",
    "REMOVE FIELD IF EXISTS out ON TABLE has_commit_chunk;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE has_commit_chunk TYPE record<commit> REFERENCE MATERIALIZE INTO commit.has_commit_chunk_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE has_commit_chunk TYPE record<commit_chunk> REFERENCE MATERIALIZE INTO commit_chunk.has_commit_chunk_edges;",
    "DEFINE TABLE IF NOT EXISTS emits_log TYPE RELATION IN source_file OUT log_template SCHEMAFULL;",
    // Axon-like edges
    "DEFINE TABLE IF NOT EXISTS calls TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS file ON TABLE calls TYPE option<record<source_file>>;",
    "DEFINE FIELD IF NOT EXISTS line ON TABLE calls TYPE option<int>;",
    "REMOVE FIELD IF EXISTS in ON TABLE calls;",
    "REMOVE FIELD IF EXISTS out ON TABLE calls;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE calls TYPE record<symbol> REFERENCE MATERIALIZE INTO symbol.calls_made_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE calls TYPE record<symbol> REFERENCE MATERIALIZE INTO symbol.calls_received_edges;",
    "DEFINE TABLE IF NOT EXISTS uses_type TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS extends TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS coupled_with TYPE RELATION IN source_file OUT source_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS commit ON TABLE coupled_with TYPE option<record<commit>>;",
    "DEFINE TABLE IF NOT EXISTS member_of TYPE RELATION IN symbol OUT community SCHEMAFULL;",
    // ── Indexes ─────────────────────────────────────────────────────────────
    "DEFINE INDEX IF NOT EXISTS commit_hash ON TABLE commit COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS issue_external ON TABLE issue COLUMNS provider, external_key UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS project_name ON TABLE project COLUMNS name UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS log_file_path ON TABLE log_file COLUMNS path UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS errortype_hash ON TABLE error_type COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS credential_target ON TABLE git_credential COLUMNS target UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS processed_file_hash ON TABLE log_processed_file COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS job_status ON TABLE batch_job COLUMNS status;",
    "DEFINE INDEX IF NOT EXISTS chunk_batch_id ON TABLE file_chunk COLUMNS batch_id;",
    "DEFINE INDEX IF NOT EXISTS chunk_batch_status ON TABLE file_chunk COLUMNS batch_status;",
    // source_file lookups by path go through the named id components
    // (`WHERE repo = … AND file_path = …` → KeyRangeScan); the old
    // `file_path`/`file_version` secondary indexes duplicated the key.
    // Vector indexes: DiskANN (8-bit quantised codes + exact rerank), 768-dim
    // cosine, elements stored f16 (coerced from the f32 vectors we write).
    "DEFINE INDEX IF NOT EXISTS vector_embedding ON TABLE file_chunk COLUMNS embedding DISKANN DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS error_embedding ON TABLE error_type COLUMNS embedding DISKANN DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS memory_embedding ON TABLE memory COLUMNS embedding DISKANN DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS reasoning_embedding ON TABLE reasoning COLUMNS embedding DISKANN DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS commit_chunk_embedding ON TABLE commit_chunk COLUMNS embedding DISKANN DIMENSION 768 DIST COSINE;",
    // Lexical channel: one index per field (an index cannot span fields).
    "DEFINE INDEX IF NOT EXISTS memory_title_text ON TABLE memory COLUMNS title FULLTEXT ANALYZER ibis_text BM25;",
    "DEFINE INDEX IF NOT EXISTS memory_content_text ON TABLE memory COLUMNS content FULLTEXT ANALYZER ibis_text BM25;",
    "DEFINE INDEX IF NOT EXISTS file_chunk_content_text ON TABLE file_chunk COLUMNS content FULLTEXT ANALYZER ibis_text BM25;",
    // Memory absorption: new edges
    // supersedes: both endpoints are memory — both lists live on memory.
    "DEFINE TABLE IF NOT EXISTS supersedes TYPE RELATION IN memory OUT memory SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE supersedes;",
    "REMOVE FIELD IF EXISTS out ON TABLE supersedes;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE supersedes TYPE record<memory> REFERENCE MATERIALIZE INTO memory.supersedes_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE supersedes TYPE record<memory> REFERENCE MATERIALIZE INTO memory.superseded_by_edges;",
    // `derived_from` materializes the memory side only (out is a union).
    "DEFINE TABLE IF NOT EXISTS derived_from TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE derived_from;",
    "REMOVE FIELD IF EXISTS out ON TABLE derived_from;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE derived_from TYPE record<memory> REFERENCE MATERIALIZE INTO memory.derived_from_edges;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE derived_from TYPE record<source_file | file_chunk | commit | issue>;",
    "DEFINE TABLE IF NOT EXISTS part_of_workstream TYPE RELATION IN memory OUT workstream SCHEMAFULL;",
    // Memory absorption: indexes
    "DEFINE INDEX IF NOT EXISTS memory_kind ON TABLE memory COLUMNS kind;",
    "DEFINE INDEX IF NOT EXISTS memory_status ON TABLE memory COLUMNS status;",
    "DEFINE INDEX IF NOT EXISTS memory_visibility ON TABLE memory COLUMNS visibility;",
    "DEFINE INDEX IF NOT EXISTS memory_project ON TABLE memory COLUMNS project;",
    // Layer 3: ancestry prefilter indexes
    "DEFINE INDEX IF NOT EXISTS memory_introduced_in_oid ON TABLE memory COLUMNS project, introduced_in_oid;",
    "DEFINE INDEX IF NOT EXISTS memory_invalidated_at_oid ON TABLE memory COLUMNS project, invalidated_at_oid;",
    // Claim store: withdrawal reconciliation scans by store.
    "DEFINE INDEX IF NOT EXISTS memory_store ON TABLE memory COLUMNS project, store;",
];

/// Returns the full SurrealQL script to initialize the database.
pub fn generate_init_sql() -> String {
    let mut out = String::with_capacity(8 * 1024);
    for statement in INIT_STATEMENTS {
        out.push_str(statement);
        out.push('\n');
    }
    out
}

/// Initializes the database schema by defining tables, edges and indexes.
///
/// Unlike the `InitSchema`, statement failures are propagated instead of only
/// logged: a silently missing `DEFINE` (e.g. a UNIQUE index) breaks dedup and
/// privacy semantics downstream.
pub async fn init_schema(exec: &dyn Executor) -> Result<(), DbError> {
    tracing::info!("Initializing database schema...");
    for statement in INIT_STATEMENTS {
        exec.execute(statement).await?;
    }
    tracing::info!("Database schema initialized successfully.");
    Ok(())
}

/// Re-runs the DDL to ensure indexes exist.
pub async fn ensure_indexes(exec: &dyn Executor) -> Result<(), DbError> {
    tracing::info!("Ensuring database indexes...");
    exec.execute(&generate_init_sql()).await?;
    Ok(())
}
