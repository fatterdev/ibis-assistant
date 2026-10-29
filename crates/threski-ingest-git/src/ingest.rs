//! History ingest pipeline.
//!
//! Flow: `UPSERT repo` → `git log HEAD --reverse --format=%H` (GitRunner) →
//! per-500-hash batches checked against `commit` (`SELECT VALUE id WHERE id IN
//! $ids` with record bindings) → new hashes fed to one `git log --no-walk
//! --stdin --numstat --format=COMMIT|%H|%P|%an|%aI|%s` invocation per batch →
//! lines parsed into a 1000-op `BEGIN TRANSACTION … COMMIT` batch writer.
//!
//! The writer state (transaction text, bound vars, op count, ticket dedupe,
//! per-commit file coupling) persists across git-log batches, preserving the
//! batching semantics exactly (E-A07).

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc;

use threski_db::executor::{Executor, Vars};
use threski_db::ids::{format_record_id, sanitize_id};
use threski_db::schema::names::{
    AUTHOR, AUTHORED, CHANGED, COMMIT, COUPLED_WITH, IMPLEMENTS, PARENT_OF, REPO, SOURCE_FILE,
};
use threski_db::value::{RecordId, Value};
use threski_git::sync::{history_oids, ingest_log_batch};
use threski_git::GitRunner;
use threski_ticketing::{issue_record_id, Ingester, IssueReference, ProviderName, PROVIDER_REDMINE};

use crate::error::IngestError;
use crate::issue_parser::extract_issue_refs;

/// Commits per existing-rows check (`batchSize in the feeder).
pub const HASH_BATCH_SIZE: usize = 500;
/// Parsed lines per `BEGIN TRANSACTION … COMMIT` flush.
pub const TXN_OPS: usize = 1000;
/// Ticket fan-out channel capacity.
pub const JOB_CHANNEL_CAP: usize = 100;

/// Fn-item helper for the git-log line feed (`fn(&str) -> String`, HRTB
/// — see the N-A14 note at the call site).
fn line_trim_owned(line: &str) -> String {
    line.trim().to_string()
}

/// Analyzes the git history of a repository into the knowledge graph
/// `concurrency` bounds the ticket-ingest worker pool.
pub async fn ingest_repo(
    client: Arc<dyn Executor>,
    ticket_ingester: Option<Arc<dyn Ingester>>,
    repo_path: &str,
    repo_name: &str,
    concurrency: usize,
    custom_patterns: &[&str],
) -> Result<(), IngestError> {
    tracing::info!("Ingesting git repository: {repo_path} (Name: {repo_name})");
    let abs = std::path::absolute(repo_path).map_err(IngestError::RepoPath)?;
    let abs_path = abs.display().to_string();

    let repo_id = format_record_id(REPO, &sanitize_id(repo_name));
    let mut vars = Vars::new();
    vars.insert("name".to_string(), Value::from_t(repo_name));
    client
        .smart_query(&format!("UPSERT {repo_id} SET name = $name;"), vars)
        .await
        .map_err(IngestError::RepoUpsert)?;

    let runner = GitRunner::anonymous();
    let hashes = history_oids(&runner, &abs_path)
        .await
        .map_err(IngestError::History)?;

    // Ticket fan-out: bounded channel + one worker per `concurrency` slot
    // ( jobChan + worker goroutines). Workers share the receiver via a
    // mutex; they drain and exit once the sender side is dropped below.
    let (tx, rx) = mpsc::channel::<IssueReference>(JOB_CHANNEL_CAP);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let mut handles = Vec::new();
    if let Some(ingester) = &ticket_ingester {
        for _ in 0..concurrency.max(1) {
            handles.push(tokio::spawn(ticket_worker(
                rx.clone(),
                ingester.clone(),
                repo_name.to_string(),
            )));
        }
    }

    let mut writer = StreamWriter::new(
        &client,
        ticket_ingester.as_ref(),
        &repo_id,
        repo_name,
        custom_patterns,
        tx,
    );
    let result = feed_batches(&runner, &abs_path, &hashes, &mut writer).await;

    // Close the fan-out and wait for in-flight ticket workers ('s
    // `defer close(jobChan); wg.Wait`).
    drop(writer);
    for handle in handles {
        let _ = handle.await;
    }
    if result.is_ok() {
        tracing::info!("Ingestion complete for {repo_name}");
    }
    result
}

