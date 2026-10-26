//! Query-side reads: recent failures (memories +
//! log error types) and the `why_changed` provenance/commit projection.
//!
//! Deltas (F-002): interpolated `db.EscapeSQL`-escaped user text
//! (area, file path, symbol, issue id) directly into SurrealQL — the Rust
//! port binds every content-bearing value as `$…` variables instead.

use ibis_db::value::Value;

use super::{
    repo_record_id, temporal::memory_currentness_sql, Bindings, Memory, ES_CHANGED,
    ES_DERIVED_FROM, ES_IMPLEMENTS,
};
use crate::error::{Result, SearchError};
use crate::graph::{CommitSummary, IssueSummary};
use crate::rows;
use crate::search::SearchService;

/// `[1-9][0-9]*[dhw]` — the `failureSincePattern` guard ( used a regexp;
/// the accepted shape is a strict prefix check + unit + integer scan).
pub(crate) fn valid_failure_since(since: &str) -> bool {
    let bytes = since.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i + 1 != bytes.len() {
        return false;
    }
    matches!(bytes[i], b'd' | b'h' | b'w')
}

/// Recent-failure arguments. Area optionally
/// narrows both memory text and log errors related to a source file.
#[derive(Debug, Clone, Default)]
pub struct RecentFailuresParams {
    pub project_name: String,
    pub area: String,
    pub kinds: Vec<String>,
    pub since: String,
    pub limit: usize,
}

impl SearchService {
    /// Combines durable fix/gotcha memories with recent error types observed
    /// through the `log_entry -> is_type_of -> error_type` graph
    /// Values bind (F-002).
    pub async fn recent_failures(&self, params: &RecentFailuresParams) -> Result<Vec<Memory>> {
        let db = self.require_db()?;
        let mut kinds = super::non_empty_strings(&params.kinds);
        if kinds.is_empty() {
            kinds = vec!["fix".to_string(), "gotcha".to_string()];
        }
        let since = params.since.trim().to_lowercase();
        let since = if since.is_empty() {
            "30d"
        } else {
            since.as_str()
        }
        .to_string();
        if !valid_failure_since(&since) {
            return Err(SearchError::other(format!(
                "invalid failure duration {:?}",
                params.since
            )));
        }
        let mut limit = params.limit;
        if limit == 0 {
            limit = 10;
        }

        let mut memory_where = vec![
            "kind IN $kinds".to_string(),
            memory_currentness_sql(),
            "visibility = 'team'".to_string(),
            format!("created_at > time::now() - {since}"),
        ];
        let mut bindings: Bindings = vec![(
            "kinds".into(),
            Value::from_t(
                kinds
                    .iter()
                    .map(|k| Value::from_t(k.as_str()))
                    .collect::<Vec<Value>>(),
            ),
        )];
        let project = params.project_name.trim();
        if !project.is_empty() {
            memory_where.push(format!("project = {}", repo_record_id(project)));
        }
        let area = params.area.trim();
        if !area.is_empty() {
            memory_where.push(
                "(string::contains(string::lowercase(title OR ''), $area) \
                 OR string::contains(string::lowercase(content), $area) \
                 OR tags CONTAINS $area_raw)"
                    .to_string(),
            );
            bindings.push(("area".into(), Value::from_t(area.to_lowercase())));
            bindings.push(("area_raw".into(), Value::from_t(area)));
        }

        let sql = format!(
            "SELECT * FROM memory WHERE {} ORDER BY created_at DESC LIMIT {limit};",
            memory_where.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, super::bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("query recent failure memories: {e}")))?;
        let mut failures =
            super::memories_from_rows(&rows::normalize_rows(&outcome).unwrap_or_default());

        let mut log_where = vec![
            "hidden != true".to_string(),
            format!("created_at > time::now() - {since}"),
            "array::len(<-is_type_of<-log_entry) > 0".to_string(),
        ];
        let mut log_bindings: Bindings = Vec::new();
        if !project.is_empty() {
            log_where.push(
                "array::len(->related_to->source_file[WHERE repo_name = $project]) > 0".to_string(),
            );
            log_bindings.push(("project".into(), Value::from_t(project)));
        }
        if !area.is_empty() {
            log_where.push(
                "(string::contains(string::lowercase(category OR ''), $area) \
                 OR string::contains(string::lowercase(stack_trace OR ''), $area) \
                 OR array::len(->related_to->source_file[WHERE string::contains(string::lowercase(rel_path OR path), $area)]) > 0)"
                    .to_string(),
            );
            log_bindings.push(("area".into(), Value::from_t(area.to_lowercase())));
        }

