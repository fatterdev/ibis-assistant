//! threski-ingest-code — polyglot code ingest.
//!
//! Discovery-driven scan of the workspace (git tree first, directory-walk
//! fallback), ast-grep (`sg`) chunking with symbol/call edges and log
//! templates, sparse-AST detection with AI rule synthesis + validated
//! persistence, the ast-grep binary installer, and repo ignore handling.
//!
//! Module map:
//! - [`ingest`] — [`ingest_codebase`] orchestration, prune,
//!   per-file processing, naive chunking.
//! - [`ast_chunker`] — `sg` invocation, NDJSON parsing,
//!   rule-suffix dispatch, printf-format → regex conversion, sg config/binary
//!   discovery ([`ast_chunker::SgLocator`] is the injectable seam; tests skip
//!   gracefully when no `sg` binary is available).
//! - [`rule_synth`] — [`rule_synth::persist_validated_rule`]
//!   (shared with the log lane), [`rule_synth::synthesize_ingest_rules`],
//!   [`rule_synth::ContentGenerator`] (implemented by [`threski_ai::Client`];
//!   test fakes implement it directly and the sg sandbox is injectable via
//!   [`rule_synth::persist_validated_rule_with`]).
//! - [`install`] — [`install::ensure_ast_grep`],
//!   [`install::ensure_ast_grep_rules`], release download/extract.
//! - [`sparse_ast`] — synthesis need + host-language hints.
//! - [`ignore`] — `.gitignore`/`.aiignore` union matcher.
//! - [`lang_globs`] — sgconfig.yml `languageGlobs` edits.
//!
//! Cross-lane contract: every content-bearing SurrealQL statement binds its
//! values via [`threski_db::executor::Executor::smart_query`] (F-002); record
//! links are bound as `Value::RecordId` and record targets are inlined
//! through `format_record_id` (LB-08); all git access goes through
//! [`threski_git::GitRunner`] (D-A07).

pub mod ast_chunker;
pub mod error;
pub mod ignore;
pub mod ingest;
pub mod install;
pub mod lang_globs;
pub mod rule_synth;
pub mod sparse_ast;

pub use ast_chunker::{
    guess_symbol_name, has_sg_config, is_useful_log_template, parse_ast, parse_ast_with,
    parse_format_to_regex, parse_sg_output, resolve_sg_binary, sg_binary_path, sg_config_dir,
    AstChunk, CallEdge, LogTemplate, SgLocator, SgMatch,
};
pub use error::{CodeIngestError, RulePersistError};
pub use ignore::{is_ignored, load_ignore_patterns};
pub use ingest::{ingest_codebase, AiClientAdapter, CodeAIClient, ProgressCallback};
pub use install::{
    ast_grep_bin_path, ensure_ast_grep, ensure_ast_grep_rules, extract_tar_gz, extract_zip,
    latest_release, local_version, match_asset, parse_version_output, sync_sg_assets, GitHubAsset,
    GitHubRelease,
};
pub use lang_globs::{ensure_language_glob, upsert_language_glob};
pub use rule_synth::{
    canonicalize_generated_rule_id, ensure_synth_yaml_id, maybe_synthesize_for_sparse_ast,
    normalize_rule_kind_from_str, normalize_synth_language, persist_validated_rule,
    persist_validated_rule_with, synthesize_ingest_rules, ContentGenerator, RuleKind,
    RulePersistResult, SampleFile, SgSandbox, MAX_SYNTH_PER_EXT_BATCH, MAX_SYNTH_YAML_BYTES,
    RULE_QUARANTINE_DIR, SYNTH_COOLDOWN,
};
pub use sparse_ast::{is_html_host_ext, needs_rule_synthesis, suggest_host_language};
