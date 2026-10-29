//! Commit-chunk scoring and excerpt loading.

use std::collections::HashMap;

use threski_db::value::Value;

use crate::error::{Result, SearchError};
use crate::rows;

/// Patch-text cap for agent observations.
pub const COMMIT_CHUNK_OBSERVATION_CAP: usize = 2000;

/// Vector for [`commit_chunk_score_expr`]: the query embedding as floats.
pub fn vec_value(vec: &[f32]) -> Value {
    Value::from_t(vec.iter().map(|f| f64::from(*f)).collect::<Vec<f64>>())
}

/// Alias kept for readability at embedding call sites (`vec_value_f32`).
pub(crate) fn vec_value_f32(vec: &[f32]) -> Value {
    vec_value(vec)
}

/// Embedding carried as f64 (precomputed vectors) bound as `$embedding`.
pub(crate) fn f64_vec_value(vec: &[f64]) -> Value {
    Value::from_t(vec.to_vec())
}

/// Scores commit_chunk by vector similarity + commit_date decay (authorship
/// time), never embedding ingest time. The vector is bound (`$vec`, F-002).
/// `math::max` takes a single array on SurrealDB 3.x (D17) — the original 2.x
/// two-arg form fails live.
pub(crate) fn commit_chunk_score_expr() -> String {
    "(vector::similarity::cosine(embedding, $vec) * 0.7) + \
     (IF commit_date IS NONE THEN 0.3 ELSE math::max([0, 1 - duration::days(time::now() - type::datetime(commit_date)) / 365]) * 0.3 END)"
        .to_string()
}

/// `commitChunkAskQuery (vector bound as `$vec`).
pub(crate) fn commit_chunk_ask_query() -> String {
    format!(
        "SELECT id, content, commit_hash, commit_date, {} as score FROM commit_chunk WHERE embedding != NONE ORDER BY score DESC LIMIT 15;",
        commit_chunk_score_expr()
    )
}

/// `TruncateCommitMeaning: caps patch text for agent observations.
pub fn truncate_commit_meaning(content: &str) -> String {
    let runes: Vec<char> = content.chars().collect();
    if runes.len() <= COMMIT_CHUNK_OBSERVATION_CAP {
        return content.to_string();
    }
    let mut out: String = runes[..COMMIT_CHUNK_OBSERVATION_CAP].iter().collect();
    out.push_str("...(truncated)");
    out
}

/// `LoadCommitChunkExcerpts: truncated commit_chunk content per hash.
/// Each hash is capped independently so one busy commit cannot starve the
/// others (per-hash query, bound `$hash` — F-002).
pub async fn load_commit_chunk_excerpts(
    db: Option<&std::sync::Arc<dyn threski_db::Executor>>,
    commit_hashes: &[String],
    per_commit: usize,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    let Some(db) = db else {
        return Ok(out);
    };
    if commit_hashes.is_empty() {
        return Ok(out);
    }
    let per_commit = per_commit.max(1);
    let mut seen = std::collections::HashSet::new();
    for raw_hash in commit_hashes {
        let h = raw_hash.trim();
        if h.is_empty() || !seen.insert(h.to_string()) {
            continue;
        }
        let sql = format!(
            "SELECT commit_hash, content FROM commit_chunk WHERE commit_hash = $hash ORDER BY id LIMIT {per_commit};"
        );
        let mut vars = threski_db::executor::Vars::new();
        vars.insert("hash".into(), Value::from_t(h));
        let outcome = db
            .smart_query(&sql, vars)
            .await
            .map_err(|e| SearchError::other(format!("load commit_chunk excerpts for {h}: {e}")))?;
        let Some(rows_values) = rows::normalize_rows(&outcome) else {
            continue;
        };
        for row in rows::object_rows(&rows_values) {
            let hash = rows::scalar_string(rows::field(row, "commit_hash"))
                .trim()
                .to_string();
            let content = rows::scalar_string(rows::field(row, "content"))
                .trim()
                .to_string();
            if hash.is_empty() || content.is_empty() {
                continue;
            }
            out.entry(hash)
                .or_insert_with(|| truncate_commit_meaning(&content));
            break;
        }
    }
    Ok(out)
}

/// `LoadCommitChangedPaths: one representative changed file path per
/// commit hash (hashes bound as `$hashes` — F-002).
pub async fn load_commit_changed_paths(
    db: Option<&std::sync::Arc<dyn threski_db::Executor>>,
    commit_hashes: &[String],
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    let Some(db) = db else {
        return Ok(out);
    };
    if commit_hashes.is_empty() {
        return Ok(out);
    }
    let mut clean: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw_hash in commit_hashes {
        let h = raw_hash.trim();
        if h.is_empty() || !seen.insert(h.to_string()) {
            continue;
        }
        clean.push(h.to_string());
    }
    if clean.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT hash, ->changed->source_file.{{path, rel_path}} AS files FROM commit WHERE hash IN $hashes LIMIT {};",
        clean.len()
    );
    let mut vars = threski_db::executor::Vars::new();
    vars.insert(
        "hashes".into(),
        Value::from_t(
            clean
                .iter()
                .map(|h| Value::from_t(h.as_str()))
                .collect::<Vec<Value>>(),
        ),
    );
    let outcome = db
        .smart_query(&sql, vars)
        .await
        .map_err(|e| SearchError::other(format!("load commit changed paths: {e}")))?;
    let Some(rows_values) = rows::normalize_rows(&outcome) else {
        return Ok(out);
    };
    for row in rows::object_rows(&rows_values) {
        let hash = rows::scalar_string(rows::field(row, "hash"))
            .trim()
            .to_string();
        if hash.is_empty() {
            continue;
        }
        if let Some(path) = first_changed_file_path(rows::field(row, "files")) {
            out.insert(hash, path);
        }
    }
    Ok(out)
}

/// `firstChangedFilePath.
fn first_changed_file_path(raw: &Value) -> Option<String> {
    match raw {
        Value::Array(items) => {
            for item in items {
                if let Some(path) = path_from_file_row(item) {
                    return Some(path);
                }
            }
            None
        }
        other => path_from_file_row(other),
    }
}

/// `pathFromFileRow.
fn path_from_file_row(raw: &Value) -> Option<String> {
    match raw {
        Value::Object(row) => {
            let mut path = rows::scalar_string(rows::field(row, "path"))
                .trim()
                .to_string();
            if path.is_empty() {
                path = rows::scalar_string(rows::field(row, "rel_path"))
                    .trim()
                    .to_string();
            }
            if path.is_empty() {
                None
            } else {
                Some(path)
            }
        }
        Value::None | Value::Null => None,
        other => {
            let s = rows::scalar_string(other).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_commit_meaning_caps_runes() {
        assert_eq!(truncate_commit_meaning("short"), "short");
        let long = "x".repeat(COMMIT_CHUNK_OBSERVATION_CAP + 10);
        let out = truncate_commit_meaning(&long);
        assert_eq!(
            out.chars().count(),
            COMMIT_CHUNK_OBSERVATION_CAP + "...(truncated)".len()
        );
        assert!(out.ends_with("...(truncated)"));
    }

    #[test]
    fn score_expr_binds_vector() {
        let expr = commit_chunk_score_expr();
        assert!(expr.contains("vector::similarity::cosine(embedding, $vec)"));
        assert!(expr.contains("commit_date"));
        let q = commit_chunk_ask_query();
        assert!(q.contains("FROM commit_chunk"));
        assert!(q.contains("ORDER BY score DESC LIMIT 15"));
    }
}
