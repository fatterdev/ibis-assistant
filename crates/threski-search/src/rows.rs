//! Row decoding helpers over `threski_db::value::Value` — the 's
//! `normalizeRows` + the hand-coercion zoo. Every query
//! result passes through these typed accessors; decoding never hand-digs the
//! tagged JSON view and never panics on malformed rows (graph_robustness
//! contract).
//!
//! coercion parity:
//! - [`scalar_string`] mirrors `scalarString` (string → as-is, absent/none →
//!   "", other kinds → driver rendering).
//! - [`numeric_float`] mirrors `numericFloat` (Int/Float/Decimal → f64).
//! - [`string_slice`] mirrors `stringSlice` (array of strings).
//! - [`record_id`] mirrors `db.CoerceRecordID` (string/record/object shapes).
//! - [`row_maps`] mirrors `rowMaps` (array of objects).

use threski_db::executor::QueryOutcome;
use threski_db::value::{Number, Object, ToSql, Value};

/// `normalizeRows: the statement result as a row list.
///
/// - `Single(Array)` → the array items (the normal `SELECT …` shape).
/// - `Single(Object)` → one row (single-record projection).
/// - `None`/`Null` → an empty list.
/// - any other shape → `None` (the nil: callers treat it as "unexpected
///   result type", e.g. AskProject's decode failure).
/// - `Multi` → one entry per statement (statements are usually arrays, which
///   per-row accessors then skip — mirroring how the coercion skips them).
pub fn normalize_rows(outcome: &QueryOutcome) -> Option<Vec<Value>> {
    match outcome {
        QueryOutcome::Single(Value::Array(items)) => Some(items.iter().cloned().collect()),
        QueryOutcome::Single(Value::Object(_)) => Some(vec![outcome.first().cloned()?]),
        QueryOutcome::Single(Value::None | Value::Null) => Some(Vec::new()),
        QueryOutcome::Single(_) => None,
        QueryOutcome::Multi(statements) => Some(statements.clone()),
    }
}

/// `row.(map[string]interface{}) filter: keeps object rows only.
pub fn object_rows(rows: &[Value]) -> Vec<&Object> {
    rows.iter()
        .filter_map(|v| match v {
            Value::Object(obj) => Some(obj),
            _ => None,
        })
        .collect()
}

/// Field access on an object row; absent → `Value::None`.
pub fn field<'a>(row: &'a Object, key: &str) -> &'a Value {
    row.get(key).unwrap_or(&Value::None)
}

/// `db.CoerceRecordID: string / record / `{tb,id}` object shapes →
/// `table:id` (empty for NONE).
pub fn record_id(value: &Value) -> String {
    threski_db::ids::coerce_record_id(value)
}

/// `scalarString: string values pass through; NONE is ""; other kinds
/// render like the `fmt.Sprint` (integral floats without a decimal tail).
pub fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::None | Value::Null => String::new(),
        Value::Number(n) => number_string(n),
        Value::Bool(b) => b.to_string(),
        Value::Datetime(dt) => dt.to_string(),
        other => other.to_sql(),
    }
}

/// `numericFloat (Int | Float | Decimal → f64, else 0).
pub fn numeric_float(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.to_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `stringSlice: array-of-strings → Vec (other shapes → empty).
pub fn string_slice(value: &Value) -> Vec<String> {
    match value {
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

/// `rowMaps: array-of-objects (or a bare object) → object rows.
pub fn row_maps(value: &Value) -> Vec<&Object> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::Object(obj) => Some(obj),
                _ => None,
            })
            .collect(),
        Value::Object(obj) => vec![obj],
        _ => Vec::new(),
    }
}

/// Renders a number the way the JSON/`fmt.Sprint` pipeline does: integral
/// values carry no fractional tail (`123`, not `123.0`).
pub fn number_string(n: &Number) -> String {
    match n {
        Number::Int(i) => i.to_string(),
        other => match other.to_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            Some(f) => format!("{f}"),
            None => other.to_string(),
        },
    }
}

/// True when the value is a non-empty object (`len(row) == 0 guard).
pub fn is_object(value: &Value) -> bool {
    matches!(value, Value::Object(o) if !o.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use threski_db::value::RecordId;

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        let mut o = Object::new();
        for (k, v) in pairs {
            o.insert(k, v);
        }
        Value::Object(o)
    }

    #[test]
    fn normalize_shapes() {
        let rows = Value::from_t(vec![obj(vec![("id", Value::from_t("a"))])]);
        let out = normalize_rows(&QueryOutcome::Single(rows)).expect("array shape");
        assert_eq!(out.len(), 1);

        let single = normalize_rows(&QueryOutcome::Single(obj(vec![("id", Value::from_t("a"))])))
            .expect("object shape");
        assert_eq!(single.len(), 1);

        assert_eq!(
            normalize_rows(&QueryOutcome::Single(Value::None)).expect("none"),
            Vec::<Value>::new()
        );
        assert!(normalize_rows(&QueryOutcome::Single(Value::from_t("scalar"))).is_none());
    }

    #[test]
    fn scalar_string_number_render() {
        assert_eq!(scalar_string(&Value::from_t("x")), "x");
        assert_eq!(scalar_string(&Value::None), "");
        assert_eq!(scalar_string(&Value::from_t(123i64)), "123");
        assert_eq!(scalar_string(&Value::from_t(123.0f64)), "123");
        assert_eq!(scalar_string(&Value::from_t(1.5f64)), "1.5");
        assert_eq!(scalar_string(&Value::from_t(true)), "true");
    }

    #[test]
    fn coercions() {
        assert_eq!(record_id(&Value::from_t("memory:⟨a⟩")), "memory:⟨a⟩");
        assert_eq!(
            record_id(&Value::RecordId(RecordId::new("commit", "abc"))),
            "commit:abc"
        );
        assert_eq!(numeric_float(&Value::from_t(0.9f64)), 0.9);
        assert_eq!(numeric_float(&Value::from_t("no")), 0.0);
        let arr = Value::from_t(vec![Value::from_t("a"), Value::from_t(1i64)]);
        assert_eq!(string_slice(&arr), vec!["a".to_string()]);
        assert_eq!(row_maps(&arr).len(), 0);
        assert_eq!(row_maps(&Value::from_t(vec![obj(vec![])])).len(), 1);
        assert_eq!(row_maps(&obj(vec![])).len(), 1);
    }
}
