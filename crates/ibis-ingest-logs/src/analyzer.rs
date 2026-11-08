//! The staged log analyzer.
//!
//! Pipeline per batch: deterministic LogAlign matching (AST/static templates)
//! → known-pattern masking with `[KNOWN_ERROR: category]` markers →
//! known-markers-only early exit (AI skipped) → AI classification →
//! enrichment → LogAlign promotion → optional ast-grep rule synthesis.
//! Batch chunking caps every AI prompt at [`DEFAULT_BATCH_CHUNK_SIZE`] lines;
//! known patterns accumulate across chunks within one [`LogAnalyzer::process_batch_sync`].
//!
//! Parity notes:
//! - All content-bearing SurrealQL goes through bound variables (F-002); record
//!   targets are inlined identifiers built via `ibis_db::ids::format_record_id`
//!   (sanitized, verified-safe) — the original code interpolated escaped strings.
//! - `AiError::ContextOnly` from the AI stage degrades like any AI failure:
//!   partial results are returned and the pipeline never crashes.
//! - The cost tracker is bridged onto the [`Executor`] via [`DbCostStore`]
//!   (`ai.NewCostTracker(dbClient) wrote `tokens_used` directly).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use ibis_ai::cost::{CostStore, CostTracker};
use ibis_ai::types::{Content, GenerationConfig, Part};
use ibis_ai::Client;
use ibis_db::executor::{vars, Executor};
use ibis_db::ids::{format_record_id, sanitize_id};
use ibis_db::schema::names as schema;
use ibis_db::value::{SurrealValue, I64};

use crate::ast_rule_gen::{LocalRuleSynthesizer, RuleSynthesizer};
use crate::enrich::{normalize_rel_path, CodeSearch};
use crate::error::{LogsError, Result};
use crate::logalign::is_useful_static_pattern;

/// Batch processing limit: lines per AI-prompt chunk.
pub const DEFAULT_BATCH_CHUNK_SIZE: usize = 250;

/// Caps HTTP/polling body reads (~32 MiB). Prefer truncating and
/// chunk-processing over OOM; callers surface a clear warning.
pub const MAX_LOG_INGEST_BYTES: usize = 32 << 20;

/// D10 per-task routing: semantic log analysis and format discovery are
/// mid-tier local tasks (cloud fallback preserved by the `local:` base).
const ANALYZER_ROUTE_HINT: &str = "local:M";

/// Progress reporter seam: the server lane attaches a
/// throttled MCP implementation; `None` is a no-op.
pub trait ProgressReporter: Send + Sync {
    fn report(&self, done: u64, total: u64, message: &str);
}

/// A known-error masking pattern: category + compiled regex
/// built from an AI `<VAR>` mask template.
#[derive(Clone)]
pub struct Pattern {
    pub category: String,
    pub regex: regex::Regex,
}

/// A static LogAlign template.
#[derive(Clone)]
pub struct LogTemplatePattern {
    pub id: String,
    pub regex: regex::Regex,
    pub format_str: String,
    pub source_file: String,
    pub source_line: i64,
}

/// One classified log anomaly (`SemanticError; serde tags match JSON).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SemanticError {
    #[serde(default)]
    pub category: String,
    #[serde(rename = "stack_trace", default)]
    pub stack_trace: String,
    #[serde(default)]
    pub file: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub line: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub symbol: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cause: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub baseline: String,
    #[serde(default)]
    pub severity: i64,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub supersedes: Vec<String>,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// DB row projection for loading known error categories (constructor).
#[derive(Debug, SurrealValue)]
struct KnownPatternRow {
    category: Option<String>,
    template: Option<String>,
}

/// DB row projection for loading static log templates (constructor).
#[derive(Debug, SurrealValue)]
struct TemplateRow {
    id: Option<ibis_db::value::RecordId>,
    format_string: Option<String>,
    regex: Option<String>,
    source_file: Option<String>,
    source_line: Option<I64>,
    edge_rel_path: Option<String>,
}

/// DB row projection for the log-file offset (GetOffset).
#[derive(Debug, SurrealValue)]
struct OffsetRow {
    offset: Option<I64>,
}

/// Bridges the cost tracker onto the live executor (SQL parity with
/// : `UPSERT <id> SET tokens_used += <n>;` — the SurrealDB 3.x
/// idiomatic counter increment: `+=` infers `int` with default 0 on first
/// write, so no `OR 0` guard is needed).
pub struct DbCostStore {
    db: Arc<dyn Executor>,
}

#[async_trait]
impl CostStore for DbCostStore {
    async fn increment_tokens(
        &self,
        record_id: &str,
        total: u64,
    ) -> std::result::Result<(), ibis_ai::AiError> {
        let sql = format!("UPSERT {record_id} SET tokens_used += {total};");
        self.db
            .execute(&sql)
            .await
            .map_err(|e| ibis_ai::AiError::other(e.to_string()))?;
        Ok(())
    }
}

/// The log analyzer: one instance per log file / per analysis request.
pub struct LogAnalyzer {
    /// Absolute path of the analyzed log (or a synthetic logical path for
    /// MCP/CLI/polling uploads).
    pub path: String,
    pub cfg: Option<Arc<ibis_core::config::Config>>,
    pub db: Option<Arc<dyn Executor>>,
    pub ai: Option<Arc<Client>>,
    pub project: String,
    /// `log_file:⟨sanitized-basename⟩` record id used for offsets/cost.
    pub log_file_id: String,
    pub cost_tracker: Arc<CostTracker>,
    /// AI classification prompt enrichment seam (lane H provides the search
    /// implementation; `None` skips the vector-search stage).
    pub code_search: Option<Arc<dyn CodeSearch>>,
    /// Path-D ast-grep rule synthesizer (defaults to the local implementation;
    /// the wiring lane may substitute the unified ingest-code one).
    pub rule_synth: Arc<dyn RuleSynthesizer>,
    /// Progress reporter seam (None = no-op, `progress.Report).
    pub progress: Option<Arc<dyn ProgressReporter>>,
    /// Git runner for baseline HEAD probes (D-A07: all git access routes
    /// through the credential-injecting runner; anonymous by default).
    pub git: ibis_git::GitRunner,
    pub(crate) known_patterns: Mutex<Vec<Pattern>>,
    pub(crate) static_patterns: Mutex<Vec<LogTemplatePattern>>,
    /// Limits Path-D AI→ast-grep rule synthesis to once per process_batch_sync.
    pub(crate) ast_rule_gen_used: AtomicBool,
}

impl Default for LogAnalyzer {
    fn default() -> Self {
        LogAnalyzer {
            path: String::new(),
            cfg: None,
            db: None,
            ai: None,
            project: String::new(),
            log_file_id: String::new(),
            cost_tracker: Arc::new(CostTracker::new(None)),
            code_search: None,
            rule_synth: Arc::new(LocalRuleSynthesizer::default()),
            progress: None,
            git: ibis_git::GitRunner::anonymous(),
            known_patterns: Mutex::new(Vec::new()),
            static_patterns: Mutex::new(Vec::new()),
            ast_rule_gen_used: AtomicBool::new(false),
        }
    }
}

impl LogAnalyzer {
    /// Builds a minimal analyzer without any DB/AI initialization — the
    /// in-memory test fixture shape ( tests construct `&LogAnalyzer{}`).
    pub fn bare(project: impl Into<String>) -> Self {
        LogAnalyzer {
            project: project.into(),
            ..Default::default()
        }
    }

