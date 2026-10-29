//! Polyglot code ingest orchestration
//!
//!
//! Git tree as source of truth: `git ls-tree -r HEAD --name-only` (routed
//! through the credential-injecting runner, D-A07) lists active files, a
//! worker pool processes each one (hash-skip → HEAD marker refresh → chunk
//! persistence → symbol/call edges → log templates), and obsolete files are
//! pruned in 50-id batches. When the git command fails the scan falls back to
//! a directory walk (dot-dirs, configured ignores, unsupported extensions and
//! oversized files filtered out).
//!
//! Parity notes:
//! - **F-002 / F-A01 / F-A02**: every content-bearing value (paths, hashes,
//!   chunk content, symbol names, log formats/regexes) is bound via
//!   `smart_query` variables; only record identifiers and table names are
//!   interpolated, and pruned ids are validated with `is_safe_record_id`
//!   (F-A04).
//! - **F-A03**: chunk rows are created with `CREATE IF NOT EXISTS` and
//!   in-batch symbol nodes with `UPSERT` — a partial resume or a symbol id
//!   colliding with a previous file version no longer fails the whole batch
//!   transaction (the own comment claimed idempotency the SQL lacked).
//! - **F-A18**: `git ls-tree` goes through [`threski_git::GitRunner`] ( used a
//!   raw `exec.Command`, bypassing credential masking).
//! - Cancellation: threaded `context.Context` through every stage; Rust
//!   relies on dropping the future (sg/git children are killed via
//!   `kill_on_drop`), following the wave-1 crate convention.
//! - Files are read once (hash + content from the same buffer) instead of
//!   the hash-then-seek-then-read two-pass; the discovery size filter
//!   (`max_file_size`, default 10 MiB) bounds every read.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use threski_db::executor::{Executor, Vars};
use threski_db::schema::names as t;
use threski_db::value::{RecordId, SurrealValue, I64};
use threski_git::GitRunner;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use walkdir::WalkDir;

use crate::ast_chunker::{AstChunk, CallEdge, LogTemplate};
use crate::error::CodeIngestError;
use crate::ignore::{is_ignored, load_ignore_patterns};
use crate::install::ensure_ast_grep_rules;
use crate::rule_synth::{maybe_synthesize_for_sparse_ast, ContentGenerator};
use crate::sparse_ast::needs_rule_synthesis;

/// Coarse progress reporter for a long ingest call (`progress.Report —
/// the MCP-level throttle lives in the server lane).
pub type ProgressCallback = Arc<dyn Fn(u64, u64, &str) + Send + Sync>;

fn report(progress: Option<&ProgressCallback>, done: u64, total: u64, message: &str) {
    if let Some(p) = progress {
        p(done, total, message);
    }
}

/// The AI surface code ingest needs: embedding functionality
/// plus an optional rule-synthesis generator (`ContentGenerator` type
/// assertion on the same client).
#[async_trait]
pub trait CodeAIClient: Send + Sync {
    /// Sync embedding-half probe.
    fn is_embedding_functional(&self) -> bool;

    /// Rule-synthesis surface; `None` disables sparse-AST rescans
    /// (the `aiClient.(ContentGenerator)` assertion failing).
    fn as_content_generator(&self) -> Option<&dyn ContentGenerator> {
        None
    }
}

/// Adapter wiring [`threski_ai::Client`] into [`CodeAIClient`] (the server lane
/// holds `Client`; passed the same struct for both halves).
pub struct AiClientAdapter(pub Arc<threski_ai::Client>);

#[async_trait]
impl CodeAIClient for AiClientAdapter {
    fn is_embedding_functional(&self) -> bool {
        self.0.is_embedding_functional()
    }

    fn as_content_generator(&self) -> Option<&dyn ContentGenerator> {
        Some(self.0.as_ref())
    }
}

/// Fn-item helper: owned file name from a path (HRTB — N-A14).
fn os_file_name_string(path: &std::ffi::OsStr) -> String {
    path.to_string_lossy().into_owned()
}