/// Processes one git-log stream (exposed for the parser contract tests and
/// direct reuse; `processGitLogStream). Creates its own ticket fan-out,
/// flushes the final batch, and drains the workers before returning.
pub async fn process_git_log_lines(
    lines: impl IntoIterator<Item = String>,
    client: Arc<dyn Executor>,
    ticket_ingester: Option<Arc<dyn Ingester>>,
    repo_id: &str,
    repo_name: &str,
    concurrency: usize,
    custom_patterns: &[&str],
) -> Result<(), IngestError> {
    let (tx, rx) = mpsc::channel::<IssueReference>(JOB_CHANNEL_CAP);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let mut handles = Vec::new();
    if let Some(ingester) = &ticket_ingester {
        for _ in 0..concurrency.max(1) {
            handles.push(tokio::spawn(ticket_worker(
                rx.clone(),
                ingester.clone(),
                repo_name.to_string(),
            )));
        }
    }

    let mut writer = StreamWriter::new(
        &client,
        ticket_ingester.as_ref(),
        repo_id,
        repo_name,
        custom_patterns,
        tx,
    );
    let result = writer.process_lines(lines).await;
    if result.is_ok() {
        writer.flush().await?;
        tracing::debug!("Final batch flushed.");
    }
    drop(writer);
    for handle in handles {
        let _ = handle.await;
    }
    result
}

async fn ticket_worker(
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<IssueReference>>>,
    ingester: Arc<dyn Ingester>,
    repo_name: String,
) {
    loop {
        let Some(issue_ref) = rx.lock().await.recv().await else {
            break;
        };
        let project_key = if issue_ref.project_key.is_empty() {
            repo_name.as_str()
        } else {
            issue_ref.project_key.as_str()
        };
        if let Err(err) = ingester
            .ingest_issue_reference(project_key, issue_ref.clone())
            .await
        {
            tracing::warn!(
                "Failed to ingest referenced ticket {} ({}): {err}",
                issue_ref.external_key,
                issue_ref.provider
            );
        }
    }
}

async fn feed_batches(
    runner: &GitRunner,
    abs_path: &str,
    hashes: &[String],
    writer: &mut StreamWriter<'_>,
) -> Result<(), IngestError> {
    for batch in hashes.chunks(HASH_BATCH_SIZE) {
        let existing = check_existing(writer.client, batch).await?;
        let new_hashes: Vec<String> = batch
            .iter()
            .filter(|h| !existing.contains(&format_record_id(COMMIT, h)))
            .cloned()
            .collect();
        if new_hashes.is_empty() {
            continue;
        }
        tracing::debug!(
            "Batch check: {}/{} new commits queued",
            new_hashes.len(),
            batch.len()
        );
        let out = ingest_log_batch(runner, abs_path, &new_hashes)
            .await
            .map_err(IngestError::GitLog)?;
        let text = String::from_utf8_lossy(&out);
        writer
            // Fn-item path (HRTB): an inferred closure here made the
            // surrounding future non-Send when demanded by the server lane
            // (N-A14; rustc #89976 class).
            .process_lines(text.lines().map(line_trim_owned))
            .await?;
    }
    writer.flush().await?;
    tracing::debug!("Final batch flushed.");
    Ok(())
}

/// `SELECT VALUE id FROM commit WHERE id IN $ids` with **record** bindings —
/// string-form ids never match record comparisons on SurrealDB 3.x (
/// E-A31).
async fn check_existing(
    client: &Arc<dyn Executor>,
    batch: &[String],
) -> Result<HashSet<String>, IngestError> {
    let sql = "SELECT VALUE id FROM commit WHERE id IN $ids";
    let ids: Vec<Value> = batch
        .iter()
        .map(|h| Value::RecordId(RecordId::new(COMMIT, h.as_str())))
        .collect();
    let mut vars = Vars::new();
    vars.insert("ids".to_string(), Value::from_t(ids));
    let outcome = client
        .smart_query(sql, vars)
        .await
        .map_err(IngestError::ExistingCheck)?;
    Ok(outcome
        .value_ids(sql)
        .map_err(IngestError::ExistingCheck)?
        .into_iter()
        .collect())
}