    /// Equivalent to `new_with_project(path, "", …)`:
    /// the project is inferred from the path.
    pub async fn new(
        path: &str,
        cfg: Option<Arc<ibis_core::config::Config>>,
        db: Option<Arc<dyn Executor>>,
        ai: Option<Arc<Client>>,
    ) -> Self {
        Self::new_with_project(path, "", cfg, db, ai).await
    }

    /// Builds an analyzer bound to an explicit project name (
    /// `NewLogAnalyzerWithProject`). When project is empty it prefers the
    /// parent directory of the path (`logs/<project>/<file>`) and falls back
    /// to the legacy `<project>_<rest>.log` filename convention.
    ///
    /// Performs the database bootstrap writes (project/log_file upsert + the
    /// `has_log` edge) and loads known patterns + static log templates.
    pub async fn new_with_project(
        path: &str,
        project: &str,
        cfg: Option<Arc<ibis_core::config::Config>>,
        db: Option<Arc<dyn Executor>>,
        ai: Option<Arc<Client>>,
    ) -> Self {
        // Fn-item paths (HRTB) — N-A14 (see ibis-ingest-git note).
        let base = std::path::Path::new(path)
            .file_name()
            .map(std::ffi::OsStr::to_string_lossy)
            .map(|cow| cow.into_owned())
            .unwrap_or_default();

        let mut project = project.to_string();
        if project.is_empty() {
            let parent = std::path::Path::new(path)
                .parent()
                .and_then(|p| p.file_name())
                .map(std::ffi::OsStr::to_string_lossy)
                .map(|cow| cow.into_owned())
                .unwrap_or_default();
            let logs_root_base = cfg
                .as_ref()
                .map(|c| {
                    std::path::Path::new(c.logs_root.trim())
                        .file_name()
                        .map(std::ffi::OsStr::to_string_lossy)
                        .map(|cow| cow.into_owned())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            if !parent.is_empty() && parent != "." && parent != logs_root_base {
                project = parent;
            } else if let Some(first) = base.split('_').next() {
                if !first.is_empty() && base.contains('_') {
                    project = first.to_string();
                } else {
                    project = "Unknown".to_string();
                }
            } else {
                project = "Unknown".to_string();
            }
        }

        // Initialize database records for the log file and its associated project.
        let log_file_id = format_record_id(schema::LOG_FILE, &sanitize_id(&base));
        let project_id = format_record_id(schema::PROJECT, &sanitize_id(&project));

        if let Some(db) = &db {
            if let Err(e) = db
                .smart_query(
                    &format!("UPSERT {project_id} SET name = $name;"),
                    vars([("name", project.as_str())]),
                )
                .await
            {
                tracing::error!("Log analyzer project upsert failed: {e}");
            }
            if let Err(e) = db
                .smart_query(
                    &format!(
                        "UPSERT {log_file_id} MERGE {{ path: $path, project: {project_id} }};"
                    ),
                    vars([("path", path)]),
                )
                .await
            {
                tracing::error!("Log analyzer log_file upsert failed: {e}");
            }
            if let Err(e) = db
                .execute(&format!(
                    "RELATE {}->{}->{};",
                    project_id,
                    schema::HAS_LOG,
                    log_file_id
                ))
                .await
            {
                tracing::error!("Log analyzer has_log edge failed: {e}");
            }
        }

        let cost_tracker = match &db {
            Some(db) => Arc::new(CostTracker::new(Some(
                Arc::new(DbCostStore { db: db.clone() }) as Arc<dyn CostStore>,
            ))),
            None => Arc::new(CostTracker::new(None)),
        };

        let mut analyzer = LogAnalyzer {
            path: path.to_string(),
            cfg,
            db: db.clone(),
            ai: ai.clone(),
            project: project.clone(),
            log_file_id: log_file_id.clone(),
            cost_tracker,
            code_search: None,
            rule_synth: Arc::new(LocalRuleSynthesizer::default()),
            progress: None,
            git: ibis_git::GitRunner::anonymous(),
            known_patterns: Mutex::new(Vec::new()),
            static_patterns: Mutex::new(Vec::new()),
            ast_rule_gen_used: AtomicBool::new(false),
        };

        if let Some(db) = &db {
            // Load known error categories (non-hidden, with templates).
            let sql = format!(
                "SELECT category, template, created_at FROM {} WHERE template != '' AND hidden != true ORDER BY created_at ASC;",
                schema::ERROR_TYPE
            );
            match db.execute(&sql).await {
                Ok(outcome) => match outcome.rows::<KnownPatternRow>(&sql) {
                    Ok(rows) => {
                        for row in rows {
                            let (Some(cat), Some(tmpl)) = (row.category, row.template) else {
                                continue;
                            };
                            if cat.is_empty() || tmpl.is_empty() {
                                continue;
                            }
                            if let Err(e) = analyzer.add_known_pattern(&cat, &tmpl) {
                                tracing::warn!("Skipping known pattern '{cat}': {e}");
                            }
                        }
                    }
                    Err(e) => tracing::warn!("Known-pattern load failed: {e}"),
                },
                Err(e) => tracing::warn!("Known-pattern query failed: {e}"),
            }

            // Load static log templates (prefer repo-relative path from the
            // emits_log edge when present; fall back for older DBs).
            let edge_sql = format!(
                "SELECT id, format_string, regex, source_file, source_line,\n\t\t\tarray::first(<-{edge}<-{file}.rel_path) as edge_rel_path\n\t\tFROM {tmpl};",
                edge = schema::EMITS_LOG,
                file = schema::SOURCE_FILE,
                tmpl = schema::LOG_TEMPLATE
            );
            let outcome = match db.execute(&edge_sql).await {
                Ok(out) => Some(out),
                Err(e) => {
                    tracing::debug!("LogAlign template edge query failed ({e}); falling back");
                    let plain_sql = format!(
                        "SELECT id, format_string, regex, source_file, source_line FROM {};",
                        schema::LOG_TEMPLATE
                    );
                    db.execute(&plain_sql)
                        .await
                        .ok()
                        .map(|out| (plain_sql, out))
                        .map(|(_, out)| out)
                }
            };
            if let Some(outcome) = outcome {
                let rows = outcome.rows::<TemplateRow>(&edge_sql).unwrap_or_default();
                let mut statics = Vec::new();
                for row in rows {
                    let Some(regex_str) = row.regex.filter(|r| !r.is_empty()) else {
                        continue;
                    };
                    let format_str = row.format_string.unwrap_or_default();
                    let mut src_file = row.source_file.unwrap_or_default();
                    if src_file.is_empty() {
                        src_file = row.edge_rel_path.unwrap_or_default();
                    }
                    let src_file = normalize_rel_path(&src_file);
                    let src_line = row.source_line.map(|v| v.get()).unwrap_or(0);
                    if !is_useful_static_pattern(&format_str, &regex_str) {
                        continue;
                    }
                    if let Ok(re) = compile_dotall(&regex_str) {
                        statics.push(LogTemplatePattern {
                            id: row
                                .id
                                .as_ref()
                                .map(ibis_db::ids::record_id_to_plain)
                                .unwrap_or_default(),
                            regex: re,
                            format_str,
                            source_file: src_file,
                            source_line: src_line,
                        });
                    }
                }
                analyzer.static_patterns = Mutex::new(statics);
            }
        }

        analyzer
    }

    /// Reports progress through the attached reporter (no-op when absent).
    pub(crate) fn report_progress(&self, done: u64, total: u64, message: &str) {
        if let Some(p) = &self.progress {
            p.report(done, total, message);
        }
    }

    /// Fire-and-forget batch processing.
    pub async fn process_batch(&self, lines: &[String]) {
        let _ = self.process_batch_sync(lines).await;
    }

    /// Synchronous batch processing: prefilters noise, chunks the batch, and
    /// accumulates anomalies across chunks. Known
    /// patterns learned in an earlier chunk short-circuit later chunks.
    ///
    /// Returns `(anomalies, error)` likewise: partial anomalies collected from
    /// successful chunks ride along when a later chunk fails.
    pub async fn process_batch_sync(
        &self,
        lines: &[String],
    ) -> (Vec<SemanticError>, Option<LogsError>) {
        if lines.is_empty() {
            return (Vec::new(), None);
        }
        self.ast_rule_gen_used.store(false, Ordering::SeqCst);

        let (lines, _dropped) = prefilter_noise_lines(lines);
        if lines.is_empty() {
            return (Vec::new(), None);
        }

        let chunk_size = DEFAULT_BATCH_CHUNK_SIZE;
        let n_chunks = lines.len().div_ceil(chunk_size);
        self.report_progress(0, n_chunks as u64, "Analyzing log batch");
        if n_chunks <= 1 {
            let (out, err) = self.process_batch_chunk(&lines, 1, 1).await;
            self.report_progress(1, 1, "Log analysis complete");
            return (out, err);
        }

        let mut all: Vec<SemanticError> = Vec::new();
        let mut first_err: Option<LogsError> = None;
        for (chunk_idx, chunk) in lines.chunks(chunk_size).enumerate() {
            let chunk_idx = chunk_idx + 1;
            let (part, err) = self
                .process_batch_chunk(chunk, chunk_idx as u64, n_chunks as u64)
                .await;
            // Scenario: appends partial results even when the chunk errored.
            if !part.is_empty() {
                all.extend(part);
            }
            if err.is_some() && first_err.is_none() {
                first_err = err;
                // Continue remaining chunks so known-pattern
                // accumulation still helps later slices.
            }
            self.report_progress(
                chunk_idx as u64,
                n_chunks as u64,
                &format!("Log analysis chunk {chunk_idx}/{n_chunks}"),
            );
        }
        (all, first_err)
    }

    /// Runs LogAlign → known-filter → optional AI for one line slice
    /// Returns `(anomalies, error)`: the static-phase
    /// anomalies are preserved even when the AI stage fails (parity).
    async fn process_batch_chunk(
        &self,
        lines: &[String],
        chunk_index: u64,
        chunk_total: u64,
    ) -> (Vec<SemanticError>, Option<LogsError>) {
        if lines.is_empty() {
            return (Vec::new(), None);
        }

        let mut results: Vec<SemanticError> = Vec::new();
        let mut persist_failures: Vec<String> = Vec::new();

        // Phase 1: deterministic LogAlign matching (AST/static templates —
        // bypasses AI). Prefer longer format strings when multiple patterns
        // match (avoids weak leftovers); the first of equal lengths wins
        // (`n > bestLen keeps the earliest).
        let mut unmatched_lines: Vec<String> = Vec::new();
        for line in lines {
            // Lock scope: the guard must never live across an await.
            let best: Option<LogTemplatePattern> = {
                let statics = self.static_patterns.lock().unwrap();
                let mut best: Option<&LogTemplatePattern> = None;
                let mut best_len = 0usize;
                for sp in statics.iter() {
                    if !sp.regex.is_match(line) {
                        continue;
                    }
                    let n = sp.format_str.len();
                    if best.is_none() || n > best_len {
                        best_len = n;
                        best = Some(sp);
                    }
                }
                best.cloned()
            };
            let mut matched = false;
            if let Some(sp) = best {
                let se = SemanticError {
                    category: format!("Static Log: {}", sp.format_str),
                    stack_trace: line.clone(),
                    file: sp.source_file.clone(),
                    line: sp.source_line,
                    severity: 5,
                    template: sp.format_str.clone(),
                    cause: format!(
                        "Matched LogAlign template from {}:{}.",
                        display_or(&sp.source_file, "unknown"),
                        sp.source_line
                    ),
                    ..Default::default()
                };
                if let Err(persist_err) = self
                    .ingest_error(
                        &se.category,
                        &se.stack_trace,
                        &se.file,
                        se.severity,
                        &se.template,
                        &[],
                    )
                    .await
                {
                    persist_failures.push(format!("{}: {persist_err}", se.category));
                }
                results.push(se);
                matched = true;
            }
            if !matched {
                unmatched_lines.push(line.clone());
            }
        }

        if unmatched_lines.is_empty() {
            let enriched = self.enrich_anomalies(results).await;
            return (enriched, None);
        }

        // Phase 2: known-pattern filter, then AI only if novel text remains.
        let text = self.filter_known_errors(&unmatched_lines.join("\n"));
        let marker_counts = count_known_error_markers(&text);

        if is_known_markers_only(&text) {
            let aggregated = aggregate_known_markers(&marker_counts);
            if !results.is_empty() {
                results = self.enrich_anomalies(results).await;
            }
            results.extend(aggregated);
            return (results, None);
        }

        let active_categories = {
            let known = self.known_patterns.lock().unwrap();
            known
                .iter()
                .map(|p| p.category.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };

        let baseline = crate::enrich::resolve_baseline(self).await;
        let repo_hints = crate::enrich::gather_repo_hints(self, &unmatched_lines).await;

        let prompt = build_analyze_prompt(
            &self.project,
            &baseline.to_string(),
            &active_categories,
            display_or(&repo_hints, "(none)"),
            &text,
        );

        let contents = [Content {
            role: String::new(),
            parts: vec![Part { text: prompt }],
        }];
        let cfg = GenerationConfig {
            temperature: 0.1,
            route_hint: ANALYZER_ROUTE_HINT.to_string(),
            ..Default::default()
        };

        let Some(ai) = &self.ai else {
            return (
                results,
                Some(LogsError::other("AI client is not functional")),
            );
        };
        if !ai.is_functional() {
            return (
                results,
                Some(LogsError::other("AI client is not functional")),
            );
        }

        let candidate = match ai.generate_content(&contents, cfg).await {
            Ok(c) => c,
            // ContextOnly (and any other AI failure) degrades: partial results
            // ride along with the error and the pipeline continues with the
            // next chunk.
            Err(e) => {
                if e.is_context_only() {
                    tracing::warn!(
                        "AI reasoning unavailable (context-only) for {}: {e}",
                        self.path
                    );
                } else {
                    tracing::error!("AI Generation error for {}: {e}", self.path);
                }
                return (results, Some(LogsError::Ai(e)));
            }
        };

        if let Some(usage) = &candidate.usage_metadata {
            self.cost_tracker
                .record_usage(
                    &self.log_file_id,
                    usage.prompt_token_count.max(0) as u64,
                    usage.candidates_token_count.max(0) as u64,
                    usage.cached_tokens(),
                )
                .await;
        }

        let Some(first_text) = candidate.content.parts.first().map(|p| p.text.clone()) else {
            return (results, Some(LogsError::other("empty AI response")));
        };
        let clean_json = strip_code_fences(first_text.trim());
        let mut errors: Vec<SemanticError> = match serde_json::from_str(&clean_json) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::debug!("Failed to parse AI JSON for {}: {e}", self.path);
                return (
                    results,
                    Some(LogsError::other(format!("failed to parse AI JSON: {e}"))),
                );
            }
        };

        if errors.is_empty() {
            if !results.is_empty() {
                results = self.enrich_anomalies(results).await;
            }
            return (results, None);
        }
        tracing::info!(
            "Detected {} semantic errors in {} (chunk {chunk_index}/{chunk_total})",
            errors.len(),
            self.path
        );

        errors = self.enrich_anomalies(errors).await;

        // Static results currently occupy the results prefix; the count is
        // captured before AI errors are appended.
        let static_count_before_ai = results.len();

        // Path C: promote AI templates into durable log_template /
        // staticPatterns so subsequent lines/chunks LogAlign-bypass classify
        // AI (AST remains primary).
        let _ = self.promote_ai_errors_to_log_align(&errors).await;

        // Path D: optionally synthesize ast-grep *-logs.yml rules
        // (cost-bounded). Full LogAlign source_file/line effect typically
        // requires a later ingest_code.
        if self.should_attempt_ast_rule_gen(&errors) {
            let _ = self.generate_ast_rules_from_errors(&errors).await;
        }

        for e in errors {
            if !e.template.is_empty() {
                let _ = self.add_known_pattern(&e.category, &e.template);
            }
            // C2: a DB persist failure must not be interpreted as a
            // successful analysis — the batch reports it (the tailer retries
            // instead of advancing past the batch).
            if let Err(persist_err) = self
                .ingest_error(
                    &e.category,
                    &e.stack_trace,
                    &e.file,
                    e.severity,
                    &e.template,
                    &e.supersedes,
                )
                .await
            {
                persist_failures.push(format!("{}: {persist_err}", e.category));
            }
            results.push(e);
        }

        // Scenario: re-enriches the static prefix (results[:nStatic]) after appending
        // the AI errors — re-resolution only fills still-missing fields.
        let n_static = static_count_before_ai;
        if n_static > 0 {
            let static_part = self.enrich_anomalies(results[..n_static].to_vec()).await;
            let mut merged = static_part;
            merged.extend_from_slice(&results[n_static..]);
            results = merged;
        }

        let err = if persist_failures.is_empty() {
            None
        } else {
            Some(LogsError::other(format!(
                "anomaly persist failed: {}",
                persist_failures.join("; ")
            )))
        };
        (results, err)
    }

    /// Replaces every known-pattern match with a `[KNOWN_ERROR: category]`
    /// marker.
    pub fn filter_known_errors(&self, text: &str) -> String {
        let mut text = text.to_string();
        let known = self.known_patterns.lock().unwrap().clone();
        for pattern in &known {
            let marker = format!("[KNOWN_ERROR: {}]", pattern.category);
            text = pattern
                .regex
                .replace_all(&text, marker.as_str())
                .into_owned();
        }
        text
    }

    /// Registers a known-error pattern compiled from an AI `<VAR>` mask
    /// template.
    pub fn add_known_pattern(&self, category: &str, template: &str) -> Result<()> {
        let pattern_str = mask_template_to_regex(template);
        let regex = compile_dotall(&pattern_str).map_err(|e| {
            tracing::error!(
                "Failed to compile regex from AI template for category {}: {e}",
                category
            );
            LogsError::other(format!("failed to compile regex from AI template: {e}"))
        })?;

        self.known_patterns.lock().unwrap().push(Pattern {
            category: category.to_string(),
            regex,
        });
        Ok(())
    }

    /// Asks the AI for the log-entry-beginning regex;
    /// returns "" when AI is absent/functional-no or on any failure.
    pub async fn discover_format(&self, lines: &[String]) -> String {
        let Some(ai) = &self.ai else {
            return String::new();
        };
        if !ai.is_functional() || lines.is_empty() {
            return String::new();
        }

        let sample = lines.join("\n");
        // D13 prompt stability: byte-stable instructions, the log sample —
        // the query — last.
        let prompt = format!("{DISCOVER_FORMAT_INSTRUCTIONS}Log sample:\n{sample}");

        let contents = [Content {
            role: String::new(),
            parts: vec![Part { text: prompt }],
        }];
        let cfg = GenerationConfig {
            temperature: 0.0,
            route_hint: ANALYZER_ROUTE_HINT.to_string(),
            ..Default::default()
        };

        let Ok(candidate) = ai.generate_content(&contents, cfg).await else {
            return String::new();
        };

        let Some(text) = candidate.content.parts.first().map(|p| p.text.clone()) else {
            return String::new();
        };
        text.trim()
            .trim_start_matches('`')
            .trim_end_matches('`')
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string()
    }

    /// Upserts an `error_type` row + `log_entry` edge + embedding + related
    /// file edge. Bound variables everywhere (F-002).
    ///
    /// C2/F09: persist failures propagate to the caller — a DB error must not
    /// be interpreted as a successful analysis (the tailer then retries the
    /// batch instead of advancing past it).
    pub(crate) async fn ingest_error(
        &self,
        category: &str,
        stack: &str,
        file: &str,
        severity: i64,
        template: &str,
        supersedes: &[String],
    ) -> Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };

        let msg = format!("{category}{stack}");
        if msg.is_empty() {
            return Ok(());
        }
        let hash: String = Sha256::digest(msg.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let error_type_id = format_record_id(schema::ERROR_TYPE, &hash);
        let nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| chrono::Utc::now().timestamp_micros() * 1_000);
        let log_entry_id = format_record_id(schema::LOG_ENTRY, &format!("{hash}_{nanos}"));

        let timestamp = chrono::Utc::now();
        let sql = format!(
            "UPSERT {error_type_id} SET hash = $hash, category = $category, stack_trace = $stack, severity = $severity, template = $template, created_at = $created_at, hidden = false;"
        );
        let result = db
            .smart_query(
                &sql,
                vars([
                    ("hash", ibis_db::value::Value::from_t(hash.as_str())),
                    ("category", ibis_db::value::Value::from_t(category)),
                    ("stack", ibis_db::value::Value::from_t(stack)),
                    ("severity", ibis_db::value::Value::from_t(severity)),
                    ("template", ibis_db::value::Value::from_t(template)),
                    (
                        "created_at",
                        ibis_db::value::Value::Datetime(surrealdb::types::Datetime::from(
                            timestamp,
                        )),
                    ),
                ]),
            )
            .await;
        if let Err(e) = result {
            tracing::error!("Failed to upsert ErrorType: {e}");
            return Err(LogsError::other(format!("error_type upsert failed: {e}")));
        }

