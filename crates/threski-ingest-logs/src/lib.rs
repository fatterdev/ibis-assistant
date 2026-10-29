//! threski-ingest-logs — the log analysis pipeline (
//! `internal/ingest/logs`, 2,937 LOC across 9 files).
//!
//! Flow: watcher/tailer collect lines → [`analyzer::LogAnalyzer`] runs the
//! staged analysis (static LogAlign → known-marker → AI) →
//! [`logalign`] promotes AI templates → [`ast_rule_gen`] synthesizes ast-grep
//! rules → [`reporter`] writes markdown reports → [`notifier`] emails/queues
//! digests → [`polling`] pulls remote log dirs → [`enrich`] resolves
//! file/symbol/baseline context.
//!
//! Design decisions honored:
//! - **D1**: SMB remote polling dropped — UNC/native mounts via the local
//!   filesystem client; `smb` protocol yields a typed fallback error.
//! - **D12**: `notify` crate + custom tailer with DB-offset resume (same
//!   batching: ≥100 lines, entry-regex cut, ≥500 fallback, 2s ticker, format
//!   discovery after 50 lines).
//! - **F-002**: every content-bearing SurrealQL statement binds variables;
//!   record targets are inlined only through sanitized/validated ids.
//! - **G-A09**: watcher/tailer/poller tasks are owned (shutdown channels +
//!   joins) — used detached goroutines.
//!
//! Wiring seams consumed by the server lane (wave 4):
//! - [`analyzer::ProgressReporter`] — throttled MCP progress (lane N).
//! - [`enrich::CodeSearch`] — vector search backed by `threski-search` (lane H).
//! - [`ast_rule_gen::RuleSynthesizer`] — unified persistence owned by
//!   `threski-ingest-code` (lane F); the default in-crate implementation ports
//!   the same behavior standalone.

pub mod analyzer;
pub mod ast_rule_gen;
pub mod enrich;
pub mod error;
pub mod logalign;
pub mod notifier;
pub mod polling;
pub mod reporter;
pub mod smtp;
pub mod tailer;
pub mod watcher;

pub use analyzer::{
    prefilter_noise_lines, read_capped, LogAnalyzer, LogTemplatePattern, Pattern, ProgressReporter,
    SemanticError, DEFAULT_BATCH_CHUNK_SIZE, MAX_LOG_INGEST_BYTES,
};
pub use ast_rule_gen::{
    infer_language_from_errors, looks_like_sg_error, normalize_ast_language,
    parse_ai_ast_rule_json, structural_validate_ast_rule, ASTRuleGenResult, RulePersistOutcome,
    RuleSynthesizer,
};
pub use error::{LogsError, Result};
pub use logalign::{
    is_useful_static_pattern, looks_dangerous_regex, safe_compile_log_align,
    template_to_flexible_regex,
};
pub use notifier::Notifier;
pub use polling::{
    content_hash, join_host_port, parse_go_duration, GlobPattern, LocalFs, PollingManager,
    RemoteEntry, RemoteFs, DEFAULT_POLL_INTERVAL,
};
pub use reporter::Reporter;
pub use tailer::Tailer;
pub use watcher::{is_watchable_log, Watcher};