        let sql = format!(
            "SELECT id, category, stack_trace, template, severity, created_at, array::len(<-is_type_of<-log_entry) AS occurrences, ->related_to.out AS related FROM error_type WHERE {} ORDER BY created_at DESC LIMIT {limit};",
            log_where.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, super::bind(log_bindings))
            .await
            .map_err(|e| SearchError::other(format!("query recent log failures: {e}")))?;
        if let Some(rows_values) = rows::normalize_rows(&outcome) {
            for row in rows::object_rows(&rows_values) {
                failures.push(log_failure_from_row(row));
            }
        }
        // Stable sort by created_at descending ( sort.SliceStable).
        failures.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        failures.truncate(limit);
        Ok(failures)
    }
}

/// `logFailureFromRow.
pub(crate) fn log_failure_from_row(row: &ibis_db::value::Object) -> Memory {
    let title = rows::scalar_string(rows::field(row, "category"));
    let mut content = rows::scalar_string(rows::field(row, "stack_trace"));
    if content.is_empty() {
        content = rows::scalar_string(rows::field(row, "template"));
    }
    Memory {
        id: rows::record_id(rows::field(row, "id")),
        kind: "gotcha".to_string(),
        title,
        content,
        status: "current".to_string(),
        confidence: 0.8,
        visibility: "team".to_string(),
        created_at: rows::scalar_string(rows::field(row, "created_at")),
        ..Default::default()
    }
}

/// Identifies a file, symbol, issue, or a file/symbol pair
///
#[derive(Debug, Clone, Default)]
pub struct WhyChangedParams {
    pub project_name: String,
    pub file_path: String,
    pub symbol_name: String,
    pub issue_id: String,
    pub limit: usize,
}

/// Merges memory provenance with commit and issue history
///
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WhyChangedResult {
    pub subject: String,
    pub memories: Vec<Memory>,
    pub commits: Vec<CommitSummary>,
    pub issues: Vec<IssueSummary>,
}

impl SearchService {
    /// Explains a change from `derived_from` provenance and the existing
    /// `commit -> changed -> source_file` / `commit -> implements -> issue`
    /// graph. Values bind (F-002).
    pub async fn why_changed(&self, params: &WhyChangedParams) -> Result<WhyChangedResult> {
        let db = self.require_db()?;
        let subject = why_changed_subject(params);
        if subject.is_empty() {
            return Err(SearchError::other(
                "file_path, symbol_name, or issue_id is required",
            ));
        }
        let mut limit = params.limit;
        if limit == 0 {
            limit = 5;
        }

        let mut provenance_where = why_changed_provenance_where(params);
        let bindings: Bindings = why_changed_provenance_bindings(params);
        let project = params.project_name.trim();
        if !project.is_empty() {
            provenance_where.push(format!("in.project = {}", repo_record_id(project)));
        }
        provenance_where.push(
            "(in.status = 'current') AND (in.valid_until = NONE OR in.valid_until > time::now())"
                .to_string(),
        );
        provenance_where.push("in.visibility = 'team'".to_string());
        provenance_where.push(
            "in.kind IN ['decision', 'fix', 'constraint', 'design', 'gotcha', 'reasoning']"
                .to_string(),
        );
        let sql = format!(
            "SELECT in.* FROM {ES_DERIVED_FROM} WHERE {} ORDER BY in.updated_at DESC LIMIT {limit};",
            provenance_where.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, super::bind(bindings))
            .await
            .map_err(|e| SearchError::other(format!("query change provenance: {e}")))?;
        let mut result = WhyChangedResult {
            subject,
            memories: super::memories_from_rows(
                &rows::normalize_rows(&outcome).unwrap_or_default(),
            ),
            commits: Vec::new(),
            issues: Vec::new(),
        };

        let (mut commit_where, commit_bindings) = why_changed_commit_where(params);
        if !project.is_empty() {
            commit_where.push(format!("repo = {}", repo_record_id(project)));
        }
        let sql = format!(
            "SELECT id, hash, message, date, <-authored<-author.name AS author, ->{ES_IMPLEMENTS}->issue.{{id, subject, status}} AS issues FROM commit WHERE {} ORDER BY date DESC LIMIT {limit};",
            commit_where.join(" AND ")
        );
        let outcome = db
            .smart_query(&sql, super::bind(commit_bindings))
            .await
            .map_err(|e| SearchError::other(format!("query change commits: {e}")))?;
        let (commits, issues) =
            why_changed_graph_rows(&rows::normalize_rows(&outcome).unwrap_or_default());
        result.commits = commits;
        result.issues = issues;

        let hashes: Vec<String> = result
            .commits
            .iter()
            .filter(|c| !c.hash.is_empty())
            .map(|c| c.hash.clone())
            .collect();
        let excerpts =
            crate::commit_meaning::load_commit_chunk_excerpts(self.db.as_ref(), &hashes, 1).await?;
        for commit in result.commits.iter_mut() {
            if let Some(meaning) = excerpts.get(&commit.hash) {
                commit.meaning = meaning.clone();
            }
        }
        Ok(result)
    }
}