        for sup in supersedes {
            let hide_sql = format!(
                "UPDATE {} SET hidden = true WHERE category = $category;",
                schema::ERROR_TYPE
            );
            if let Err(e) = db
                .smart_query(&hide_sql, vars([("category", sup.as_str())]))
                .await
            {
                tracing::warn!("Failed to supersede error category '{sup}': {e}");
            }
        }

        // SurrealDB 3.x RELATE no longer implicit-creates the edge endpoints
        // (legacy 2.x behavior): the log_entry record is persisted explicitly.
        if let Err(e) = db
            .execute(&format!(
                "UPSERT {log_entry_id} SET created_at = time::now();"
            ))
            .await
        {
            tracing::warn!("Failed to persist log entry: {e}");
            return Err(LogsError::other(format!("log_entry persist failed: {e}")));
        }

        if let Err(e) = db
            .execute(&format!(
                "RELATE {}->{}->{};",
                log_entry_id,
                schema::IS_TYPE_OF,
                error_type_id
            ))
            .await
        {
            tracing::warn!("Failed to relate log entry: {e}");
            return Err(LogsError::other(format!("log_entry relate failed: {e}")));
        }

        let embedding = match &self.ai {
            Some(ai) => ai.embed_text(&format!("{category}\n{stack}")).await.ok(),
            None => None,
        };
        if let Some(vec) = embedding.filter(|v| !v.is_empty()) {
            let sql = format!("UPSERT {error_type_id} SET embedding = $vec;");
            if let Err(e) = db
                .smart_query(&sql, vars([("vec", ibis_db::value::Embedding(vec))]))
                .await
            {
                tracing::warn!("Failed to store error embedding: {e}");
            }
        }