/// Scans the repository and updates vector embeddings for any modified or new
/// files.
///
/// `cfg = None` keeps the original semantics: the supported-extension filter is
/// empty, so no file is chunked (only the prune phase still runs) — the
/// server lane always passes a config (F-A12 documents the trap).
pub async fn ingest_codebase(
    db: Arc<dyn Executor>,
    ai: Arc<dyn CodeAIClient>,
    repo_path: &str,
    repo_name: &str,
    cfg: Option<&threski_core::config::Config>,
    progress: Option<ProgressCallback>,
) -> Result<(), CodeIngestError> {
    // Scenario: filepath.Abs: clean absolute path (no symlink resolution).
    let abs_path: PathBuf = if Path::new(repo_path).is_absolute() {
        PathBuf::from(repo_path)
    } else {
        std::env::current_dir()?.join(repo_path)
    };

    tracing::info!("Starting code analysis for: {}", abs_path.display());
    report(progress.as_ref(), 0, 4, "Scanning repository");
    let _ = ensure_ast_grep_rules();

    if !ai.is_embedding_functional() {
        return Err(CodeIngestError::other(
            "AI vectorization is disabled: embedding provider not configured (set embedding.provider and embedding.model in config.json)",
        ));
    }

    // Load ignore patterns (load failures are non-fatal; empty file set → no
    // patterns).
    let ignore_patterns = load_ignore_patterns(&abs_path);

    // Prepare lookup maps from config.
    let supported_exts: HashSet<String> = cfg
        .map(|c| {
            c.supported_extensions
                .iter()
                .map(|e| e.to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let ignored_dirs: HashSet<String> = cfg
        .map(|c| c.ignored_dirs.iter().cloned().collect())
        .unwrap_or_default();
    let ignored_files: HashSet<String> = cfg
        .map(|c| c.ignored_files.iter().cloned().collect())
        .unwrap_or_default();
    let max_size: i64 = match cfg {
        Some(c) if c.max_file_size > 0 => c.max_file_size,
        _ => 10 * 1024 * 1024, // default 10MB
    };
    let num_workers: usize = match cfg {
        Some(c) if c.code_concurrency > 0 => c.code_concurrency.max(1) as usize,
        _ => 5, // safer default to avoid DB overload
    };

    // 3. Get active files via git (D-A07: through the injecting runner).
    let mut active_files: HashSet<String> = HashSet::new();
    let mut pending: Vec<(PathBuf, String)> = Vec::new();

    match GitRunner::anonymous()
        .run(
            abs_path.to_string_lossy().as_ref(),
            &["ls-tree", "-r", "HEAD", "--name-only"],
        )
        .await
    {
        Ok(out) => {
            for rel in String::from_utf8_lossy(&out).lines() {
                let rel = rel.trim();
                if rel.is_empty() {
                    continue;
                }
                let full_path = abs_path.join(rel);
                let Ok(meta) = std::fs::metadata(&full_path) else {
                    continue;
                };
                // Fn-item path (HRTB) — N-A14 (see threski-ingest-git note).
                let name = full_path
                    .file_name()
                    .map(os_file_name_string)
                    .unwrap_or_default();
                if ignored_files.contains(&name)
                    || is_ignored(&full_path, &abs_path, &ignore_patterns, false)
                {
                    continue;
                }
                let ext = crate::ast_chunker::path_ext(&full_path.to_string_lossy()).to_lowercase();
                if !supported_exts.contains(&ext) {
                    continue;
                }
                if i64::try_from(meta.len()).unwrap_or(i64::MAX) > max_size {
                    tracing::debug!(
                        "Skipping large file: {} ({} bytes)",
                        full_path.display(),
                        meta.len()
                    );
                    continue;
                }
                active_files.insert(rel.to_string());
                pending.push((full_path, rel.to_string()));
            }
        }
        Err(git_err) => {
            // Fallback to directory walk.
            tracing::warn!(
                "git ls-tree failed for {} ({git_err}). Falling back to directory walk.",
                abs_path.display()
            );
            walk_active_files(
                &abs_path,
                &ignore_patterns,
                &supported_exts,
                &ignored_dirs,
                &ignored_files,
                max_size,
                &mut active_files,
                &mut pending,
            )?;
        }
    }

    report(progress.as_ref(), 2, 4, "Processing files");

    // Worker pool bounded by `code_concurrency` (default 5).
    let semaphore = Arc::new(Semaphore::new(num_workers));
    let mut tasks: JoinSet<Result<(), CodeIngestError>> = JoinSet::new();
    for (full_path, rel_path) in pending {
        let db = db.clone();
        let ai = ai.clone();
        let repo_name = repo_name.to_string();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| CodeIngestError::other(e.to_string()))?;
        tasks.spawn(async move {
            let _permit = permit;
            let res =
                process_file(db.as_ref(), ai.as_ref(), &full_path, &rel_path, &repo_name).await;
            if let Err(err) = &res {
                tracing::error!("Error processing {}: {err}", full_path.display());
            }
            res
        });
    }
    while let Some(joined) = tasks.join_next().await {
        // Per-file failures were already logged inside the task; a panicking
        // worker would surface here ( would crash the process).
        if let Err(e) = joined {
            tracing::error!("code ingest worker task failed: {e}");
        }
    }

    // Prune phase: delete files from DB that are no longer active in git HEAD.
    if let Err(err) = prune_repo(db.as_ref(), &abs_path, &active_files).await {
        tracing::error!(
            "Error pruning obsolete files for {}: {err}",
            abs_path.display()
        );
    }

    tracing::info!("Code analysis complete for {}", abs_path.display());
    report(progress.as_ref(), 4, 4, "Ingest complete");
    Ok(())
}

/// Directory-walk fallback (`filepath.Walk closure): skips dot-dirs,
/// configured ignored dirs/files, ignored paths, unsupported extensions and
/// oversized files. Walk errors abort the scan (parity).
#[allow(clippy::too_many_arguments)]
fn walk_active_files(
    abs_path: &Path,
    ignore_patterns: &[String],
    supported_exts: &HashSet<String>,
    ignored_dirs: &HashSet<String>,
    ignored_files: &HashSet<String>,
    max_size: i64,
    active_files: &mut HashSet<String>,
    pending: &mut Vec<(PathBuf, String)>,
) -> Result<(), CodeIngestError> {
    let mut iter = WalkDir::new(abs_path)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter();
    while let Some(entry) = iter.next() {
        let entry = entry.map_err(|e| CodeIngestError::Walk(e.to_string()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_dir() {
            if name.starts_with('.') && name != "." {
                iter.skip_current_dir();
                continue;
            }
            if ignored_dirs.contains(&name) {
                iter.skip_current_dir();
                continue;
            }
            if is_ignored(path, abs_path, ignore_patterns, true) {
                iter.skip_current_dir();
                continue;
            }
            continue;
        }

        if ignored_files.contains(&name) {
            continue;
        }
        if is_ignored(path, abs_path, ignore_patterns, false) {
            continue;
        }
        let ext = crate::ast_chunker::path_ext(path.to_string_lossy().as_ref()).to_lowercase();
        if !supported_exts.contains(&ext) {
            continue;
        }
        let size = i64::try_from(
            entry
                .metadata()
                .map_err(|e| CodeIngestError::Walk(e.to_string()))?
                .len(),
        )
        .unwrap_or(i64::MAX);
        if size > max_size {
            continue;
        }
        let rel = path
            .strip_prefix(abs_path)
            .map_err(|e| CodeIngestError::other(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        // Normalize slashes for git matching (git always uses forward slashes).
        active_files.insert(rel.replace('\\', "/"));
        pending.push((path.to_path_buf(), rel));
    }
    Ok(())
}

/// The `source_file` row projection used by pruning (`SELECT id, path`).
/// `id` is optional so malformed rows are skipped per-row likewise.
#[derive(Debug, SurrealValue)]
struct PruneFileRow {
    id: Option<RecordId>,
    path: Option<String>,
}

/// The `SELECT hash FROM <file id>` projection.
#[derive(Debug, SurrealValue)]
struct FileHashRow {
    hash: Option<String>,
}

/// The `SELECT count FROM file_chunk …` projection (Q1: count may arrive as
/// Int/Float/Decimal).
#[derive(Debug, SurrealValue)]
struct ChunkCountRow {
    count: Option<I64>,
}

/// Builds a [`Vars`] map from `(key, SurrealValue)` pairs.
fn vars_of(pairs: impl IntoIterator<Item = (&'static str, threski_db::value::Value)>) -> Vars {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Deletes DB rows for files that are no longer active in git HEAD
/// Record ids are validated with `is_safe_record_id`
/// before interpolation (F-A04).
async fn prune_repo(
    db: &dyn Executor,
    repo_path: &Path,
    active_files: &HashSet<String>,
) -> Result<(), CodeIngestError> {
    tracing::info!("Pruning obsolete files for {}...", repo_path.display());

    let separator = std::path::MAIN_SEPARATOR.to_string();
    let root = format!("{}{}", repo_path.display(), separator);
    let sql = format!(
        "SELECT id, path FROM {} WHERE string::starts_with(path, $root);",
        t::SOURCE_FILE
    );
    let outcome = db
        .smart_query(&sql, vars_of([("root", root.into_value())]))
        .await
        .map_err(|e| CodeIngestError::other(format!("failed to fetch files for pruning: {e}")))?;

    let rows: Vec<PruneFileRow> = outcome.rows(&sql)?;

    let mut to_delete: Vec<String> = Vec::new();
    for row in rows {
        let (Some(id), Some(path)) = (row.id, row.path) else {
            continue; // malformed row, skipped likewise
        };
        if path.is_empty() {
            continue;
        }
        // Calculate the relative path to match the active-files keys.
        let Ok(rel_os) = Path::new(&path).strip_prefix(repo_path) else {
            tracing::warn!("Failed to get relative path for {path}");
            continue;
        };
        let rel = rel_os.to_string_lossy().into_owned();
        let rel_git = rel.replace('\\', "/");
        // If the file is not in active files, it's not in git HEAD (or walk).
        if !active_files.contains(&rel_git) && !active_files.contains(&rel) {
            tracing::debug!("Deleting {} (Not in git HEAD / Walk)", path);
            to_delete.push(threski_db::ids::format_record(&id));
        }
    }

    if to_delete.is_empty() {
        return Ok(());
    }

    tracing::info!("Deleting {} obsolete files...", to_delete.len());

    // Batch delete (50 ids per batch).
    for batch in to_delete.chunks(50) {
        let unsafe_count = batch
            .iter()
            .filter(|id| !threski_db::ids::is_safe_record_id(id))
            .count();
        if unsafe_count > 0 {
            tracing::warn!("Prune batch had {unsafe_count} unsafe record ids; they were skipped");
        }
        let safe: Vec<&str> = batch
            .iter()
            .filter(|id| threski_db::ids::is_safe_record_id(id))
            .map(|id| id.as_str())
            .collect();
        if safe.is_empty() {
            continue;
        }
        let id_list = safe.join(", ");

        // 1. Delete chunks.
        let chunk_sql = format!("DELETE {} WHERE file IN [{}];", t::FILE_CHUNK, id_list);
        if let Err(err) = db.execute(&chunk_sql).await {
            tracing::warn!("Failed to delete chunks: {err}");
        }

        // 2. Delete files.
        let file_sql = format!("DELETE [{id_list}];");
        if let Err(err) = db.execute(&file_sql).await {
            tracing::warn!("Failed to delete files: {err}");
        }
    }

    Ok(())
}

/// Processes one file: hash-skip, HEAD-marker refresh, chunk persistence,
/// symbol/call edges and log templates.
async fn process_file(
    db: &dyn Executor,
    ai: &dyn CodeAIClient,
    abs_path: &Path,
    rel_path: &str,
    repo_name: &str,
) -> Result<(), CodeIngestError> {
    // Read once; hash and content share the buffer. The discovery size
    // filter (max_file_size) bounds this read.
    let file_bytes = std::fs::read(abs_path)?;

    // 1. Calculate hash.
    let hash = file_hash(&file_bytes);

    // Use repo-relative path + hash for the ID to allow history; prefix with
    // repoName to avoid collisions between multiple repositories.
    let safe_repo_name = threski_db::ids::sanitize_id(repo_name);
    let file_key = format!(
        "{}_{}_{}",
        safe_repo_name,
        threski_db::ids::sanitize_id(rel_path),
        hash
    );
    let file_id = threski_db::ids::format_record_id(t::SOURCE_FILE, &file_key);
    let file_record = RecordId::new(t::SOURCE_FILE, file_key.clone());

    // 2. Check if changed / check if the version already exists.
    let select_sql = format!("SELECT hash FROM {file_id};");
    let current_db_hash = match db.execute(&select_sql).await {
        Ok(outcome) => outcome
            .rows::<FileHashRow>(&select_sql)
            .unwrap_or_default()
            .into_iter()
            .next()
            .and_then(|r| r.hash)
            .unwrap_or_default(),
        Err(_) => String::new(),
    };

    // Optimization: if the current DB hash matches the disk hash, we are
    // 100% up to date.
    if !current_db_hash.is_empty() && current_db_hash == hash {
        return Ok(());
    }

    if !current_db_hash.is_empty() {
        tracing::debug!(
            "Hash mismatch for {}: DB={}, Disk={}",
            rel_path,
            truncate_hash(&current_db_hash),
            truncate_hash(&hash)
        );
    } else {
        tracing::debug!("File {} not found in DB or missing hash", rel_path);
    }

    tracing::info!("Processing {}...", rel_path);

    // 3. Update the file node (pointer to the current version): mark this
    // hash-suffixed record as HEAD and clear is_head on older versions of
    // the same path so temporal refresh/ask can resolve the canonical hash.
    let clear_head_sql = format!(
        "UPDATE {} SET is_head = false WHERE (path = $path OR rel_path = $rel) AND id != {file_id} AND is_head = true;",
        t::SOURCE_FILE
    );
    if let Err(err) = db
        .smart_query(
            &clear_head_sql,
            vars_of([
                ("path", path_str(abs_path).into_value()),
                ("rel", rel_path.into_value()),
            ]),
        )
        .await
    {
        tracing::warn!("Failed to clear previous HEAD markers for {rel_path}: {err}");
    }
    let upsert_sql = format!(
        "UPSERT {file_id} SET hash = $hash, path = $path, rel_path = $rel, repo_name = $repo, is_head = true, updated_at = time::now();"
    );
    db.smart_query(
        &upsert_sql,
        vars_of([
            ("hash", hash.clone().into_value()),
            ("path", path_str(abs_path).into_value()),
            ("rel", rel_path.into_value()),
            ("repo", repo_name.into_value()),
        ]),
    )
    .await
    .map_err(|e| CodeIngestError::other(format!("db update error: {e}")))?;

    // Check if chunks already exist for this file hash to avoid redundant AI
    // processing and storage.
    let count_sql = format!(
        "SELECT count() FROM {} WHERE file = {file_id} AND hash = $hash;",
        t::FILE_CHUNK
    );
    if let Ok(outcome) = db
        .smart_query(&count_sql, vars_of([("hash", hash.clone().into_value())]))
        .await
    {
        let count = outcome
            .rows::<ChunkCountRow>(&count_sql)
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|row| row.count)
            .map(|c| c.get())
            .unwrap_or(0);
        if count > 0 {
            tracing::info!(
                "  - Using existing embeddings for hash {}",
                truncate_hash(&hash)
            );
            return Ok(()); // Already have embeddings for this version!
        }
    }

    // 5. Chunk & persist (the async batch manager embeds later).
    let mut logs: Vec<LogTemplate> = Vec::new();
    let mut calls: Vec<CallEdge> = Vec::new();
    let mut parse_ok = false;
    let mut ast_chunks: Vec<AstChunk> = Vec::new();
    if let Ok((chunks, parsed_logs, parsed_calls)) =
        crate::ast_chunker::parse_ast(rel_path, &file_bytes).await
    {
        ast_chunks = chunks;
        logs = parsed_logs;
        calls = parsed_calls;
        parse_ok = true;
    }

    if parse_ok && needs_rule_synthesis(rel_path, &ast_chunks, &calls, &file_bytes) {
        if let Some(generator) = ai.as_content_generator() {
            let rewritten = maybe_synthesize_for_sparse_ast(
                generator,
                rel_path,
                &file_bytes,
                &ast_chunks,
                &calls,
            )
            .await;
            if rewritten {
                match crate::ast_chunker::parse_ast(rel_path, &file_bytes).await {
                    Ok((chunks, parsed_logs, parsed_calls)) => {
                        ast_chunks = chunks;
                        logs = parsed_logs;
                        calls = parsed_calls;
                    }
                    Err(_) => parse_ok = false,
                }
            }
        }
    }

    let valid_chunks: Vec<AstChunk> = if parse_ok && !ast_chunks.is_empty() {
        tracing::debug!(
            "  - Extracted {} AST chunks, {} log templates, and {} calls",
            ast_chunks.len(),
            logs.len(),
            calls.len()
        );

        // Persist calls (semantic architecture): translate AST call edges
        // into graph relations (symbol -> calls -> symbol).
        persist_call_edges(db, rel_path, repo_name, &file_id, &calls).await;
        ast_chunks
    } else {
        // Whole-file fallback chunking.
        let raw_chunks = chunk_content(&file_bytes, 1000)?;
        raw_chunks
            .into_iter()
            .map(|content| AstChunk {
                symbol_name: String::new(),
                kind: String::new(),
                content,
                start_line: 0,
                end_line: 0,
            })
            .collect()
    };

    // NOTE: old chunks are NOT deleted — history is kept.

    // 5.1 Persist log templates.
    if !logs.is_empty() {
        let mut sql = String::from("BEGIN TRANSACTION; ");
        let mut log_vars: Vars = Vars::new();
        for (i, log_def) in logs.iter().enumerate() {
            // ID based on file and line.
            let log_id = threski_db::ids::format_record_id(
                t::LOG_TEMPLATE,
                &format!(
                    "{}_{}_{}",
                    safe_repo_name,
                    threski_db::ids::sanitize_id(rel_path),
                    log_def.source_line
                ),
            );
            // Content goes through bound variables (F-002); the Windows
            // backslash JSON-encode workaround is no longer needed.
            let src_path = log_def.source_file.replace('\\', "/");
            log_vars.insert(
                format!("fmt{i}"),
                log_def.format_string.clone().into_value(),
            );
            log_vars.insert(format!("rx{i}"), log_def.regex.clone().into_value());
            log_vars.insert(format!("src{i}"), src_path.into_value());
            log_vars.insert(
                format!("line{i}"),
                I64::new(i64::from(log_def.source_line)).into_value(),
            );
            sql.push_str(&format!(
                "UPSERT {log_id} SET format_string = $fmt{i}, regex = $rx{i}, source_file = $src{i}, source_line = $line{i}, origin = 'ast'; "
            ));

            // Relate file -> log template.
            let rel_id = threski_db::ids::format_record_id(
                t::EMITS_LOG,
                &format!(
                    "{}_{}_{}",
                    safe_repo_name,
                    threski_db::ids::sanitize_id(rel_path),
                    i
                ),
            );
            // Deterministic edge id: re-ingest must be idempotent — SurrealDB 3.x
            // re-RELATE of an existing edge id errors (`already exists`), and
            // RELATION records cannot be upserted directly, so the idiom is
            // DELETE + RELATE inside the same transaction (D17 R5).
            sql.push_str(&format!(
                "DELETE {rel_id}; RELATE {file_id}->{}->{log_id} SET id = {rel_id}; ",
                t::EMITS_LOG
            ));
        }
        sql.push_str("COMMIT;");
        if let Err(err) = db.smart_query(&sql, log_vars).await {
            tracing::warn!("Failed to persist log templates for {rel_path}: {err}");
        }
    }

    // Batching logic.
    let batch_size = 100;
    for (start, batch) in valid_chunks.chunks(batch_size).enumerate() {
        let batch_start = start * batch_size;
        let mut valid_batch: Vec<(usize, &AstChunk)> = Vec::new();
        for (k, chunk) in batch.iter().enumerate() {
            if !chunk.content.trim().is_empty() {
                valid_batch.push((batch_start + k, chunk));
            }
        }
        if valid_batch.is_empty() {
            continue;
        }

        // Store chunks with batch_status = 'pending'; idempotent
        // (IF NOT EXISTS) so a partial resume preserves existing embeddings
        // (F-A03).
        let mut sql = String::from("BEGIN TRANSACTION; ");
        let mut batch_vars: Vars = Vars::new();
        for (k, (original_index, chunk_obj)) in valid_batch.iter().enumerate() {
            // Chunk ID also uses the prefixed relative path.
            let chunk_id = threski_db::ids::format_record_id(
                t::FILE_CHUNK,
                &format!(
                    "{}_{}_{}_{}",
                    safe_repo_name,
                    threski_db::ids::sanitize_id(rel_path),
                    hash,
                    original_index
                ),
            );
            batch_vars.insert(format!("file{k}"), file_record.clone().into_value());
            batch_vars.insert(format!("hash{k}"), hash.clone().into_value());
            batch_vars.insert(
                format!("content{k}"),
                chunk_obj.content.clone().into_value(),
            );
            batch_vars.insert(
                format!("sym{k}"),
                chunk_obj.symbol_name.clone().into_value(),
            );
            batch_vars.insert(format!("kind{k}"), chunk_obj.kind.clone().into_value());
            batch_vars.insert(
                format!("s{k}"),
                I64::new(i64::from(chunk_obj.start_line)).into_value(),
            );
            batch_vars.insert(
                format!("e{k}"),
                I64::new(i64::from(chunk_obj.end_line)).into_value(),
            );

            // `UPSERT` (3.x) replaces the removed `CREATE … IF NOT EXISTS` (D17 R1):
            // on a partial resume the deterministic chunk id already exists and
            // the embedding field is NOT in the SET list, so its value survives
            // (F-A03) while content/hash/lines refresh.
            sql.push_str(&format!(
                "UPSERT {chunk_id} SET file=$file{k}, hash=$hash{k}, content=$content{k}, symbol_name=$sym{k}, kind=$kind{k}, start_line=$s{k}, end_line=$e{k}, batch_status='pending'; "
            ));

            // If the chunk represents a symbol (function/class), explicitly
            // create the symbol node.
            if !chunk_obj.kind.is_empty()
                && !chunk_obj.kind.contains("snippet")
                && !chunk_obj.symbol_name.is_empty()
            {
                let symbol_id = threski_db::ids::format_record_id(
                    t::SYMBOL,
                    &format!(
                        "{}_{}_{}",
                        safe_repo_name,
                        threski_db::ids::sanitize_id(rel_path),
                        threski_db::ids::sanitize_id(&chunk_obj.symbol_name)
                    ),
                );
                // UPSERT (F-A03): a symbol id from a previous file version
                // must not fail the whole batch transaction.
                sql.push_str(&format!(
                    "UPSERT {symbol_id} SET name = $sym{k}, kind = $kind{k}, file = $file{k}; "
                ));
                sql.push_str(&format!("RELATE {file_id}->{}->{symbol_id}; ", t::CONTAINS));
            }
        }
        sql.push_str("COMMIT;");

        if let Err(err) = db.smart_query(&sql, batch_vars).await {
            tracing::error!("Failed to persist pending chunks for {rel_path}: {err}");
        }
        tracing::info!(
            "  - Queued {}/{} chunks for {}",
            batch_start + batch.len(),
            valid_chunks.len(),
            rel_path
        );
    }

    Ok(())
}

/// Persists call edges from AST extraction into the symbol graph
/// (the per-call node/edge SQL inside `processFile`). Best-effort: failures
/// are logged, never fatal ( discarded them silently — F-A15).
async fn persist_call_edges(
    db: &dyn Executor,
    rel_path: &str,
    repo_name: &str,
    file_id: &str,
    calls: &[CallEdge],
) {
    let safe_repo_name = threski_db::ids::sanitize_id(repo_name);
    for call in calls {
        // Format the IDs exactly as the chunk builder does.
        let caller_symbol_id = threski_db::ids::format_record_id(
            t::SYMBOL,
            &format!(
                "{}_{}_{}",
                safe_repo_name,
                threski_db::ids::sanitize_id(rel_path),
                threski_db::ids::sanitize_id(&call.caller_name)
            ),
        );
        let callee_symbol_id = threski_db::ids::format_record_id(
            t::SYMBOL,
            &format!(
                "{}_{}_{}",
                safe_repo_name,
                threski_db::ids::sanitize_id(rel_path),
                threski_db::ids::sanitize_id(&call.callee_name)
            ),
        );

        // Upsert the nodes in case the chunk logic hasn't created them yet;
        // record links are bound as RecordId (LB-08).
        let sql = format!(
            "UPSERT {caller_symbol_id} SET name = $caller, file = {file_id}; UPSERT {callee_symbol_id} SET name = $callee, file = {file_id}; RELATE {caller_symbol_id}->{}->{callee_symbol_id} SET file = $rel, line = $line;",
            t::CALLS
        );
        let call_vars = vars_of([
            ("caller", call.caller_name.clone().into_value()),
            ("callee", call.callee_name.clone().into_value()),
            ("rel", rel_path.into_value()),
            ("line", I64::new(i64::from(call.start_line)).into_value()),
        ]);
        if let Err(err) = db.smart_query(&sql, call_vars).await {
            tracing::warn!("Failed to persist call edge for {rel_path}: {err}");
        }
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// 8-char safe prefix for log lines (F-A05: sliced `[:8]` and could panic
/// on malformed rows).
fn truncate_hash(hash: &str) -> &str {
    let cut = {
        let mut cut = 8.min(hash.len());
        while cut > 0 && !hash.is_char_boundary(cut) {
            cut -= 1;
        }
        cut
    };
    &hash[..cut]
}

fn file_hash(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Very naive chunking by lines/size. Production should
/// use a tokenizer or smarter splitter. F-A11: the scanner had a 64KB line
/// cap that failed whole files on long lines (minified code); Rust line
/// iteration has no cap. Invalid UTF-8 is replaced like the JSON-marshal
/// write path did.
fn chunk_content(content: &[u8], size: usize) -> Result<Vec<String>, CodeIngestError> {
    let text = String::from_utf8_lossy(content);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        // Flush only a non-empty current chunk (F-A11: pushed an empty
        // chunk for an oversized first line and its 64KB scanner cap failed
        // whole files; Rust keeps the oversized line as its own chunk).
        if current.len() + line.len() > size && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_hash_is_sha256_hex() {
        // Well-known SHA-256 vector.
        assert_eq!(
            file_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(file_hash(b"").len(), 64);
    }

    #[test]
    fn truncate_hash_never_panics() {
        // Scenario: sliced [:8] and panicked on malformed (<8 byte) DB hashes (F-A05).
        assert_eq!(truncate_hash(""), "");
        assert_eq!(truncate_hash("ab"), "ab");
        assert_eq!(truncate_hash("abcdefghij"), "abcdefgh");
        // Char-boundary safe: 6 bytes < 8 → returned whole.
        assert_eq!(truncate_hash("héllo"), "héllo");
    }

    #[test]
    fn chunk_content_matrix() {
        // Scenario: TestChunkContent.
        let text = "line1\nline2\nline3\nline4\nline5";
        // Infinite size → 1 chunk, trailing newline appended per line.
        let chunks = chunk_content(text.as_bytes(), 100).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], format!("{text}\n"));

        // Small size → 1 line per chunk.
        let chunks = chunk_content(text.as_bytes(), 10).unwrap();
        assert_eq!(chunks.len(), 5, "chunks: {chunks:?}");
        assert_eq!(chunks[0], "line1\n");
    }

    #[test]
    fn chunk_content_large_input_scenario() {
        // Benchmark scenario: 1000 lines, small chunk size
        // forces many splits — pin the behavior, not the perf.
        let line = "This is a sample line of text to simulate code content.\n";
        let text = line.repeat(1000);
        let chunks = chunk_content(text.as_bytes(), 100).unwrap();
        assert!(chunks.len() > 10);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn chunk_content_long_line_is_not_dropped() {
        // F-A11: a single line larger than the chunk size must not fail the
        // file (the 64KB scanner cap errored out the whole ingest).
        let long_line = "x".repeat(70_000);
        let text = format!("{long_line}\nshort\n");
        let chunks = chunk_content(text.as_bytes(), 1000).unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks[0].starts_with("xxxx"));
        assert!(chunks.iter().any(|c| c.contains("short")));
    }

    #[test]
    fn vars_of_builds_map() {
        let v = vars_of([("a", "x".into_value()), ("b", I64::new(3).into_value())]);
        assert_eq!(v.len(), 2);
    }
}
