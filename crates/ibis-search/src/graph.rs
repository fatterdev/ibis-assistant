//! Graph context traversal: file history context, blast
//! radius and dead-code discovery over the commit/issue graph.
//!
//! Deltas:
//! - **F-001/D7**: [`get_file_context`] queries `source_file` — the
//!   oracle's raw query targeted the legacy `file` table (dead on every
//!   migrated database).
//! - Issue summaries keep insertion order (the map iteration was
//!   nondeterministic).

use std::collections::HashMap;
use std::sync::Arc;

use ibis_db::ids;
use ibis_db::value::{Object, Value};

use crate::commit_meaning;
use crate::error::{Result, SearchError};
use crate::rows;

/// Historical context of a file.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GraphContext {
    pub commits: Vec<CommitSummary>,
    pub issues: Vec<IssueSummary>,
}

/// One commit in a graph context.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommitSummary {
    pub id: String,
    pub hash: String,
    pub message: String,
    pub author: String,
    pub date: String,
    /// 0.0–1.0 score based on lines changed (edge `impact`).
    pub impact: f64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    /// Truncated commit_chunk excerpt.
    pub meaning: String,
}

/// One issue in a graph context.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IssueSummary {
    pub id: String,
    pub subject: String,
    pub status: String,
    /// Reinforcement score (edge `usage_weight`).
    pub weight: f64,
}