        if !file.is_empty() {
            // F09: the anomaly's file link must point at the SAME canonical
            // `source_file:[repo, file_path]` record code/git ingest writes —
            // the old scalar `source_file:⟨sanitized⟩` id is rejected by the
            // typed id field and siloed log errors away from the code graph.
            let repo_ref = format_record_id(schema::REPO, &sanitize_id(&self.project));
            let file_id = ibis_db::ids::source_file_id(&repo_ref, file);
            // 3.x: endpoints must exist — persist the source_file record with
            // its rel_path and repo_name (recent_failures filters the
            // related_to traversal on repo_name) before linking
            // ( 2.x implicit-created it empty).
            if let Err(e) = db
                .smart_query(
                    &format!("UPSERT {file_id} SET rel_path = $rel_path, repo_name = $repo_name;"),
                    vars([("rel_path", file), ("repo_name", self.project.as_str())]),
                )
                .await
            {
                tracing::warn!("Failed to persist source file: {e}");
            }
            if let Err(e) = db
                .execute(&format!(
                    "RELATE {}->{}->{};",
                    error_type_id,
                    schema::RELATED_TO,
                    file_id
                ))
                .await
            {
                tracing::warn!("Failed to relate error to file: {e}");
                return Err(LogsError::other(format!("related_to file failed: {e}")));
            }
        }
        Ok(())
    }

    /// Persists the tail resume offset on the log_file record.
    pub async fn update_offset(&self, offset: i64) {
        let Some(db) = &self.db else {
            return;
        };
        let sql = format!("UPSERT {} SET offset = $offset;", self.log_file_id);
        if let Err(e) = db.smart_query(&sql, vars([("offset", offset)])).await {
            tracing::error!("Failed to update log offset: {e}");
        }
    }

    /// Reads the persisted tail resume offset, 0 when absent.
    pub async fn get_offset(&self) -> i64 {
        let Some(db) = &self.db else {
            return 0;
        };
        let sql = format!("SELECT offset FROM {};", self.log_file_id);
        let Ok(outcome) = db.execute(&sql).await else {
            return 0;
        };
        let rows = outcome.rows::<OffsetRow>(&sql).unwrap_or_default();
        rows.first()
            .and_then(|r| r.offset)
            .map(|v| v.get())
            .unwrap_or(0)
    }
}

/// Compiles a regex with the `(?s) single-line flag (dot matches newline).
pub(crate) fn compile_dotall(pattern: &str) -> Result<regex::Regex> {
    regex::Regex::new(&format!("(?s){pattern}"))
        .map_err(|e| LogsError::other(format!("regex compile failed: {e}")))
}

/// Strips markdown code fences around an AI JSON reply ( in both
/// `processBatchChunk` and `parseAIASTRuleJSON`).
pub(crate) fn strip_code_fences(raw: &str) -> String {
    let mut clean = raw.trim().to_string();
    if clean.starts_with("```") {
        if let Some(idx) = clean.find('\n') {
            clean = clean[idx + 1..].to_string();
        }
    }
    clean = clean.trim().to_string();
    if clean
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .ends_with("```")
    {
        clean = clean.trim_end_matches([' ', '\t', '\r', '\n']).to_string();
        clean = clean[..clean.len() - 3].to_string();
    }
    clean.trim().to_string()
}

/// `displayOr` helper : falls back when blank.
pub(crate) fn display_or<'a>(v: &'a str, fallback: &'a str) -> &'a str {
    if v.trim().is_empty() {
        fallback
    } else {
        v
    }
}

/// Matches markers injected by `filter_known_errors`.
fn known_error_marker_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[KNOWN_ERROR:\s*([^\]]+)\]").expect("static regex"))
}

/// Log-level tokens that usually indicate non-error chatter.
fn noise_level_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:^|[\[\s|/])(INFO|DEBUG|TRACE|VERBOSE)(?:[\]\s:/=]|$)")
            .expect("static regex")
    })
}

/// Keep the line if any of these appear (conservative; `signalLevelRE).
fn signal_level_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:ERROR|ERR\b|WARN(?:ING)?|FATAL|PANIC|EXCEPTION|CRITICAL|SEVERE|FAIL(?:URE|ED)?)",
        )
        .expect("static regex")
    })
}

/// Drops obvious non-error noise before AI.
///
/// Heuristic (conservative — prefer keeping a line when unsure): drop only when
/// the line has an INFO/DEBUG/TRACE/VERBOSE level token AND does not contain
/// ERROR/WARN/FATAL/panic/exception/critical/fail signals. Stack frames, plain
/// messages without a level, and mixed "INFO … exception" lines are kept.
pub fn prefilter_noise_lines(lines: &[String]) -> (Vec<String>, usize) {
    let mut kept = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for line in lines {
        if is_obvious_noise_line(line) {
            dropped += 1;
            continue;
        }
        kept.push(line.clone());
    }
    (kept, dropped)
}

fn is_obvious_noise_line(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }
    if signal_level_re().is_match(line) {
        return false;
    }
    noise_level_re().is_match(line)
}

/// Reports whether text is only `[KNOWN_ERROR:…]` markers and whitespace.
pub fn is_known_markers_only(text: &str) -> bool {
    let remaining = known_error_marker_re().replace_all(text, "");
    remaining.trim().is_empty()
}

/// Returns occurrence counts keyed by category name.
pub fn count_known_error_markers(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for m in known_error_marker_re().captures_iter(text) {
        let Some(cat) = m.get(1).map(|c| c.as_str().trim()) else {
            continue;
        };
        if cat.is_empty() {
            continue;
        }
        *counts.entry(cat.to_string()).or_insert(0) += 1;
    }
    counts
}

/// Builds report rows from marker counts without calling AI
///
fn aggregate_known_markers(counts: &HashMap<String, usize>) -> Vec<SemanticError> {
    if counts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(counts.len());
    for (cat, n) in counts {
        out.push(SemanticError {
            category: cat.clone(),
            stack_trace: format!("[KNOWN_ERROR: {cat}] x{n}"),
            severity: 5,
            cause: format!("Aggregated {n} known occurrences; AI skipped (known markers only)."),
            ..Default::default()
        });
    }
    out
}

/// Reads up to `max_bytes` from an async reader. If the stream continues past
/// `max_bytes`, truncated is true and the returned slice holds only the capped
/// prefix (`ReadCapped; `max_bytes <= 0` → [`MAX_LOG_INGEST_BYTES`]).
pub async fn read_capped(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    max_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let max = if max_bytes == 0 {
        MAX_LOG_INGEST_BYTES
    } else {
        max_bytes
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut chunk).await?;
        if n == 0 {
            return Ok((buf, false));
        }
        let remaining = max.saturating_sub(buf.len());
        if n <= remaining {
            buf.extend_from_slice(&chunk[..n]);
        } else {
            buf.extend_from_slice(&chunk[..remaining]);
            // Drain the rest without buffering it (to detect truncation).
            let extra = n - remaining;
            let _ = extra;
            return Ok((buf, true));
        }
    }
}

