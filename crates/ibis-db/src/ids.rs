//! Record-ID utilities.
//!
//! Semantics:
//! - [`format_record_id`] — angle-bracket wrapping for SurrealDB 3.0 compat,
//!   skipping re-wrap, with the numeric-ID exception on the `issue` table (Q4/Q5).
//! - [`source_file_id`] / [`file_chunk_id`] — canonical **array record ids**
//!   for the composite keys (WQ-7, SurrealDB 4 named id parts). The path
//!   component is the **raw repo-relative path** (F02: lossless — case,
//!   spaces, Unicode and special characters survive; collisions like
//!   `src/a-b.rs` vs `src/a/b.rs` or `Foo.rs` vs `foo.rs` are impossible),
//!   normalized to forward slashes. Ids are built as driver [`RecordId`]
//!   values and rendered with the SDK `to_sql` escaping (`'…'`/`"…"`
//!   literals, `\n`/`\t`/`\\`/quote escapes) — never hand-concatenated.
//! - [`is_safe_record_id`] — single record reference safe to interpolate,
//!   including the canonical array-id forms produced above (escape-aware
//!   structural validation) and their decoded renderings.
//! - [`sanitize_id`] — safe record-ID suffix (repo keys, symbol keys, …).
//! - [`escape_sql`] — legacy string-literal escaping (F-002: only for
//!   identifiers/internal use; all content-bearing queries must use bound
//!   variables via [`crate::executor::Executor::smart_query`]).
//! - [`coerce_record_id`] — accepts every legacy record-ID shape (Q3).

use surrealdb::types::{Array, RecordId, RecordIdKey, ToSql, Value};

use crate::schema::names::REPO;

/// Characters that make a record ID unsafe to interpolate.
const UNSAFE_ID_CHARS: [char; 8] = [';', '\n', '\r', '\t', ' ', '\'', '"', '\\'];

/// Maximum nesting depth accepted by the array-id validator.
const MAX_ARRAY_ID_DEPTH: usize = 3;

/// Reports whether `id` is a single Surreal record reference safe to interpolate.
pub fn is_safe_record_id(id: &str) -> bool {
    is_safe_record_id_depth(id, 0)
}

fn is_safe_record_id_depth(id: &str, depth: usize) -> bool {
    if depth > MAX_ARRAY_ID_DEPTH {
        return false;
    }
    let id = id.trim();
    if id.is_empty() {
        return false;
    }
    let Some((table, key)) = id.split_once(':') else {
        return false;
    };
    if table.is_empty() || key.is_empty() {
        return false;
    }
    if !safe_record_id_table().is_match(table) {
        return false;
    }
    let key = key.trim();
    // Array record id (WQ-7 composite keys): components carry their own
    // quoting/escaping rules, validated structurally (escape-aware — F02).
    if key.starts_with('[') && key.ends_with(']') {
        return is_safe_array_key(key, depth + 1);
    }
    if key
        .chars()
        .any(|c| matches!(c, ';' | '\n' | '\r' | '\t' | '\\'))
    {
        return false;
    }
    if key.chars().any(|c| UNSAFE_ID_CHARS.contains(&c)) {
        return false;
    }
    safe_record_id_key().is_match(key)
}

/// Validates the inside of a canonical array record id
/// (`[repo:⟨r⟩, 'src/a-b.rs', 42]`).
///
/// The split is **escape-aware**: quoted string components may contain any
/// character (the SDK `to_sql` rendering escapes quotes and backslashes and
/// escapes control characters as `\n`/`\t`), so only the literal structure is
/// policed — an unescaped quote, a trailing dangling escape or a raw newline
/// inside a literal is rejected.
fn is_safe_array_key(key: &str, depth: usize) -> bool {
    let inner = &key[1..key.len() - 1];
    if inner.trim().is_empty() {
        return false;
    }
    let mut parts: Vec<&str> = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut bracket = 0usize;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '[' => bracket += 1,
            ']' => {
                if bracket == 0 {
                    return false;
                }
                bracket -= 1;
            }
            ',' if bracket == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() || escaped || bracket != 0 {
        return false;
    }
    parts.push(&inner[start..]);
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        let bytes = part.as_bytes();
        if bytes.len() >= 2 && (bytes[0] == b'\'' || bytes[0] == b'"') {
            let q = bytes[0] as char;
            // Quoted string component: literal must close with the same
            // quote and its body must be escape-safe.
            if !part.ends_with(q) {
                return false;
            }
            if !is_safe_quoted_body(&part[1..part.len() - 1], q) {
                return false;
            }
        } else if part.bytes().all(|b| b.is_ascii_digit()) {
            // Integer component.
        } else if !is_safe_record_id_depth(part, depth) {
            return false;
        }
    }
    true
}

