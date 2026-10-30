//! Record-ID utilities.
//!
//! Semantics are implemented verbatim:
//! - [`format_record_id`] — angle-bracket wrapping for SurrealDB 3.0 compat,
//!   skipping re-wrap, with the numeric-ID exception on the `issue` table (Q4/Q5).
//! - [`is_safe_record_id`] — single record reference safe to interpolate.
//! - [`sanitize_id`] — safe record-ID suffix.
//! - [`escape_sql`] — legacy string-literal escaping (F-002: only for
//!   identifiers/internal use; all content-bearing queries must use bound
//!   variables via [`crate::executor::Executor::smart_query`]).
//! - [`coerce_record_id`] — accepts every legacy record-ID shape (Q3).

use surrealdb::types::{RecordId, RecordIdKey, ToSql, Value};

/// Characters that make a record ID unsafe to interpolate.
const UNSAFE_ID_CHARS: [char; 8] = [';', '\n', '\r', '\t', ' ', '\'', '"', '\\'];

/// Reports whether `id` is a single Surreal record reference safe to interpolate.
pub fn is_safe_record_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id.chars().any(|c| UNSAFE_ID_CHARS.contains(&c)) {
        return false;
    }
    let Some((table, key)) = id.split_once(':') else {
        return false;
    };
    if table.is_empty() || key.is_empty() {
        return false;
    }
    safe_record_id_table().is_match(table) && safe_record_id_key().is_match(key)
}

/// Returns a SurrealDB record ID formatted with angled brackets for v3.0 compatibility.
///
/// If `id` already contains `:` it is treated as a complete record ID and only the
/// ID part is (re-)wrapped; already-wrapped IDs are returned unchanged. Numeric IDs
/// on the `issue` table stay unwrapped (Q5).
pub fn format_record_id(table: &str, id: &str) -> String {
    // Already-wrapped key (`⟨abc⟩`, e.g. decoded from a wrapped string
    // record id): return as-is — re-wrapping would produce `⟨⟨abc⟩⟩`.
    if id.starts_with('⟨') && id.ends_with('⟩') {
        return format!("{table}:{id}");
    }
    if id.contains(':') {
        // It's already a complete record ID; parse it and wrap only the ID part.
        if let Some((table_part, id_part)) = id.split_once(':') {
            // Don't re-wrap if already wrapped
            if id_part.starts_with('⟨') && id_part.ends_with('⟩') {
                return id.to_string();
            }
            if is_numeric_id(id_part) && table_part == "issue" {
                return format!("{table_part}:{id_part}");
            }
            return format!("{table_part}:⟨{id_part}⟩");
        }
        return id.to_string();
    }
    if is_numeric_id(id) && table == "issue" {
        return format!("{table}:{id}");
    }
    format!("{table}:⟨{id}⟩")
}

/// Formats a decoded [`RecordId`] into the project-canonical interpolated form
/// (angle-bracket wrap + numeric exception).
pub fn format_record(record: &RecordId) -> String {
    match &record.key {
        RecordIdKey::Number(n) => format!("{}:{n}", record.table),
        RecordIdKey::String(s) => format_record_id(record.table.as_str(), s),
        other => format_record_id(record.table.as_str(), &other.to_sql()),
    }
}