/// Converts an AI `<VAR>` mask template into a RE2 fragment
/// Trailing `<VAR>` uses `[^\n]*` so the dynamic
/// suffix is consumed (`.*?` at the end would match empty and leave residue,
/// defeating known-marker early-exit).
pub(crate) fn mask_template_to_regex(template: &str) -> String {
    let parts: Vec<&str> = template.split("<VAR>").collect();
    let mut b = String::new();
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        b.push_str(&regex::escape(part));
        if i < last {
            if i == last.saturating_sub(1) && parts[last].is_empty() {
                b.push_str(r"[^\n]*");
            } else {
                b.push_str(r".*?");
            }
        }
    }
    b
}

/// D13 prompt stability: byte-stable instructions block for the semantic
/// log-analysis call site. Everything dynamic (project, baseline, known
/// categories, repo hints) follows this block, and the log text — the query —
/// goes last. No timestamp or request-id may ever enter this const.
const ANALYZE_INSTRUCTIONS: &str = r#"Analyze the log text provided at the end.
Extract distinct errors or anomalies — classify AND pinpoint likely source location / root cause using repo context when available.

For each error, provide:
1. "category": a normalized, general description of the error (ignore specific IDs/timestamps)
2. "stack_trace": the relevant stack trace or context
3. "file": repo-relative source path when known (from stack frames OR repo hints). Prefer paths like src/db/client.rs over bare basenames. Empty string only if truly unknown.
4. "line": integer source line when known, else 0
5. "symbol": function/method/symbol name when known, else ""
6. "cause": one short sentence explaining the likely root cause grounded in the log + repo hints (not just restating the category)
7. "severity": a number from 1 to 10 (10 being critical)
8. "template": The exact raw text of the entire error block (including cascade logs), but replace any dynamic/variable parts (like timestamps, specific IDs, memory addresses) with the literal string "<VAR>". Do not use regex.
9. "supersedes": a string array of known categories that this new error incorporates or replaces (e.g., if this is a cascade that contains them).
Note: Previously known errors have been replaced with markers like [KNOWN_ERROR: category]. If you see these markers, understand that the corresponding error occurred there.
Format your response purely as a JSON array of objects with the above keys. No markdown blocks.
"#;

/// Assembles the semantic-analysis prompt: byte-stable instructions →
/// per-call variable context → log text (the query) last (D13).
fn build_analyze_prompt(
    project: &str,
    baseline: &str,
    active_categories: &str,
    repo_hints: &str,
    text: &str,
) -> String {
    let mut p = String::from(ANALYZE_INSTRUCTIONS);
    p.push_str("Project: ");
    p.push_str(project);
    p.push_str("\nCode baseline for correlation: ");
    p.push_str(baseline);
    p.push_str("\nCurrently known error categories for this project: [");
    p.push_str(active_categories);
    p.push_str(
        "]. Use this context to build incremental knowledge.\nRepo knowledge hints (may be empty):\n",
    );
    p.push_str(repo_hints);
    p.push_str("\n\nLog text:\n");
    p.push_str(text);
    p
}