/// `whyChangedSubject.
pub(crate) fn why_changed_subject(params: &WhyChangedParams) -> String {
    let file_path = params.file_path.trim();
    let symbol = params.symbol_name.trim();
    if !file_path.is_empty() && !symbol.is_empty() {
        return format!("{file_path}#{symbol}");
    }
    if !file_path.is_empty() {
        return file_path.to_string();
    }
    if !symbol.is_empty() {
        return symbol.to_string();
    }
    params.issue_id.trim().to_string()
}

/// `whyChangedProvenanceWhere (values bound, F-002).
fn why_changed_provenance_where(params: &WhyChangedParams) -> Vec<String> {
    if !params.file_path.trim().is_empty() {
        let mut where_parts = vec!["(out.path = $file OR out.rel_path = $file OR out.file.path = $file OR out.file.rel_path = $file)".to_string()];
        if !params.symbol_name.trim().is_empty() {
            where_parts.push(
                "(string::contains(string::lowercase(in.title OR ''), $symbol) OR string::contains(string::lowercase(in.content), $symbol))"
                    .to_string(),
            );
        }
        return where_parts;
    }
    if !params.symbol_name.trim().is_empty() {
        return vec![
            "(string::contains(string::lowercase(in.title OR ''), $symbol) OR string::contains(string::lowercase(in.content), $symbol))"
                .to_string(),
        ];
    }
    vec![
        "(out.external_id = $issue OR out.external_key = $issue OR string::ends_with(string(out.id), $issue))"
            .to_string(),
    ]
}

fn why_changed_provenance_bindings(params: &WhyChangedParams) -> Bindings {
    let mut bindings: Bindings = Vec::new();
    if !params.file_path.trim().is_empty() {
        bindings.push(("file".into(), Value::from_t(params.file_path.trim())));
    }
    if !params.symbol_name.trim().is_empty() {
        bindings.push((
            "symbol".into(),
            Value::from_t(params.symbol_name.trim().to_lowercase()),
        ));
    }
    if params.file_path.trim().is_empty() && params.symbol_name.trim().is_empty() {
        bindings.push(("issue".into(), Value::from_t(params.issue_id.trim())));
    }
    bindings
}

/// `whyChangedCommitWhere (values bound, F-002). The changed-path graph
/// traversal targets `source_file` (the post-migration table name — the
/// oracle used the `schema.TableFile` constant, which already resolves to
/// `source_file`).
fn why_changed_commit_where(params: &WhyChangedParams) -> (Vec<String>, Bindings) {
    let file_path = params.file_path.trim();
    if !file_path.is_empty() {
        return (
            vec![format!(
                "array::len(->{ES_CHANGED}->source_file[WHERE path = $file OR rel_path = $file]) > 0"
            )],
            vec![("file".into(), Value::from_t(file_path))],
        );
    }
    let symbol = params.symbol_name.trim();
    if !symbol.is_empty() {
        return (
            vec![format!(
                "array::len(->{ES_CHANGED}->source_file[WHERE path IN (SELECT VALUE file.rel_path FROM symbol WHERE name = $symbol) OR rel_path IN (SELECT VALUE file.rel_path FROM symbol WHERE name = $symbol)]) > 0"
            )],
            vec![("symbol".into(), Value::from_t(symbol.to_lowercase()))],
        );
    }
    let issue = params.issue_id.trim();
    (
        vec![format!(
            "array::len(->{ES_IMPLEMENTS}->issue[WHERE external_id = $issue OR external_key = $issue OR string::ends_with(string(id), $issue)]) > 0"
        )],
        vec![("issue".into(), Value::from_t(issue))],
    )
}

/// `whyChangedGraphRows.
pub(crate) fn why_changed_graph_rows(
    rows_values: &[ibis_db::value::Value],
) -> (Vec<CommitSummary>, Vec<IssueSummary>) {
    let mut commits = Vec::with_capacity(rows_values.len());
    let mut issues: Vec<IssueSummary> = Vec::new();
    let mut seen_issues = std::collections::HashSet::new();
    for row in rows::object_rows(rows_values) {
        let authors = rows::string_slice(rows::field(row, "author"));
        let author = authors.first().cloned().unwrap_or_default();
        commits.push(CommitSummary {
            id: rows::record_id(rows::field(row, "id")),
            hash: rows::scalar_string(rows::field(row, "hash")),
            message: rows::scalar_string(rows::field(row, "message")),
            author,
            date: rows::scalar_string(rows::field(row, "date")),
            impact: 0.0,
            meaning: String::new(),
        });
        for issue_row in rows::row_maps(rows::field(row, "issues")) {
            let issue = IssueSummary {
                id: rows::record_id(rows::field(issue_row, "id")),
                subject: rows::scalar_string(rows::field(issue_row, "subject")),
                status: rows::scalar_string(rows::field(issue_row, "status")),
                weight: 0.0,
            };
            if !seen_issues.insert(issue.id.clone()) {
                continue;
            }
            issues.push(issue);
        }
    }
    (commits, issues)
}