/// Makes a string safe for use as a SurrealDB record ID suffix.
///
/// Lowercases, removes quotes completely, replaces every other character outside
/// `[a-z0-9_]` with underscores, collapses runs, and trims leading/trailing underscores.
pub fn sanitize_id(s: &str) -> String {
    // 1. Convert to lowercase
    let s = s.to_lowercase();
    // 2. Remove quotes completely (heuristic: they are often inside words)
    let s: String = s
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`'))
        .collect();
    // 3. Replace all other invalid characters with underscore (/, \, ., -, :, {, }, =, >, space, …)
    let replaced: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // 4. Collapse multiple underscores
    let mut out = String::with_capacity(replaced.len());
    for c in replaced.chars() {
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    // 5. Trim leading/trailing underscores
    out.trim_matches('_').to_string()
}

/// Escapes a string for use in a SurrealDB single-quoted string literal.
///
/// Prefer bound variables (`Executor::smart_query`) for all content-bearing
/// values — this helper covers only identifiers and legacy parity paths.
pub fn escape_sql(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Converts SurrealDB driver ID values (string, map, or record struct) into a
/// record ID string, accepting every legacy shape (Q3).
///
/// Returns an empty string for `NONE`/`NULL` and for shapes without an ID.
pub fn coerce_record_id(raw: &Value) -> String {
    match raw {
        Value::None | Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::RecordId(r) => record_id_to_plain(r),
        Value::Object(o) => coerce_record_id_map(o),
        _ => raw.to_sql(),
    }
}

fn coerce_record_id_map(map: &surrealdb::types::Object) -> String {
    let mut id_val = map
        .get("id")
        .and_then(Value::as_string)
        .cloned()
        .unwrap_or_default();
    if id_val.is_empty() {
        id_val = map
            .get("ID")
            .and_then(Value::as_string)
            .cloned()
            .unwrap_or_default();
    }
    if id_val.is_empty() {
        return String::new();
    }
    if id_val.contains(':') {
        return id_val;
    }
    let mut tb = map
        .get("tb")
        .and_then(Value::as_string)
        .cloned()
        .unwrap_or_default();
    if tb.is_empty() {
        tb = map
            .get("table")
            .and_then(Value::as_string)
            .cloned()
            .unwrap_or_default();
    }
    if tb.is_empty() {
        tb = map
            .get("Table")
            .and_then(Value::as_string)
            .cloned()
            .unwrap_or_default();
    }
    if !tb.is_empty() {
        format!("{tb}:{id_val}")
    } else {
        id_val
    }
}

/// Renders a decoded record as the plain (unwrapped) `table:id` form, mirroring
/// the `CoerceRecordID` struct path (`table + ":" + id`).
pub fn record_id_to_plain(record: &RecordId) -> String {
    match &record.key {
        RecordIdKey::Number(n) => format!("{}:{n}", record.table),
        RecordIdKey::String(s) => format!("{}:{s}", record.table),
        other => format!("{}:{}", record.table, other.to_sql()),
    }
}

/// Detects IDs that are purely numeric, which don't need bracket wrapping.
fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// safeRecordIDTable matches a Surreal table name segment.
fn safe_record_id_table() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("^[a-z][a-z0-9_]*$").expect("static regex"))
}

// safeRecordIDKey matches the ID segment (optionally angle-bracket wrapped).
fn safe_record_id_key() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("^(?:⟨)?[a-zA-Z0-9_.-]+(?:⟩)?$").expect("static regex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Object;

    #[test]
    fn test_sanitize_id() {
        let tests = [
            ("simple.txt", "simple_txt"),
            ("path/to/file.go", "path_to_file_go"),
            ("weird char's", "weird_chars"),
            ("file-with-dashes", "file_with_dashes"),
            (
                "app_docs_{scheda_view_=>_schede_views}",
                "app_docs_scheda_view_schede_views",
            ),
            ("c:\\Users\\Foo\\Code.cs", "c_users_foo_code_cs"),
            ("key=value", "key_value"),
            ("pointer->value", "pointer_value"),
            ("__start_end__", "start_end"),
        ];
        for (input, expected) in tests {
            assert_eq!(sanitize_id(input), expected, "sanitize_id({input:?})");
        }
    }

    #[test]
    fn test_escape_sql() {
        let tests = [
            ("simple string", "simple string"),
            ("It's a test", "It\\'s a test"),
            ("C:\\Path\\To\\File", "C:\\\\Path\\\\To\\\\File"),
            ("Line 1\nLine 2", "Line 1\\nLine 2"),
            ("Mixed: 'C:\\test\n'", "Mixed: \\'C:\\\\test\\n\\'"),
        ];
        for (input, expected) in tests {
            assert_eq!(escape_sql(input), expected, "escape_sql({input:?})");
        }
    }

    #[test]
    fn test_format_record_id() {
        let tests = [
            (
                "source_file",
                "extensionmethods_cs",
                "source_file:⟨extensionmethods_cs⟩",
            ),
            ("error_type", "abc", "error_type:⟨abc⟩"),
            ("", "table:id", "table:⟨id⟩"),
            ("issue", "42", "issue:42"),
            ("issue", "925", "issue:925"),
            ("commit", "99", "commit:⟨99⟩"),
            ("repo", "repo:⟨alpha⟩", "repo:⟨alpha⟩"),
            ("repo", "repo:alpha", "repo:⟨alpha⟩"),
            ("issue", "issue:77", "issue:77"),
            // Already-wrapped bare key: never double-wrapped.
            (
                "source_file",
                "⟨test-repo_secret.go⟩",
                "source_file:⟨test-repo_secret.go⟩",
            ),
        ];
        for (table, id, expected) in tests {
            assert_eq!(
                format_record_id(table, id),
                expected,
                "format({table:?}, {id:?})"
            );
        }
    }

    #[test]
    fn test_is_safe_record_id() {
        let ok = ["commit:abc123", "file:⟨main_go⟩", "issue:42"];
        let bad = [
            "",
            "nocolon",
            "x; DELETE repo;",
            "a:b c",
            "a:'b'",
            "DROP TABLE:x",
        ];
        for id in ok {
            assert!(
                is_safe_record_id(id),
                "IsSafeRecordID({id:?}) = false; want true"
            );
        }
        for id in bad {
            assert!(
                !is_safe_record_id(id),
                "IsSafeRecordID({id:?}) = true; want false"
            );
        }
    }

    #[test]
    fn test_coerce_record_id() {
        let tests: Vec<(&str, Value, &str)> = vec![
            ("string", Value::from_t("file_chunk:abc"), "file_chunk:abc"),
            (
                "map full",
                obj_str([("id", "file_chunk:abc")]),
                "file_chunk:abc",
            ),
            (
                "map tb+id",
                obj_str([("tb", "file_chunk"), ("id", "abc")]),
                "file_chunk:abc",
            ),
            (
                "map Table+ID",
                obj_str([("Table", "file_chunk"), ("ID", "abc")]),
                "file_chunk:abc",
            ),
            ("map empty", obj_str([("other", "x")]), ""),
            ("nil", Value::None, ""),
        ];
        for (name, raw, want) in tests {
            assert_eq!(coerce_record_id(&raw), want, "{name}: CoerceRecordID()");
        }
        // Surreal models.RecordID-like decoded value
        let record = RecordId::new("file_chunk", "xyz");
        assert_eq!(coerce_record_id(&Value::RecordId(record)), "file_chunk:xyz");
        // Must not recurse infinitely on maps that miss id
        let nested = obj_value([("nested", obj_str([("id", "x")]))]);
        assert_eq!(coerce_record_id(&nested), "");
    }

    #[test]
    fn test_format_record() {
        let angled = RecordId::new("memory", "ws_abc");
        assert_eq!(format_record(&angled), "memory:⟨ws_abc⟩");
        let numeric = RecordId::new("issue", RecordIdKey::Number(42));
        assert_eq!(format_record(&numeric), "issue:42");
    }

    fn obj_value(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        let mut obj = Object::new();
        for (k, v) in pairs {
            obj.insert(k, v);
        }
        Value::Object(obj)
    }

    fn obj_str(pairs: impl IntoIterator<Item = (&'static str, &'static str)>) -> Value {
        obj_value(pairs.into_iter().map(|(k, v)| (k, Value::from_t(v))))
    }
}