/// Validates the body of a quoted SurrealQL string literal: every backslash
/// escapes the following character, the matching quote never appears
/// unescaped, and no raw line break is carried inside the literal.
fn is_safe_quoted_body(body: &str, quote: char) -> bool {
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if chars.next().is_none() {
                return false;
            }
        } else if c == quote || matches!(c, '\n' | '\r') {
            return false;
        }
    }
    true
}

/// Normalizes a repo-relative path for the composite id: forward slashes,
/// verbatim otherwise (F02 — case, spaces, Unicode and special characters
/// survive; Windows backslashes are separators, never filename characters).
pub fn normalize_rel_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Resolves the repo record key carried by a `repo_ref` (`repo:⟨name⟩`,
/// `` repo:`name` ``, `repo:name` or a bare repo name — sanitized like every
/// repo record key).
fn repo_key_from_ref(repo_ref: &str) -> String {
    let r = repo_ref.trim();
    if let Some((table, key)) = r.split_once(':') {
        if table.trim() == REPO {
            let key = key.trim();
            for (open, close) in [('⟨', '⟩'), ('`', '`')] {
                if key.starts_with(open) && key.ends_with(close) && key.len() >= 2 {
                    return key[open.len_utf8()..key.len() - close.len_utf8()].to_string();
                }
            }
            return key.to_string();
        }
    }
    sanitize_id(r)
}

/// Canonical [`RecordId`] for a `source_file` row: `[repo, file_path]`.
///
/// The path component is the **raw repo-relative path** (`normalize_rel_path`),
/// lossless per F02; the repo component keeps the sanitized repo-record key
/// convention. Git ingest and code ingest both build this record, so their
/// rows unify. Bind it as a variable (F-002) or render it with
/// [`source_file_id`] for interpolation.
pub fn source_file_record(repo_ref: &str, path: &str) -> RecordId {
    let repo = RecordId::new(REPO, repo_key_from_ref(repo_ref));
    RecordId::new(
        crate::schema::names::SOURCE_FILE,
        RecordIdKey::Array(Array::from(vec![
            Value::RecordId(repo),
            Value::String(normalize_rel_path(path)),
        ])),
    )
}

/// Canonical array record id for a `source_file` row, rendered by the SDK
/// (`to_sql` escaping — safe to interpolate after [`is_safe_record_id`]).
pub fn source_file_id(repo_ref: &str, path: &str) -> String {
    Value::RecordId(source_file_record(repo_ref, path)).to_sql()
}

/// Canonical [`RecordId`] for a `file_chunk` row: `[file_id, line]`.
///
/// `file` must be the [`source_file_record`] of the owning file; `line` is
/// the chunk's `start_line`.
pub fn file_chunk_record(file: &RecordId, line: i64) -> RecordId {
    RecordId::new(
        crate::schema::names::FILE_CHUNK,
        RecordIdKey::Array(Array::from(vec![
            Value::RecordId(file.clone()),
            Value::Number(surrealdb::types::Number::Int(line)),
        ])),
    )
}

/// Canonical array record id for a `file_chunk` row: `[file_id, line]`.
///
/// `file_ref` must be the canonical [`source_file_id`] rendering (its
/// components are already SDK-escaped and validated); `line` is the chunk's
/// `start_line`. The nested record component is concatenated, never
/// re-quoted.
pub fn file_chunk_id(file_ref: &str, line: i64) -> String {
    format!("file_chunk:[{}, {}]", file_ref.trim(), line)
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
/// (angle-bracket wrap + numeric exception; array keys render through `to_sql`,
/// which yields the canonical `[component, …]` literal).
pub fn format_record(record: &RecordId) -> String {
    match &record.key {
        RecordIdKey::Number(n) => format!("{}:{n}", record.table),
        RecordIdKey::Array(arr) => format!("{}:{}", record.table, arr.to_sql()),
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
        RecordIdKey::Array(arr) => format!("{}:{}", record.table, arr.to_sql()),
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

// safeRecordIDKey matches the ID segment (optionally angle-bracket or
// backtick wrapped — 4.0 renders wrapped keys with backticks).
fn safe_record_id_key() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new("^(?:⟨)?[a-zA-Z0-9_.-]+(?:⟩)?$|^`[^`]*`$").expect("static regex")
    })
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

    #[test]
    fn test_source_file_id_and_file_chunk_id() {
        // F02 lossless: the path component is the raw repo-relative path,
        // rendered by the SDK (repo keys stay sanitized per the repo-record
        // convention; `to_sql` renders bare keys unwrapped).
        let fid = source_file_id("repo:⟨alpha⟩", "src/Main.rs");
        assert_eq!(fid, "source_file:[repo:alpha, 'src/Main.rs']");
        // Bare repo name sanitizes like any repo record key.
        assert_eq!(
            source_file_id("Alpha", "src/Main.rs"),
            "source_file:[repo:alpha, 'src/Main.rs']"
        );
        let cid = file_chunk_id(&fid, 42);
        assert_eq!(
            cid,
            "file_chunk:[source_file:[repo:alpha, 'src/Main.rs'], 42]"
        );
    }

    /// F02: distinct paths produce distinct ids; case, spaces, Unicode and
    /// special characters survive; backslashes normalize to forward slashes.
    #[test]
    fn test_source_file_id_is_lossless() {
        let cases = [
            ("src/a-b.rs", "source_file:[repo:alpha, 'src/a-b.rs']"),
            ("src/a/b.rs", "source_file:[repo:alpha, 'src/a/b.rs']"),
            ("Foo.rs", "source_file:[repo:alpha, 'Foo.rs']"),
            ("foo.rs", "source_file:[repo:alpha, 'foo.rs']"),
            (
                "uni-code_éà漢字.rs",
                "source_file:[repo:alpha, 'uni-code_éà漢字.rs']",
            ),
            ("comma,file.rs", "source_file:[repo:alpha, 'comma,file.rs']"),
            ("brk[et].rs", "source_file:[repo:alpha, 'brk[et].rs']"),
        ];
        for (path, want) in cases {
            assert_eq!(source_file_id("alpha", path), want, "{path}");
        }
        // Windows separators normalize; case is preserved.
        assert_eq!(
            source_file_id("alpha", "src\\Main.rs"),
            source_file_id("alpha", "src/Main.rs")
        );
        // Backslashes are separators and normalize; quotes switch the literal
        // form; newlines/tabs escape as \n/\t.
        assert_eq!(
            source_file_id("alpha", "back\\slash.rs"),
            "source_file:[repo:alpha, 'back/slash.rs']"
        );
        assert_eq!(
            source_file_id("alpha", "a\nb.rs"),
            "source_file:[repo:alpha, 'a\\nb.rs']"
        );
        // Every rendering stays validator-safe.
        for path in [
            "src/a-b.rs",
            "src/a/b.rs",
            "Foo.rs",
            "foo.rs",
            "with space's file.rs",
            "uni-code_éà漢字.rs",
            "back\\slash.rs",
            "a\nb.rs",
            "tab\there.rs",
            "he said \"hi\".rs",
            "close'; DELETE repo; --",
            "brk[et].rs",
            "comma,file.rs",
        ] {
            let id = source_file_id("alpha", path);
            assert!(is_safe_record_id(&id), "safe({id:?}) = false; want true");
            assert!(is_safe_record_id(&file_chunk_id(&id, 7)));
        }
    }

    #[test]
    fn test_is_safe_record_id_array_forms() {
        let ok = [
            "source_file:[repo:⟨alpha⟩, 'src_main_rs']",
            "file_chunk:[source_file:[repo:⟨alpha⟩, 'src_main_rs'], 42]",
            "source_file:[repo:alpha, 'x']",
            "file_chunk:[source_file:[repo:⟨alpha⟩, 'src_main_rs'], 0]",
            // F02: raw paths inside quoted components.
            "source_file:[repo:alpha, 'src/a-b.rs']",
            "source_file:[repo:alpha, \"with space's file.rs\"]",
            "source_file:[repo:alpha, 'comma,file.rs']",
            "source_file:[repo:alpha, 'back\\\\slash.rs']",
            "source_file:[repo:alpha, 'a\\nb.rs']",
            "source_file:[repo:alpha, \"close'; DELETE repo; --\"]",
        ];
        let bad = [
            "source_file:[repo:⟨alpha⟩, 'x'; DELETE repo]",
            "file_chunk:[source_file:[repo:⟨alpha⟩, 'x'], 42",
            "source_file:[repo:⟨alpha⟩, 'x',]",
            "source_file:[repo:`a`, 'x'] DROP",
            // Unescaped quote inside a single-quoted literal.
            "source_file:[repo:alpha, 'it's']",
            // Dangling escape.
            "source_file:[repo:alpha, 'x\\']",
            // Raw newline inside the literal.
            "source_file:[repo:alpha, 'a\nb.rs']",
            // Unclosed quote.
            "source_file:[repo:alpha, 'x]",
        ];
        for id in ok {
            assert!(is_safe_record_id(id), "safe({id:?}) = false; want true");
        }
        for id in bad {
            assert!(!is_safe_record_id(id), "safe({id:?}) = true; want false");
        }
    }

    #[test]
    fn test_is_safe_record_id_backtick_keys() {
        assert!(is_safe_record_id("repo:`123`"));
        assert!(!is_safe_record_id("repo:`1` DROP"));
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
