//! Tool-argument helpers (`strArg`,
//! `boolArg`, `getStringArg`, `getIntArg`) and
//! (`getBoolArg`, `getStringSliceArg`, `buildTicketSearchParamsFromArgs`,
//! `formatTicketSearchResponse`, `deriveRepositoryFromOrigin`).
//!
//! arguments arrive as `map[string]interface{}` decoded from JSON; the
//! Rust equivalent is `serde_json::Map<String, Value>`. Coercion semantics
//! are pinned by `tests/ticket_args_test.rs` contract gate).

use threski_ticketing::types::SearchParams as TicketSearchParams;
use threski_ticketing::types::SearchResult;
use serde_json::Value;

/// `strArg: bare string read, no trim.
pub fn str_arg(args: &serde_json::Map<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `boolArg : bare boolean read.
pub fn bool_arg(args: &serde_json::Map<String, Value>, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `getStringArg: string read, trimmed.
pub fn get_string_arg(args: &serde_json::Map<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `getIntArg : number or numeric string; empty string
/// and absent keys yield 0; other shapes yield an error
/// (`<key> must be a number`).
pub fn get_int_arg(args: &serde_json::Map<String, Value>, key: &str) -> Result<i64, String> {
    let Some(v) = args.get(key) else {
        return Ok(0);
    };
    if v.is_null() {
        return Ok(0);
    }
    if let Some(n) = v.as_f64() {
        return Ok(n as i64);
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        return trimmed
            .parse::<i64>()
            .map_err(|_| format!("{key} must be a number"));
    }
    Err(format!("{key} must be a number"))
}

/// `getBoolArg : booleans pass through; strings
/// accept `1|true|yes|on` case-insensitively after trimming.
pub fn get_bool_arg(args: &serde_json::Map<String, Value>, key: &str) -> bool {
    match args.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            let v = s.trim().to_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
        Some(_) => false,
    }
}

/// `getStringSliceArg : a comma-separated
/// string or a JSON array of strings; entries are trimmed, empty entries
/// dropped.
pub fn get_string_slice_arg(args: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    let Some(v) = args.get(key) else {
        return Vec::new();
    };
    if v.is_null() {
        return Vec::new();
    }
    match v {
        Value::String(s) => s
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// `deriveRepositoryFromOrigin : basename of
/// the origin URL without the `.git` suffix.
pub fn derive_repository_from_origin(origin: &str) -> String {
    let origin = origin.trim();
    if origin.is_empty() {
        return String::new();
    }
    let base = std::path::Path::new(origin)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = base.strip_suffix(".git").unwrap_or(&base).to_string();
    base.trim().to_string()
}

/// `buildTicketSearchParamsFromArgs
pub fn build_ticket_search_params_from_args(
    args: &serde_json::Map<String, Value>,
) -> Result<TicketSearchParams, String> {
    let limit = get_int_arg(args, "limit")?;
    let offset = get_int_arg(args, "offset")?;
    let mut params = TicketSearchParams {
        provider: get_string_arg(args, "provider"),
        query: get_string_arg(args, "query"),
        project_key: get_string_arg(args, "project_key"),
        status: get_string_arg(args, "status"),
        issue_type: get_string_arg(args, "type"),
        assignee: get_string_arg(args, "assignee"),
        author: get_string_arg(args, "author"),
        priority: get_string_arg(args, "priority"),
        updated_from: get_string_arg(args, "updated_from"),
        updated_to: get_string_arg(args, "updated_to"),
        sort: get_string_arg(args, "sort"),
        limit,
        offset,
    };
    if params.limit <= 0 {
        params.limit = 20;
    }
    Ok(params)
}

/// `ticketSearchToolResponse
#[derive(serde::Serialize)]
pub struct TicketSearchToolResponse {
    pub summary: String,
    pub issues: Vec<threski_ticketing::Ticket>,
    pub total_count: i64,
    pub offset: i64,
    pub limit: i64,
    pub compact: Vec<String>,
    pub filters: TicketSearchParams,
}

/// `formatTicketSearchResponse : the compact
/// `"[provider:key] title (status)"` list plus the full payload, 2-space
/// `MarshalIndent`-style.
pub fn format_ticket_search_response(
    result: &SearchResult,
    params: &TicketSearchParams,
    label: &str,
) -> Result<String, String> {
    let compact: Vec<String> = result
        .issues
        .iter()
        .map(|issue| issue.compact_line())
        .collect();
    let payload = TicketSearchToolResponse {
        summary: label.to_string(),
        issues: result.issues.clone(),
        total_count: result.total_count,
        offset: result.offset,
        limit: result.limit,
        compact,
        filters: params.clone(),
    };
    serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())
}

/// `json.MarshalIndent(value, "", " ") equivalent (handler helper).
#[allow(dead_code)]
pub fn marshal_indent<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        for (k, v) in pairs {
            map.insert(k.to_string(), v.clone());
        }
        map
    }

    #[test]
    fn string_helpers_trim_and_default() {
        let a = args(&[("x", json!("  hello "))]);
        assert_eq!(get_string_arg(&a, "x"), "hello");
        assert_eq!(get_string_arg(&a, "missing"), "");
        assert_eq!(str_arg(&a, "x"), "  hello ");
        assert_eq!(str_arg(&a, "missing"), "");
        assert!(!bool_arg(&a, "x"));
    }

    #[test]
    fn int_helper_matrix() {
        let a = args(&[("limit", json!(20.0)), ("n", json!("15")), ("e", json!(""))]);
        assert_eq!(get_int_arg(&a, "limit").unwrap(), 20);
        assert_eq!(get_int_arg(&a, "n").unwrap(), 15);
        assert_eq!(get_int_arg(&a, "e").unwrap(), 0);
        assert_eq!(get_int_arg(&a, "absent").unwrap(), 0);
        // A key that is absent entirely yields 0 (`v, ok := args[key]`).
        let bad = args(&[("n", json!("abc"))]);
        assert_eq!(
            get_int_arg(&bad, "n").err(),
            Some("n must be a number".to_string())
        );
        let bad_type = args(&[("n", json!({"a": 1}))]);
        assert_eq!(
            get_int_arg(&bad_type, "n").err(),
            Some("n must be a number".to_string())
        );
    }

    #[test]
    fn bool_helper_matrix() {
        for s in ["1", "true", "YES", "on"] {
            let a = args(&[("b", json!(s))]);
            assert!(get_bool_arg(&a, "b"), "{s}");
        }
        for s in ["0", "false", "", "whatever"] {
            let a = args(&[("b", json!(s))]);
            assert!(!get_bool_arg(&a, "b"), "{s}");
        }
        let a = args(&[("b", json!(true)), ("n", json!(1))]);
        assert!(get_bool_arg(&a, "b"));
        assert!(!get_bool_arg(&a, "n"));
        assert!(!get_bool_arg(&a, "absent"));
    }

    #[test]
    fn slice_helper_matrix() {
        let a = args(&[("ticket_ids", json!("#1,ABC-2, AB#3"))]);
        let values = get_string_slice_arg(&a, "ticket_ids");
        assert_eq!(values, vec!["#1", "ABC-2", "AB#3"]);
        let arr = args(&[("ids", json!([" a ", "", "b"]))]);
        assert_eq!(get_string_slice_arg(&arr, "ids"), vec!["a", "b"]);
        assert!(get_string_slice_arg(&args(&[]), "ids").is_empty());
        let num = args(&[("ids", json!(5))]);
        assert!(get_string_slice_arg(&num, "ids").is_empty());
    }

    #[test]
    fn derive_repository() {
        assert_eq!(
            derive_repository_from_origin("https://github.com/x/y.git"),
            "y"
        );
        assert_eq!(
            derive_repository_from_origin("https://dev.azure.com/o/p/_git/r"),
            "r"
        );
        assert_eq!(derive_repository_from_origin("  "), "");
        assert_eq!(derive_repository_from_origin(""), "");
    }
}