/// Accumulates SurrealQL statements + bound variables across git-log batches
/// and flushes them in 1000-op transactions (F-002: all content is bound;
/// only sanitized record identifiers are inlined).
struct StreamWriter<'a> {
    client: &'a Arc<dyn Executor>,
    ingester: Option<&'a Arc<dyn Ingester>>,
    repo_id: &'a str,
    repo_name: &'a str,
    custom_patterns: &'a [&'a str],
    tx: mpsc::Sender<IssueReference>,

    seen_issues: HashSet<String>,
    current_commit_id: String,
    files_in_commit: Vec<String>,

    ql: String,
    vars: Vars,
    batch_count: usize,
    var_seq: usize,
}

impl<'a> StreamWriter<'a> {
    fn new(
        client: &'a Arc<dyn Executor>,
        ingester: Option<&'a Arc<dyn Ingester>>,
        repo_id: &'a str,
        repo_name: &'a str,
        custom_patterns: &'a [&'a str],
        tx: mpsc::Sender<IssueReference>,
    ) -> Self {
        StreamWriter {
            client,
            ingester,
            repo_id,
            repo_name,
            custom_patterns,
            tx,
            seen_issues: HashSet::new(),
            current_commit_id: String::new(),
            files_in_commit: Vec::new(),
            ql: String::new(),
            vars: Vars::new(),
            batch_count: 0,
            var_seq: 0,
        }
    }

    async fn process_lines(
        &mut self,
        lines: impl IntoIterator<Item = String>,
    ) -> Result<(), IngestError> {
        for line in lines {
            self.process_line(&line).await?;
        }
        Ok(())
    }

    /// Processes one line; returns whether it was consumed. Only consumed
    /// lines count toward the flush threshold — the `continue` paths skip
    /// `batchCount++` for empty/malformed lines (E-A17).
    async fn process_line(&mut self, line: &str) -> Result<bool, IngestError> {
        let counted = if let Some(rest) = line.strip_prefix("COMMIT|") {
            self.process_commit(rest).await?
        } else {
            self.process_numstat(line)?
        };
        if counted {
            self.batch_count += 1;
            if self.batch_count >= TXN_OPS {
                self.flush().await?;
                tracing::info!("... flushed {TXN_OPS} items to DB");
            }
        }
        Ok(counted)
    }

    fn next_var(&mut self, prefix: &str) -> String {
        let name = format!("{prefix}{}", self.var_seq);
        self.var_seq += 1;
        name
    }

    fn push_stmt(&mut self, stmt: String) {
        self.ql.push_str(&stmt);
        self.ql.push('\n');
    }

    fn bind_str(&mut self, name: &str, value: &str) {
        self.vars.insert(name.to_string(), Value::from_t(value));
    }

    async fn process_commit(&mut self, rest: &str) -> Result<bool, IngestError> {
        // `rest` excludes the literal `COMMIT|` prefix; fields:
        // hash | parents | author | date (RFC3339) | subject (rest of line).
        let parts: Vec<&str> = rest.splitn(5, '|').collect();
        if parts.len() < 5 {
            return Ok(false);
        }
        let hash = parts[0];
        let author_name = parts[2];
        let date = parts[3];
        let subject = parts[4];

        let commit_id = format_record_id(COMMIT, hash);
        self.current_commit_id = commit_id.clone();
        self.files_in_commit.clear();
        let author_id = format_record_id(AUTHOR, &sanitize_id(author_name));

        let author_var = self.next_var("author");
        self.push_stmt(format!("UPSERT {author_id} SET name = ${author_var};"));
        self.bind_str(&author_var, author_name);

        let hash_var = self.next_var("hash");
        let date_var = self.next_var("date");
        let msg_var = self.next_var("msg");
        self.push_stmt(format!(
            "UPSERT {commit_id} SET hash = ${hash_var}, date = ${date_var}, message = ${msg_var}, repo = {}, batch_status = 'pending';",
            self.repo_id
        ));
        self.bind_str(&hash_var, hash);
        // `commit.date` is a typed datetime field (D17 R7): bind the parsed
        // RFC3339 instant; unparsable dates bind NONE (field is optional).
        match chrono::DateTime::parse_from_rfc3339(date) {
            Ok(dt) => {
                self.vars.insert(
                    date_var,
                    Value::Datetime(surrealdb::types::Datetime::from(
                        dt.with_timezone(&chrono::Utc),
                    )),
                );
            }
            Err(_) => {
                self.vars.insert(date_var, Value::None);
            }
        }
        self.bind_str(&msg_var, subject);

        self.push_stmt(format!("RELATE {author_id}->{AUTHORED}->{commit_id};"));

        for parent in parts[1].split_whitespace() {
            let parent_id = format_record_id(COMMIT, parent);
            self.push_stmt(format!("RELATE {parent_id}->{PARENT_OF}->{commit_id};"));
        }

        let refs = extract_issue_refs(subject, self.custom_patterns);
        for issue_ref in refs {
            let confidence = issue_ref.confidence;
            let mut ticket_ref = IssueReference {
                provider: issue_ref.provider.clone(),
                external_id: issue_ref.external_id.clone(),
                external_key: issue_ref.external_key.clone(),
                project_key: issue_ref.project_key.clone(),
                confidence,
            };
            if ticket_ref.project_key.is_empty() {
                ticket_ref.project_key = self.repo_name.to_string();
            }

            let mut resolved_ref = ticket_ref.clone();
            if let Some(ingester) = self.ingester {
                match ingester
                    .resolve_reference(&ticket_ref.project_key, ticket_ref.clone())
                    .await
                {
                    Ok(resolved) => resolved_ref = resolved,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to resolve ticket reference {} {}: {err}",
                            ticket_ref.provider,
                            ticket_ref.external_key
                        );
                        continue;
                    }
                }
            } else if resolved_ref.provider.is_empty() {
                resolved_ref.provider = ProviderName::from(PROVIDER_REDMINE);
            }

            if resolved_ref.external_key.is_empty() {
                resolved_ref.external_key = resolved_ref.external_id.clone();
            }
            if resolved_ref.external_id.is_empty() {
                resolved_ref.external_id = resolved_ref.external_key.clone();
            }
            if resolved_ref.provider.is_empty() {
                resolved_ref.provider = ProviderName::from(PROVIDER_REDMINE);
            }

            let issue_id =
                issue_record_id(resolved_ref.provider.as_str(), &resolved_ref.external_key);
            let dedupe_key = format!("{}|{}", resolved_ref.provider, resolved_ref.external_key);

            if self.ingester.is_some() {
                if !self.seen_issues.contains(&dedupe_key) {
                    self.seen_issues.insert(dedupe_key);
                    let _ = self.tx.send(resolved_ref).await;
                }
            } else {
                let prov_var = self.next_var("prov");
                let extid_var = self.next_var("extid");
                let extkey_var = self.next_var("extkey");
                self.push_stmt(format!(
                    "UPSERT {issue_id} SET provider = ${prov_var}, external_id = ${extid_var}, external_key = ${extkey_var};"
                ));
                self.bind_str(&prov_var, resolved_ref.provider.as_str());
                self.bind_str(&extid_var, &resolved_ref.external_id);
                self.bind_str(&extkey_var, &resolved_ref.external_key);
            }

            self.push_stmt(format!(
                "RELATE {commit_id}->{IMPLEMENTS}->{issue_id} SET confidence = {:.6}, usage_weight = 1.0;",
                confidence
            ));
        }
        Ok(true)
    }

    fn process_numstat(&mut self, line: &str) -> Result<bool, IngestError> {
        if line.split_whitespace().count() < 3 {
            return Ok(false);
        }
        let tab_parts: Vec<&str> = line.split('\t').collect();
        if tab_parts.len() < 3 {
            return Ok(false);
        }
        let added_str = tab_parts[0];
        let deleted_str = tab_parts[1];
        let mut path = tab_parts[2..].join("\t");

        let added = parse_count(added_str);
        let deleted = parse_count(deleted_str);

        if path.starts_with('"') && path.ends_with('"') && path.len() >= 2 {
            if let Some(unquoted) = unquote_string(&path) {
                path = unquoted;
            }
        }

        // Scenario: computed log10(total+1), clamped it, then overwrote the value —
        // only the ratio survives (E-A02).
        let total_changed = (added + deleted) as f64;
        let impact = if total_changed > 0.0 {
            total_changed / (total_changed + 50.0)
        } else {
            0.0
        };

        let safe_repo_name = sanitize_id(self.repo_name);
        let file_id = format_record_id(
            SOURCE_FILE,
            &format!("{safe_repo_name}_{}", sanitize_id(&path)),
        );

        let path_var = self.next_var("path");
        self.push_stmt(format!("UPSERT {file_id} SET path = ${path_var};"));
        self.bind_str(&path_var, &path);

        if !self.current_commit_id.is_empty() {
            self.push_stmt(format!(
                "RELATE {}->{CHANGED}->{file_id} SET impact = {:.6}, added = {}, deleted = {};",
                self.current_commit_id, impact, added, deleted
            ));
            let prev_files = self.files_in_commit.clone();
            for prev_file_id in prev_files {
                self.push_stmt(format!(
                    "RELATE {file_id}->{COUPLED_WITH}->{prev_file_id} SET commit = {};",
                    self.current_commit_id
                ));
                self.push_stmt(format!(
                    "RELATE {prev_file_id}->{COUPLED_WITH}->{file_id} SET commit = {};",
                    self.current_commit_id
                ));
            }
            self.files_in_commit.push(file_id);
        }
        Ok(true)
    }

    async fn flush(&mut self) -> Result<(), IngestError> {
        if self.ql.is_empty() {
            return Ok(());
        }
        let sql = format!("BEGIN TRANSACTION;\n{}COMMIT TRANSACTION;", self.ql);
        tracing::debug!("Flushing batch of {} operations", self.batch_count);
        let vars = std::mem::take(&mut self.vars);
        self.client
            .smart_query(&sql, vars)
            .await
            .map_err(IngestError::Batch)?;
        self.ql.clear();
        self.batch_count = 0;
        Ok(())
    }
}

