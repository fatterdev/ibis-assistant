//! Final-state database DDL, **SCHEMAFULL-typed**
//! per the D17 decision (SurrealDB 3.x-native).
//!
//! Rules encoded here:
//! - every field any writer persists is `DEFINE FIELD IF NOT EXISTS`-declared;
//!   undeclared writes now fail loudly (3.x enforcement) instead of silently
//!   accepting typos;
//! - mandatory (non-`option`) only where **every** writer of the field always
//!   supplies it — 3.x rejects `UPSERT`/`MERGE` that misses a mandatory field
//!   when the record is missing;
//! - edge tables keep `TYPE RELATION IN … OUT …`; their writer-set extra fields
//!   are declared explicitly — edges are only ever written via
//!   `RELATE`, never directly upserted;
//! - semantic datetimes are `datetime`-typed: writers bind `Value::Datetime` or
//!   `time::now` (D17 R7); verbatim API strings stay `string`;
//! - `DEFAULT` covers the first-write gaps the legacy implicit-create used to
//!   fill (e.g. chunk embeddings stay `NONE` on first write, preserving the
//!   F-A03 partial-resume semantics);
//! - tables with no writer yet (`batch_job`, `error_state`, `community`,
//!   `process`, `branch`, `workstream`) are declared without fields: their
//!   future porting lanes add their own `DEFINE FIELD` lines (loud failure
//!   beats silent schema drift).
//!
//! Everything is born fresh: `contains.in` is typed `record<repo | source_file>`
//! directly, `memory` defaults are baked into the field definitions, the legacy
//! `file` rename and `ClearAbsoluteFileRecords` are omitted, and the
//! migration framework (see [`crate::migrate`]) starts with an empty history.
//!
//! Statements are string literals (identifiers only — no content interpolation,
//! F-002); they may be re-run safely (`IF NOT EXISTS` everywhere, LB-07).

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
/// per).
pub const INIT_STATEMENTS: &[&str] = &[
    // ── Node tables ──────────────────────────────────────────────────────────
    "DEFINE TABLE IF NOT EXISTS repo SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE repo TYPE string;",
    "DEFINE TABLE IF NOT EXISTS branch SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS commit SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE commit TYPE string;",
    "DEFINE FIELD IF NOT EXISTS date ON TABLE commit TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS message ON TABLE commit TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS repo ON TABLE commit TYPE option<record<repo>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE commit TYPE option<string> ASSERT $value IN ['pending','completed','failed'];",
    "DEFINE FIELD IF NOT EXISTS batch_error ON TABLE commit TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS author SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS name ON TABLE author TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS source_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS path ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS rel_path ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS is_head ON TABLE source_file TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS repo_name ON TABLE source_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE source_file TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS file_chunk SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS file ON TABLE file_chunk TYPE option<record<source_file>>;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS symbol_name ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS kind ON TABLE file_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS start_line ON TABLE file_chunk TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS end_line ON TABLE file_chunk TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE file_chunk TYPE option<array<float>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE file_chunk TYPE option<string> ASSERT $value IN ['pending','completed','failed'];",
    "DEFINE FIELD IF NOT EXISTS batch_id ON TABLE file_chunk TYPE option<string>;",
    "DEFINE TABLE IF NOT EXISTS commit_chunk SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE commit_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE commit_chunk TYPE option<array<float>>;",
    "DEFINE FIELD IF NOT EXISTS batch_status ON TABLE commit_chunk TYPE option<string> ASSERT $value IN ['pending','completed','failed'];",
    "DEFINE FIELD IF NOT EXISTS batch_id ON TABLE commit_chunk TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS commit_hash ON TABLE commit_chunk TYPE option<string>;",
    // Adapter-supplied verbatim string (display field; not a query key).
    "DEFINE FIELD IF NOT EXISTS commit_date ON TABLE commit_chunk TYPE option<string>;",
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
    // the analyzer's first MERGE R1; UNIQUE ignores NONE — probed).
    "DEFINE FIELD IF NOT EXISTS path ON TABLE log_file TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS project ON TABLE log_file TYPE option<record<project>>;",
    "DEFINE FIELD IF NOT EXISTS offset ON TABLE log_file TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS tokens_used ON TABLE log_file TYPE option<int>;",
    "DEFINE TABLE IF NOT EXISTS log_entry SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE log_entry TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS error_type SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS hash ON TABLE error_type TYPE string;",
    "DEFINE FIELD IF NOT EXISTS category ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS stack_trace ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS severity ON TABLE error_type TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS template ON TABLE error_type TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE error_type TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS hidden ON TABLE error_type TYPE option<bool>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE error_type TYPE option<array<float>>;",
    "DEFINE TABLE IF NOT EXISTS error_state SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS key_usage SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS requests ON TABLE key_usage TYPE option<int>;",
    "DEFINE FIELD IF NOT EXISTS owner ON TABLE key_usage TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS date ON TABLE key_usage TYPE option<datetime>;",
    "DEFINE FIELD IF NOT EXISTS last_updated ON TABLE key_usage TYPE option<datetime>;",
    "DEFINE TABLE IF NOT EXISTS memory SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS kind ON TABLE memory TYPE string ASSERT $value IN ['decision','fix','constraint','rule','gotcha','design','fact','working_state','observation','reasoning'] DEFAULT 'fact';",
    "DEFINE FIELD IF NOT EXISTS title ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS content ON TABLE memory TYPE string;",
    "DEFINE FIELD IF NOT EXISTS status ON TABLE memory TYPE string ASSERT $value IN ['current','superseded','archived','stale'] DEFAULT 'current';",
    "DEFINE FIELD IF NOT EXISTS confidence ON TABLE memory TYPE option<float> DEFAULT 0.7;",
    "DEFINE FIELD IF NOT EXISTS visibility ON TABLE memory TYPE string ASSERT $value IN ['team','personal'] DEFAULT 'team';",
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
    // Claim store: SoT marker (git = derived from .threski/claims/, tree is truth) + policy audit field.
    "DEFINE FIELD IF NOT EXISTS store ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS policy_applied ON TABLE memory TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE memory TYPE option<array<float>>;",
    "DEFINE FIELD IF NOT EXISTS created_at ON TABLE memory TYPE datetime DEFAULT time::now();",
    "DEFINE FIELD IF NOT EXISTS updated_at ON TABLE memory TYPE datetime DEFAULT time::now();",
    "DEFINE TABLE IF NOT EXISTS reasoning SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS question ON TABLE reasoning TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS outcome ON TABLE reasoning TYPE option<string>;",
    "DEFINE FIELD IF NOT EXISTS embedding ON TABLE reasoning TYPE option<array<float>>;",
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
    "DEFINE TABLE IF NOT EXISTS community SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS process SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS workstream SCHEMAFULL;",
    // ── Edge tables (written only via RELATE; extra writer fields declared) ──
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
    "DEFINE TABLE IF NOT EXISTS authored TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE authored;",
    "REMOVE FIELD IF EXISTS out ON TABLE authored;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE authored TYPE record<author>;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE authored TYPE record<commit | issue>;",
    "DEFINE TABLE IF NOT EXISTS implements TYPE RELATION IN commit OUT issue SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS confidence ON TABLE implements TYPE option<float>;",
    "DEFINE FIELD IF NOT EXISTS usage_weight ON TABLE implements TYPE option<float>;",
    "DEFINE TABLE IF NOT EXISTS part_of TYPE RELATION IN issue OUT tracker SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_repo TYPE RELATION IN project OUT repo SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_log TYPE RELATION IN project OUT log_file SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_entry TYPE RELATION IN log_file OUT log_entry SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS is_type_of TYPE RELATION IN log_entry OUT error_type SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS related_to TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE related_to;",
    "REMOVE FIELD IF EXISTS out ON TABLE related_to;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE related_to TYPE record<error_type>;",
    "DEFINE FIELD IF NOT EXISTS out ON TABLE related_to TYPE record<source_file | file_chunk | issue>;",
    "DEFINE TABLE IF NOT EXISTS has_memory TYPE RELATION IN repo OUT memory SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_reasoning TYPE RELATION IN repo OUT reasoning SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS has_commit_chunk TYPE RELATION IN commit OUT commit_chunk SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS emits_log TYPE RELATION IN source_file OUT log_template SCHEMAFULL;",
    // Axon-like edges
    "DEFINE TABLE IF NOT EXISTS calls TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS file ON TABLE calls TYPE option<record<source_file>>;",
    "DEFINE FIELD IF NOT EXISTS line ON TABLE calls TYPE option<int>;",
    "DEFINE TABLE IF NOT EXISTS uses_type TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS extends TYPE RELATION IN symbol OUT symbol SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS coupled_with TYPE RELATION IN source_file OUT source_file SCHEMAFULL;",
    "DEFINE FIELD IF NOT EXISTS commit ON TABLE coupled_with TYPE option<record<commit>>;",
    "DEFINE TABLE IF NOT EXISTS member_of TYPE RELATION IN symbol OUT community SCHEMAFULL;",
    // Indexes
    "DEFINE INDEX IF NOT EXISTS commit_hash ON TABLE commit COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS file_path ON TABLE source_file COLUMNS path;",
    "DEFINE INDEX IF NOT EXISTS file_version ON TABLE source_file COLUMNS path, hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS issue_external ON TABLE issue COLUMNS provider, external_key UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS project_name ON TABLE project COLUMNS name UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS log_file_path ON TABLE log_file COLUMNS path UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS errortype_hash ON TABLE error_type COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS credential_target ON TABLE git_credential COLUMNS target UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS processed_file_hash ON TABLE log_processed_file COLUMNS hash UNIQUE;",
    "DEFINE INDEX IF NOT EXISTS job_status ON TABLE batch_job COLUMNS status;",
    "DEFINE INDEX IF NOT EXISTS chunk_batch_id ON TABLE file_chunk COLUMNS batch_id;",
    "DEFINE INDEX IF NOT EXISTS chunk_batch_status ON TABLE file_chunk COLUMNS batch_status;",
    // Vector indexes (embeddings: array<float>, 768-dim, cosine)
    "DEFINE INDEX IF NOT EXISTS vector_embedding ON TABLE file_chunk COLUMNS embedding HNSW DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS error_embedding ON TABLE error_type COLUMNS embedding HNSW DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS memory_embedding ON TABLE memory COLUMNS embedding HNSW DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS reasoning_embedding ON TABLE reasoning COLUMNS embedding HNSW DIMENSION 768 DIST COSINE;",
    "DEFINE INDEX IF NOT EXISTS commit_chunk_embedding ON TABLE commit_chunk COLUMNS embedding HNSW DIMENSION 768 DIST COSINE;",
    // Memory absorption: new edges
    "DEFINE TABLE IF NOT EXISTS supersedes TYPE RELATION IN memory OUT memory SCHEMAFULL;",
    "DEFINE TABLE IF NOT EXISTS derived_from TYPE RELATION SCHEMAFULL;",
    "REMOVE FIELD IF EXISTS in ON TABLE derived_from;",
    "REMOVE FIELD IF EXISTS out ON TABLE derived_from;",
    "DEFINE FIELD IF NOT EXISTS in ON TABLE derived_from TYPE record<memory>;",
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