/// D13 prompt stability: byte-stable instructions block for the log-format
/// discovery call site; the log sample — the query — is appended last.
const DISCOVER_FORMAT_INSTRUCTIONS: &str = r#"Analyze the following log sample.
What is the regular expression (RE2) that uniquely identifies the absolute BEGINNING of a new log entry (e.g. a timestamp or log level)?
Return ONLY the raw regex string, nothing else. If there is no clear delimiter, return an empty string.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;

    /// D10 contract: analyzer and format discovery declare tier M.
    #[test]
    fn analyzer_route_hint_is_local_m() {
        assert_eq!(ANALYZER_ROUTE_HINT, "local:M");
    }

    /// Capturing executor for the C2/F09 canonical-id contract: records the
    /// SQL it receives and optionally fails matching statements.
    struct CapturingDb {
        sqls: std::sync::Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    impl CapturingDb {
        fn new() -> Self {
            CapturingDb {
                sqls: std::sync::Mutex::new(Vec::new()),
                fail_on: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ibis_db::executor::Executor for CapturingDb {
        async fn execute(
            &self,
            sql: &str,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            if let Some(fail) = &self.fail_on {
                if sql.contains(fail.as_str()) {
                    return Err(ibis_db::DbError::Query {
                        message: format!("injected failure on {fail}"),
                        duration_ms: 0,
                    });
                }
            }
            Ok(ibis_db::executor::QueryOutcome::Single(
                ibis_db::value::Value::None,
            ))
        }

        async fn smart_query(
            &self,
            sql: &str,
            _v: ibis_db::executor::Vars,
        ) -> std::result::Result<ibis_db::executor::QueryOutcome, ibis_db::DbError> {
            self.execute(sql).await
        }
    }

    /// C2/F09: the anomaly's file link uses the canonical
    /// `source_file:[repo, file_path]` id — the same record code/git ingest
    /// writes — with rel_path AND repo_name set, and the RELATE targets it.
    #[tokio::test]
    async fn ingest_error_links_canonical_source_file() {
        let db = Arc::new(CapturingDb::new());
        let mut analyzer = LogAnalyzer::bare("proj-with spaces");
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);

        analyzer
            .ingest_error("CAT", "stack", "src/lib.rs", 5, "template", &[])
            .await
            .expect("persist");

        let sqls = db.sqls.lock().unwrap();
        let want = ibis_db::ids::source_file_id("repo:⟨proj_with_spaces⟩", "src/lib.rs");
        let upsert = sqls
            .iter()
            .find(|s| s.contains("UPSERT") && s.contains("rel_path"))
            .expect("source_file upsert");
        assert!(
            upsert.contains(&want),
            "canonical array id expected {want}, got: {upsert}"
        );
        assert!(
            upsert.contains("repo_name"),
            "repo_name must be set (recent_failures traversal filter): {upsert}"
        );
        assert!(
            !sqls.iter().any(|s| s.contains("source_file:⟨src_lib_rs⟩")),
            "no scalar legacy id may be written: {sqls:?}"
        );
        let relate = sqls
            .iter()
            .find(|s| s.contains(&format!("->{}->", schema::RELATED_TO)))
            .expect("related_to relate");
        assert!(
            relate.contains(&want),
            "related_to must point at the canonical record: {relate}"
        );
    }

    /// C2: paths that collided under `sanitize_id` coexist as distinct
    /// canonical records.
    #[tokio::test]
    async fn ingest_error_collision_free_paths() {
        let db = Arc::new(CapturingDb::new());
        let mut analyzer = LogAnalyzer::bare("p");
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);

        for file in [
            "src/a-b.rs",
            "src/a/b.rs",
            "Foo.rs",
            "foo.rs",
            "with space's.rs",
        ] {
            analyzer
                .ingest_error("CAT", "stack", file, 5, "template", &[])
                .await
                .expect("persist");
        }
        let sqls = db.sqls.lock().unwrap();
        for file in [
            "src/a-b.rs",
            "src/a/b.rs",
            "Foo.rs",
            "foo.rs",
            "with space's.rs",
        ] {
            let want = ibis_db::ids::source_file_id("repo:⟨p⟩", file);
            assert!(
                sqls.iter().any(|s| s.contains(&want)),
                "canonical id for {file} expected: {want}"
            );
        }
        // Distinct canonical ids (no collapse onto one sanitized suffix).
        let a_b = ibis_db::ids::source_file_id("repo:⟨p⟩", "src/a-b.rs");
        let a_slash_b = ibis_db::ids::source_file_id("repo:⟨p⟩", "src/a/b.rs");
        assert_ne!(a_b, a_slash_b);
    }

    /// C2: a DB persist failure surfaces as the batch error — the analysis
    /// must not be reported as successful when the write failed.
    #[tokio::test]
    async fn ingest_error_failure_surfaces_as_batch_error() {
        let mut db = Arc::new(CapturingDb::new());
        Arc::get_mut(&mut db).unwrap().fail_on = Some("UPSERT error_type".to_string());
        let mut analyzer = LogAnalyzer::bare("p");
        analyzer.db = Some(db.clone() as Arc<dyn ibis_db::executor::Executor>);

        let result = analyzer
            .ingest_error("CAT", "stack", "src/lib.rs", 5, "template", &[])
            .await;
        assert!(result.is_err(), "ingest_error must propagate the failure");

        // End-to-end: process_batch_sync reports the persist failure (AI path —
        // the static LogAlign path is promoted patterns only).
        let mut err_db = Arc::new(CapturingDb::new());
        Arc::get_mut(&mut err_db).unwrap().fail_on = Some("UPSERT error_type".to_string());
        let mut err_analyzer = LogAnalyzer::bare("p");
        err_analyzer.db = Some(err_db as Arc<dyn ibis_db::executor::Executor>);
        let (ai, _calls) = counting_ai(
            r#"[{"category":"CAT","stack_trace":"stack","file":"src/lib.rs","line":1,"symbol":"","cause":"c","severity":5,"template":"t","supersedes":[]}]"#,
        );
        err_analyzer.ai = Some(ai);
        let (results, err) = err_analyzer
            .process_batch_sync(&["ERROR thing 42".to_string()])
            .await;
        assert!(!results.is_empty(), "anomalies still surface: {results:?}");
        assert!(
            err.is_some(),
            "a failed persist must surface as the batch error: {err:?}"
        );
    }

    struct CountingReporter {
        calls: AtomicUsize,
    }
    impl ProgressReporter for CountingReporter {
        fn report(&self, _done: u64, _total: u64, _message: &str) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Counting fake AI provider:
    /// functional on both halves, replies `reply` and counts GenerateContent
    /// calls. Fake stays fake — no network.
    struct CountingAI {
        calls: std::sync::Arc<AtomicUsize>,
        reply: String,
    }

    #[async_trait::async_trait]
    impl ibis_ai::AIProvider for CountingAI {
        fn name(&self) -> &str {
            "counting-ai"
        }
        async fn is_functional(&self) -> bool {
            true
        }
        async fn generate_content(
            &self,
            _contents: &[ibis_ai::types::Content],
            _config: ibis_ai::types::GenerationConfig,
        ) -> std::result::Result<ibis_ai::types::Candidate, ibis_ai::AiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ibis_ai::types::Candidate {
                content: ibis_ai::types::Content {
                    parts: vec![ibis_ai::types::Part {
                        text: self.reply.clone(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            })
        }
    }

    fn counting_ai(reply: &str) -> (Arc<Client>, std::sync::Arc<AtomicUsize>) {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingAI {
            calls: calls.clone(),
            reply: reply.to_string(),
        }) as Arc<dyn ibis_ai::AIProvider>;
        (
            Arc::new(Client::new(Some(provider.clone()), Some(provider), None)),
            calls,
        )
    }

    #[test]
    fn add_known_pattern_multiline_vars() {
        let analyzer = LogAnalyzer::bare("t");

        let category = "DB_TIMEOUT";
        let template = "2026-04-30 10:00:00 ERROR Database timeout\nat db.Connect()\nuser <VAR> failed to login from IP <VAR>";

        analyzer
            .add_known_pattern(category, template)
            .expect("no error");

        let regex = {
            let known = analyzer.known_patterns.lock().unwrap();
            assert_eq!(known.len(), 1, "expected 1 pattern");
            known[0].regex.clone()
        };

        let test_match = "2026-04-30 10:00:00 ERROR Database timeout\nat db.Connect()\nuser john_doe failed to login from IP 192.168.1.1";
        assert!(
            regex.is_match(test_match),
            "Expected regex to match string with variables replaced"
        );

        let test_match_multiline_var = "2026-04-30 10:00:00 ERROR Database timeout\nat db.Connect()\nuser admin\nsuperuser failed to login from IP 10.0.0.1";
        assert!(
            regex.is_match(test_match_multiline_var),
            "Expected regex to match string with multiline variables replaced"
        );
    }

    #[test]
    fn filter_known_errors_replaces_blocks_with_markers() {
        let analyzer = LogAnalyzer::bare("t");

        analyzer
            .add_known_pattern(
                "DB_TIMEOUT",
                "ERROR Database timeout\nat db.Connect()\nuser <VAR>",
            )
            .unwrap();
        analyzer
            .add_known_pattern("NULL_PTR", "NullReferenceException\nat main.go:10")
            .unwrap();

        let log_batch = "INFO System starting up...\nERROR Database timeout\nat db.Connect()\nuser alice\nINFO System running\nNullReferenceException\nat main.go:10\nINFO Request finished";

        let filtered = analyzer.filter_known_errors(log_batch);

        assert!(
            !filtered.contains("ERROR Database timeout"),
            "Expected DB_TIMEOUT error block to be filtered out"
        );
        assert!(
            !filtered.contains("user alice"),
            "Expected DB_TIMEOUT variable parts to be filtered out"
        );
        assert!(
            !filtered.contains("NullReferenceException"),
            "Expected NULL_PTR error block to be filtered out"
        );
        assert!(
            filtered.contains("[KNOWN_ERROR: DB_TIMEOUT]"),
            "Expected marker [KNOWN_ERROR: DB_TIMEOUT] to be injected"
        );
        assert!(
            filtered.contains("[KNOWN_ERROR: NULL_PTR]"),
            "Expected marker [KNOWN_ERROR: NULL_PTR] to be injected"
        );

        for expected in [
            "INFO System starting up...",
            "INFO System running",
            "INFO Request finished",
        ] {
            assert!(
                filtered.contains(expected),
                "Expected filtered text to retain: {expected}"
            );
        }
    }

    #[test]
    fn filter_known_errors_all_matched() {
        let analyzer = LogAnalyzer::bare("t");
        analyzer
            .add_known_pattern("SPAM_ERROR", "<VAR> SPAM")
            .unwrap();

        let filtered = analyzer.filter_known_errors("10:00 SPAM\n10:01 SPAM\n10:02 SPAM");
        assert!(
            filtered.contains("[KNOWN_ERROR: SPAM_ERROR]"),
            "Expected markers to be injected, got: {filtered:?}"
        );
    }

    /// `TestLexerParserOrdering: an AI cascade template that embeds the
    /// literal marker of an earlier category must subsume it (filtering runs
    /// in registration order, so the cascade matches after TOKEN_A was masked).
    #[test]
    fn lexer_parser_ordering_cascade_subsumes_marker() {
        let analyzer = LogAnalyzer::bare("t");
        analyzer.add_known_pattern("TOKEN_A", "Error A").unwrap();
        analyzer
            .add_known_pattern("CASCADE_C", "[KNOWN_ERROR: TOKEN_A]\nConsequence B")
            .unwrap();

        let filtered = analyzer.filter_known_errors("Error A\nConsequence B");

        assert!(
            !filtered.contains("[KNOWN_ERROR: TOKEN_A]"),
            "Expected TOKEN_A marker to be subsumed by CASCADE_C marker"
        );
        assert!(
            filtered.contains("[KNOWN_ERROR: CASCADE_C]"),
            "Expected CASCADE_C marker to be present, got: {filtered:?}"
        );
    }

    #[test]
    fn prefilter_noise_lines_keeps_signals() {
        let lines = [
            "2026-09-18 INFO request ok".to_string(),
            "2026-09-18 DEBUG cache hit".to_string(),
            "2026-09-18 ERROR connection refused".to_string(),
            "2026-09-18 INFO failed with exception in handler".to_string(),
            "at src/db/client.rs:42".to_string(),
            "2026-09-18 WARN disk almost full".to_string(),
        ];
        let (kept, dropped) = prefilter_noise_lines(&lines);
        assert_eq!(
            dropped, 2,
            "expected 2 dropped noise lines, got kept={kept:?}"
        );
        let joined = kept.join("\n");
        assert!(
            joined.contains("ERROR connection refused"),
            "ERROR line must be kept"
        );
        assert!(joined.contains("exception"), "INFO+exception must be kept");
        assert!(
            joined.contains("at src/db/client.rs:42"),
            "stack frame without level must be kept"
        );
        assert!(joined.contains("WARN"), "WARN must be kept");
        assert!(
            !joined.contains("cache hit") && !joined.contains("request ok"),
            "pure INFO/DEBUG should be dropped, got: {kept:?}"
        );
    }

    #[test]
    fn is_known_markers_only_pins() {
        assert!(is_known_markers_only(
            "[KNOWN_ERROR: A]\n[KNOWN_ERROR: B]\n  \n"
        ));
        assert!(!is_known_markers_only(
            "[KNOWN_ERROR: A]\nINFO still here\n"
        ));
    }

    /// `TestEarlyExitKnownOnlySkipsAI: fully-masked batches aggregate
    /// without a single GenerateContent call.
    #[tokio::test]
    async fn early_exit_known_only_skips_ai() {
        let (ai, calls) = counting_ai("[]");

        let mut analyzer = LogAnalyzer::bare("test");
        analyzer.ai = Some(ai);
        analyzer
            .add_known_pattern("CONN_REFUSED", "ERROR connection refused host=<VAR>")
            .unwrap();

        let lines = [
            "ERROR connection refused host=db1".to_string(),
            "ERROR connection refused host=db2".to_string(),
            "ERROR connection refused host=db3".to_string(),
        ];
        let (out, err) = analyzer.process_batch_sync(&lines).await;
        assert!(err.is_none(), "ProcessBatchSync: {err:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "expected 0 AI GenerateContent calls"
        );
        assert!(!out.is_empty(), "expected aggregated known-error results");
        assert!(
            out.iter()
                .any(|e| e.category == "CONN_REFUSED" && e.cause.contains("AI skipped")),
            "expected CONN_REFUSED aggregate with AI skipped, got {out:?}"
        );
    }

    /// `TestProcessBatchSyncChunksAndReusesPatterns: 600 lines → 3 chunks;
    /// the first chunk learns the template, the rest early-exit (exactly one
    /// AI call), and known patterns accumulate.
    #[tokio::test]
    async fn process_batch_sync_chunks_and_reuses_patterns() {
        let reply = r#"[{"category":"REPEATED_ERR","stack_trace":"ERROR boom id=1","file":"","line":0,"symbol":"","cause":"test","severity":7,"template":"ERROR boom id=<VAR>","supersedes":[]}]"#;
        let (ai, calls) = counting_ai(reply);

        let lines: Vec<String> = (0..600).map(|i| format!("ERROR boom id={i}")).collect();

        let mut analyzer = LogAnalyzer::bare("chunk-test");
        analyzer.ai = Some(ai);
        let (out, err) = analyzer.process_batch_sync(&lines).await;
        assert!(err.is_none(), "ProcessBatchSync chunked: {err:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected exactly 1 AI call (first chunk learns, rest early-exit); results={}",
            out.len()
        );
        assert!(
            !analyzer.known_patterns.lock().unwrap().is_empty(),
            "expected known pattern accumulated after first chunk"
        );
    }

    #[test]
    fn mask_template_trailing_var_consumes_line() {
        assert_eq!(mask_template_to_regex("a<VAR>"), format!("a{}", r"[^\n]*"));
        assert_eq!(mask_template_to_regex("a<VAR>b"), format!("a{}b", r".*?"));
        assert_eq!(mask_template_to_regex("a.b"), regex::escape("a.b"));
    }

    #[test]
    fn code_fences_stripped() {
        let raw = "```json\n[{\"category\":\"x\"}]\n```";
        assert_eq!(strip_code_fences(raw.trim()), "[{\"category\":\"x\"}]");
        assert_eq!(strip_code_fences("[1]"), "[1]");
    }

    #[test]
    fn aggregate_markers_skip_ai() {
        let mut counts = HashMap::new();
        counts.insert("A".to_string(), 3);
        let rows = aggregate_known_markers(&counts);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stack_trace, "[KNOWN_ERROR: A] x3");
        assert!(rows[0].cause.contains("AI skipped"));
        assert!(aggregate_known_markers(&HashMap::new()).is_empty());
    }

    #[tokio::test]
    async fn read_capped_truncates_and_passes_small_reads() {
        let data = vec![b'x'; 100];
        let (got, trunc) = read_capped(Cursor::new(data), 50).await.unwrap();
        assert!(trunc);
        assert_eq!(got.len(), 50);

        let (got, trunc) = read_capped(Cursor::new(b"abc".to_vec()), 50).await.unwrap();
        assert!(!trunc);
        assert_eq!(got, b"abc");
    }

    #[tokio::test]
    async fn progress_reports_emitted_for_chunks() {
        let reporter = Arc::new(CountingReporter {
            calls: AtomicUsize::new(0),
        });
        let mut analyzer = LogAnalyzer::bare("p");
        analyzer.progress = Some(reporter.clone() as Arc<dyn ProgressReporter>);
        let lines: Vec<String> = (0..300).map(|i| format!("ERROR x{i}")).collect();
        let _ = analyzer.process_batch_sync(&lines).await;
        assert!(reporter.calls.load(Ordering::SeqCst) >= 2);
    }
}

/// D13 prompt stability: two invocations with different contexts must share
/// the byte-identical instruction prefix, and the log text (the query) must
/// stay last.
#[cfg(test)]
mod prompt_stability_tests {
    use super::*;

    #[test]
    fn analyze_prompt_prefix_is_byte_stable_and_query_last() {
        let a = build_analyze_prompt("proj-a", "base-a", "Cat1, Cat2", "hints-a", "text A");
        let b = build_analyze_prompt("proj-b", "base-b", "", "hints-b, more", "text B\nlonger");
        assert_eq!(&a[..ANALYZE_INSTRUCTIONS.len()], ANALYZE_INSTRUCTIONS);
        assert_eq!(&b[..ANALYZE_INSTRUCTIONS.len()], ANALYZE_INSTRUCTIONS);
        assert_ne!(a, b);
        // Query last: the log text is the tail of the prompt.
        assert!(a.ends_with("Log text:\ntext A"));
        assert!(b.ends_with("Log text:\ntext B\nlonger"));
    }

    #[test]
    fn discover_format_prompt_prefix_is_byte_stable() {
        let a = format!("{DISCOVER_FORMAT_INSTRUCTIONS}Log sample:\n2026-01-01 INFO a");
        let b = format!("{DISCOVER_FORMAT_INSTRUCTIONS}Log sample:\n2027-05-05 ERROR b");
        assert_eq!(
            &a[..DISCOVER_FORMAT_INSTRUCTIONS.len()],
            DISCOVER_FORMAT_INSTRUCTIONS
        );
        assert_eq!(
            &b[..DISCOVER_FORMAT_INSTRUCTIONS.len()],
            DISCOVER_FORMAT_INSTRUCTIONS
        );
    }
}