/// `-` and malformed counters parse as 0; malformed values are logged instead
/// of swallowed silently (E-A04).
fn parse_count(s: &str) -> i64 {
    if s == "-" {
        return 0;
    }
    match s.parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            tracing::debug!("ignoring malformed numstat counter {s:?}");
            0
        }
    }
}

/// `strconv.Unquote subset for double-quoted git paths (C-style escapes
/// incl. the octal byte escapes git emits for non-ASCII). Returns `None` on
/// anything the original parser would reject, so callers keep the quoted form.
pub(crate) fn unquote_string(quoted: &str) -> Option<String> {
    let bytes = quoted.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &bytes[1..bytes.len() - 1];
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let b = inner[i];
        if b == b'"' || b == b'\n' {
            return None;
        }
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let esc = *inner.get(i)?;
        i += 1;
        match esc {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0B),
            b'x' => {
                let hi = hex_val(*inner.get(i)?)?;
                let lo = hex_val(*inner.get(i + 1)?)?;
                i += 2;
                out.push(hi * 16 + lo);
            }
            b'0'..=b'7' => {
                let mut val = (esc - b'0') as u32;
                for _ in 0..2 {
                    let d = *inner.get(i)?;
                    if !d.is_ascii_digit() || d > b'7' {
                        return None;
                    }
                    val = val * 8 + (d - b'0') as u32;
                    i += 1;
                }
                out.push(u8::try_from(val).ok()?);
            }
            b'u' | b'U' => {
                let width = if esc == b'u' { 4 } else { 8 };
                let mut val: u32 = 0;
                for _ in 0..width {
                    let d = hex_val(*inner.get(i)?)?;
                    val = val * 16 + u32::from(d);
                    i += 1;
                }
                let ch = char::from_u32(val)?;
                out.extend_from_slice(ch.encode_utf8(&mut [0u8; 4]).as_bytes());
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_matrix() {
        assert_eq!(unquote_string(r#""plain""#).as_deref(), Some("plain"));
        assert_eq!(unquote_string(r#""a\tb""#).as_deref(), Some("a\tb"));
        assert_eq!(unquote_string(r#""a\"b""#).as_deref(), Some("a\"b"));
        assert_eq!(unquote_string(r#""a\\b""#).as_deref(), Some("a\\b"));
        // octal byte escape (git core.quotePath form for é)
        assert_eq!(unquote_string(r#""caf\303\251""#).as_deref(), Some("café"));
        assert_eq!(unquote_string(r#""\x41bc""#).as_deref(), Some("Abc"));
        assert_eq!(unquote_string(r#""A""#).as_deref(), Some("A"));
        // malformed → keep quoted
        assert!(unquote_string(r#""\q""#).is_none());
        assert!(unquote_string(r#""\x2""#).is_none());
        assert!(unquote_string(r#""unterminated"#).is_none());
        assert!(unquote_string("no quotes").is_none());
    }
}