/// Traverses the weighted graph for one file path (`GetFileContext,
/// renamed logic). The path is bound (`$path` — F-002); the query targets
/// `source_file` (D7).
pub async fn get_file_context(
    db: &Arc<dyn ibis_db::Executor>,
    file_path: &str,
) -> Result<GraphContext> {
    let ql = r#"
	SELECT
		<-changed as change_edges,
		<-changed<-commit.{
			id,
			hash,
			message,
			date,
			author: <-authored<-author.name,
			issues: ->implements[WHERE usage_weight > 0.5]->issue.{
				id, subject, status,
				weight: ->implements.usage_weight
			}
		} as history
	FROM source_file
	WHERE path = $path;
	"#;
    let mut vars = ibis_db::executor::Vars::new();
    vars.insert("path".into(), Value::from_t(file_path));
    let outcome = db
        .smart_query(ql, vars)
        .await
        .map_err(|e| SearchError::wrap("graph query failed", e))?;

    let mut graph_ctx = GraphContext::default();

    let Some(rows_values) = rows::normalize_rows(&outcome) else {
        return Ok(graph_ctx);
    };
    let objects = rows::object_rows(&rows_values);
    if objects.is_empty() {
        return Ok(graph_ctx);
    }
    let row = objects[0];

    // change_edges: array of edge objects {in, out, impact}; `in` is the
    // commit id the impact belongs to (commit->changed->file).
    let mut impact_map: HashMap<String, f64> = HashMap::new();
    for edge in rows::row_maps(rows::field(row, "change_edges")) {
        let in_id = coerce_edge_id(rows::field(edge, "in"));
        let impact = rows::numeric_float(rows::field(edge, "impact"));
        // `if imp, ok := em["impact"].(float64)` — non-numeric impacts
        // (and absent ones) keep the 0.0 default. numeric_float already
        // returns 0.0 for non-numbers, so only record when present.
        if matches!(rows::field(edge, "impact"), Value::Number(_)) {
            impact_map.insert(in_id, impact);
        }
    }

    // history: projected commits with author name list + issue list.
    let history = rows::row_maps(rows::field(row, "history"));
    if history.is_empty() {
        return Ok(graph_ctx);
    }

    // Insertion-ordered issue dedupe (deterministic output).
    let mut issue_order: Vec<String> = Vec::new();
    let mut issue_map: HashMap<String, IssueSummary> = HashMap::new();

    for tc in history {
        let authors = rows::string_slice(rows::field(tc, "author"));
        let author_name = if authors.is_empty() {
            "Unknown".to_string()
        } else {
            authors[0].clone()
        };

        let id_str = rows::scalar_string(rows::field(tc, "id"));
        let impact = impact_map.get(&id_str).copied().unwrap_or(0.0);

        graph_ctx.commits.push(CommitSummary {
            id: id_str,
            hash: rows::scalar_string(rows::field(tc, "hash")),
            message: rows::scalar_string(rows::field(tc, "message")),
            date: rows::scalar_string(rows::field(tc, "date")),
            author: author_name,
            impact,
            meaning: String::new(),
        });

        for iss in rows::row_maps(rows::field(tc, "issues")) {
            let id_str = rows::scalar_string(rows::field(iss, "id"));
            // Weight may arrive scalar or as a one-element array depending on
            // the projection shape ( default 1.0 for any non-numeric cell).
            let weight_value = rows::field(iss, "weight");
            let w = match weight_value {
                Value::Array(items) => match items.first() {
                    Some(Value::Number(_)) => rows::numeric_float(items.first().unwrap()),
                    _ => 1.0,
                },
                Value::Number(_) => rows::numeric_float(weight_value),
                _ => 1.0,
            };
            if !issue_map.contains_key(&id_str) {
                issue_order.push(id_str.clone());
            }
            issue_map.insert(
                id_str.clone(),
                IssueSummary {
                    id: id_str,
                    subject: rows::scalar_string(rows::field(iss, "subject")),
                    status: rows::scalar_string(rows::field(iss, "status")),
                    weight: w,
                },
            );
        }
    }

    for id in issue_order {
        if let Some(issue) = issue_map.get(&id) {
            graph_ctx.issues.push(issue.clone());
        }
    }

    if graph_ctx.commits.len() > 5 {
        graph_ctx.commits.truncate(5);
    }

    let hashes: Vec<String> = graph_ctx
        .commits
        .iter()
        .filter(|c| !c.hash.is_empty())
        .map(|c| c.hash.clone())
        .collect();
    match commit_meaning::load_commit_chunk_excerpts(Some(db), &hashes, 1).await {
        Ok(excerpts) => {
            for commit in graph_ctx.commits.iter_mut() {
                if let Some(meaning) = excerpts.get(&commit.hash) {
                    commit.meaning = meaning.clone();
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to load commit_chunk meaning for file context: {e}");
        }
    }

    Ok(graph_ctx)
}

/// edge `in` ids arrive as strings, record structs or numbers; every
/// shape normalizes through the same coercion the history projection uses so
/// the impact map keys match (`TestGetFileContext_RobustID`).
fn coerce_edge_id(value: &Value) -> String {
    rows::scalar_string(value)
}

/// Architectural context of a symbol.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SymbolContext {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub callers: Vec<String>,
    pub called_symbols: Vec<String>,
    pub blast_radius: Vec<String>,
}

/// Traverses `calls` edges backwards to find everything that depends on the
/// symbol. The symbol id is sanitized and inlined
/// (identifier, F-002 allows inline after sanitization).
pub async fn analyze_blast_radius(
    db: &Arc<dyn ibis_db::Executor>,
    repo_name: &str,
    symbol_file: &str,
    symbol_name: &str,
) -> Result<SymbolContext> {
    let safe_repo_name = ids::sanitize_id(repo_name);
    let symbol_id = ids::format_record_id(
        "symbol",
        &format!(
            "{}_{}_{}",
            safe_repo_name,
            ids::sanitize_id(symbol_file),
            ids::sanitize_id(symbol_name)
        ),
    );

    let ql = format!(
        r#"
	SELECT
		name,
		kind,
		file.path as file_path,
		<-calls<-symbol.name as direct_callers,
		->calls->symbol.name as calls_out,
		<-calls<-symbol<-calls<-symbol.name as blast_radius
	FROM {symbol_id};
	"#
    );

    let outcome = db
        .execute(&ql)
        .await
        .map_err(|e| SearchError::wrap("failed to query blast radius", e))?;
    let Some(rows_values) = rows::normalize_rows(&outcome) else {
        return Err(SearchError::other("symbol not found in graph"));
    };
    let objects = rows::object_rows(&rows_values);
    if objects.is_empty() {
        return Err(SearchError::other("symbol not found in graph"));
    }
    let row = objects[0];

    Ok(SymbolContext {
        name: safe_get_string(row, "name"),
        kind: safe_get_string(row, "kind"),
        file: safe_get_string(row, "file_path"),
        callers: string_list(row, "direct_callers"),
        called_symbols: string_list(row, "calls_out"),
        blast_radius: string_list(row, "blast_radius"),
    })
}

/// Finds symbols with no incoming call edges.
pub async fn find_dead_code(
    db: &Arc<dyn ibis_db::Executor>,
    repo_name: &str,
) -> Result<Vec<String>> {
    let safe_repo = ids::sanitize_id(repo_name);
    let ql = format!(
        r#"
	SELECT name, file.path as file_path
	FROM symbol
	WHERE string::starts_with(id, 'symbol:{safe_repo}')
	  AND (kind = 'function_declaration' OR kind = 'method_declaration')
	  AND array::len(<-calls) = 0;
	"#
    );
    let outcome = db
        .execute(&ql)
        .await
        .map_err(|e| SearchError::wrap("failed to query dead code", e))?;
    let Some(rows_values) = rows::normalize_rows(&outcome) else {
        return Ok(Vec::new());
    };
    let mut dead_symbols = Vec::new();
    for row in rows::object_rows(&rows_values) {
        let name = safe_get_string(row, "name");
        let file = safe_get_string(row, "file_path");
        dead_symbols.push(format!("{name} (in {file})"));
    }
    Ok(dead_symbols)
}

fn safe_get_string(row: &Object, key: &str) -> String {
    match rows::field(row, key) {
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn string_list(row: &Object, key: &str) -> Vec<String> {
    match rows::field(row, key) {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fake_ai, fake_db, obj, rows_of, FakeDb};

    async fn ctx_with(return_data: Vec<(&str, Value)>) -> GraphContext {
        let db = fake_db(return_data);
        get_file_context(&db, "/path/to/file")
            .await
            .expect("get_file_context")
    }

    #[tokio::test]
    async fn maps_impact_via_id() {
        // change_edges has in="commit:123", impact=0.8; history item carries
        // the same id → impact maps onto the commit.
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                (
                    "change_edges",
                    rows_of(vec![obj(vec![
                        ("in", Value::from_t("commit:123")),
                        ("out", Value::from_t("file:abc")),
                        ("impact", Value::from_t(0.8f64)),
                    ])]),
                ),
                (
                    "history",
                    rows_of(vec![obj(vec![
                        ("id", Value::from_t("commit:123")),
                        ("hash", Value::from_t("hash123")),
                        ("message", Value::from_t("fix bug")),
                        ("date", Value::from_t("2024-01-01T00:00:00Z")),
                        ("author", Value::from_t(vec![Value::from_t("dev")])),
                        ("issues", rows_of(vec![])),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.commits.len(), 1);
        let c = &ctx.commits[0];
        assert_eq!(c.impact, 0.8);
        assert_eq!(c.hash, "hash123");
    }

    #[tokio::test]
    async fn robust_numeric_ids() {
        // Non-string (numeric) ids still map: both sides coerce to "123".
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                (
                    "change_edges",
                    rows_of(vec![obj(vec![
                        ("in", Value::from_t(123i64)),
                        ("out", Value::from_t("file:abc")),
                        ("impact", Value::from_t(0.8f64)),
                    ])]),
                ),
                (
                    "history",
                    rows_of(vec![obj(vec![
                        ("id", Value::from_t(123i64)),
                        ("hash", Value::from_t("hash123")),
                        ("message", Value::from_t("fix bug")),
                        ("date", Value::from_t("2024-01-01T00:00:00Z")),
                        ("author", Value::from_t(vec![Value::from_t("dev")])),
                        ("issues", rows_of(vec![])),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.commits.len(), 1);
        assert_eq!(ctx.commits[0].impact, 0.8);
        assert_eq!(ctx.commits[0].id, "123");
    }

    #[tokio::test]
    async fn edge_cases_empty_and_unknown_author() {
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                ("change_edges", rows_of(vec![])),
                ("history", rows_of(vec![])),
            ])]),
        )])
        .await;
        assert!(ctx.commits.is_empty());

        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                ("change_edges", rows_of(vec![])),
                (
                    "history",
                    rows_of(vec![obj(vec![
                        ("id", Value::from_t("commit:1")),
                        ("hash", Value::from_t("abc")),
                        ("message", Value::from_t("msg")),
                        ("date", Value::from_t("2024-01-01")),
                        ("author", Value::from_t(Vec::<Value>::new())),
                        ("issues", rows_of(vec![])),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.commits.len(), 1);
        assert!(ctx.issues.is_empty());
        assert_eq!(ctx.commits[0].author, "Unknown");
    }

    #[tokio::test]
    async fn malformed_result_yields_empty_context() {
        // Non-array statement result → empty context, never a panic.
        let db = fake_db(vec![("SELECT", Value::from_t("not-an-array"))]);
        let ctx = get_file_context(&db, "/file").await.expect("empty ctx");
        assert!(ctx.commits.is_empty());
    }

    #[tokio::test]
    async fn scalar_and_array_issue_weights() {
        // Scalar weight 0.9.
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                ("change_edges", rows_of(vec![])),
                (
                    "history",
                    Value::from_t(vec![obj(vec![
                        ("id", Value::from_t("commit:1")),
                        ("hash", Value::from_t("abc")),
                        ("message", Value::from_t("msg")),
                        ("date", Value::from_t("2024-01-01")),
                        ("author", Value::from_t(vec![Value::from_t("dev")])),
                        (
                            "issues",
                            Value::from_t(vec![obj(vec![
                                ("id", Value::from_t("issue:1")),
                                ("subject", Value::from_t("bug")),
                                ("status", Value::from_t("open")),
                                ("weight", Value::from_t(0.9f64)),
                            ])]),
                        ),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.issues.len(), 1);
        assert_eq!(ctx.issues[0].weight, 0.9);

        // Array weight [0.8].
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                ("change_edges", rows_of(vec![])),
                (
                    "history",
                    Value::from_t(vec![obj(vec![
                        ("id", Value::from_t("commit:2")),
                        ("hash", Value::from_t("def")),
                        ("message", Value::from_t("msg2")),
                        ("date", Value::from_t("2024-01-02")),
                        ("author", Value::from_t(vec![Value::from_t("dev")])),
                        (
                            "issues",
                            Value::from_t(vec![obj(vec![
                                ("id", Value::from_t("issue:2")),
                                ("subject", Value::from_t("feature")),
                                ("status", Value::from_t("closed")),
                                ("weight", Value::from_t(vec![Value::from_t(0.8f64)])),
                            ])]),
                        ),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.issues.len(), 1);
        assert_eq!(ctx.issues[0].weight, 0.8);
    }

    #[tokio::test]
    async fn non_numeric_impact_defaults_to_zero() {
        let ctx = ctx_with(vec![(
            "SELECT",
            rows_of(vec![obj(vec![
                (
                    "change_edges",
                    rows_of(vec![obj(vec![
                        ("in", Value::from_t("commit:3")),
                        ("out", Value::from_t("file:3")),
                        ("impact", Value::from_t("high")),
                    ])]),
                ),
                (
                    "history",
                    rows_of(vec![obj(vec![
                        ("id", Value::from_t("commit:3")),
                        ("hash", Value::from_t("ghi")),
                        ("message", Value::from_t("msg3")),
                        ("date", Value::from_t("2024-01-03")),
                    ])]),
                ),
            ])]),
        )])
        .await;
        assert_eq!(ctx.commits.len(), 1);
        assert_eq!(ctx.commits[0].impact, 0.0);
    }

    #[tokio::test]
    async fn query_targets_source_file_and_binds_path() {
        let typed = Arc::new(FakeDb::new(vec![("SELECT", rows_of(vec![obj(vec![])]))]));
        let db: Arc<dyn ibis_db::Executor> = typed.clone();
        let _ = get_file_context(&db, "/src/main.rs").await.unwrap();
        let sql = typed.smart_calls.lock().unwrap()[0].clone();
        assert!(
            sql.contains("FROM source_file"),
            "D7: query must target source_file, got {sql}"
        );
        assert!(sql.contains("$path"));
        let vars = typed.smart_vars.lock().unwrap()[0].clone();
        assert_eq!(vars.get("path"), Some(&Value::from_t("/src/main.rs")));
        let _ = fake_ai();
    }
}
