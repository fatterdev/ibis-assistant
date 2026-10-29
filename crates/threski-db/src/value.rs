//! SurrealValue codec boundary — the one place where raw SurrealDB responses
//! become typed values.
//!
//! Quirks encoded here exactly once:
//! - **Q1** numeric coercion: SurrealQL literals like `0.7` store as `Number::Decimal`
//!   and typed `f64` decode fails — numeric fields go through [`F64`] / [`I64`],
//!   which accept `Int | Float | Decimal` and coerce (validating integrality).
//! - **Q2** option fields: the driver `Option<T>` impl treats both absent keys and
//!   explicit `NONE` as `None` (row structs just use `Option<…>`). Explicit `NULL`
//!   is outside the contract — SurrealDB never persists it for `option<…>` typed
//!   fields — and surfaces as a typed decode error if it ever appears.
//! - **Q3** record links: the driver `RecordId` impl accepts record values, string
//!   `"table:id"` and object `{tb,id}` shapes; [`crate::ids::coerce_record_id`]
//!   covers the remaining string-level legacy shapes. **Write path (live
//!   finding): bind `Value::RecordId` for typed record fields — the server
//!   rejects string coercion (`Expected record<repo>, found 'repo:x'`), and
//!   `CREATE $id` is not supported, so record targets are inlined identifiers
//!   built via [`crate::ids::format_record_id`].**
//! - **Q4/Q5** angle-bracket / numeric IDs: preserved by [`crate::ids`].
//! - **Q6** datetimes: driver [`Datetime`] decodes datetime-kind values and renders
//!   RFC3339 — used as-is.
//! - **Q7** `SELECT VALUE id`: [`decode_select_value_ids`].
//! - **Q12** embeddings: [`Embedding`] (Vec<f32>) both directions; HNSW dim 768 COSINE
//!   lives in the DDL.
//!
//! Rule: no `serde_json::Value` hand-digging anywhere above this module — the
//! tagged JSON view of `Value` is never decoded manually; decode through typed
//! structs only.

pub use surrealdb::types::{
    Datetime, Decimal, Number, Object, RecordId, RecordIdKey, SurrealValue, Table, ToSql, Value,
};

use crate::error::DbError;
use crate::ids;

/// Quirk-tolerant `f64` (Q1): accepts `Int | Float | Decimal`, stores as `Float`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct F64(pub f64);

impl F64 {
    pub fn new(v: f64) -> Self {
        F64(v)
    }
    pub fn get(self) -> f64 {
        self.0
    }
}

impl From<f64> for F64 {
    fn from(v: f64) -> Self {
        F64(v)
    }
}

impl From<F64> for f64 {
    fn from(v: F64) -> f64 {
        v.0
    }
}

impl std::fmt::Display for F64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SurrealValue for F64 {
    fn kind_of() -> surrealdb::types::Kind {
        surrealdb::types::Kind::Number
    }

    fn is_value(value: &Value) -> bool {
        matches!(value, Value::Number(_))
    }

    fn into_value(self) -> Value {
        Value::Number(Number::Float(self.0))
    }

    fn from_value(value: Value) -> Result<Self, surrealdb::types::Error> {
        let Value::Number(n) = value else {
            return Err(number_conversion_error(&value, "f64"));
        };
        n.to_f64()
            .map(F64)
            .ok_or_else(|| surrealdb::types::Error::internal(format!("cannot coerce {n} to f64")))
    }
}

/// Quirk-tolerant `i64` (Q1): accepts `Int | Float | Decimal`, validates integrality.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct I64(pub i64);

impl I64 {
    pub fn new(v: i64) -> Self {
        I64(v)
    }
    pub fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for I64 {
    fn from(v: i64) -> Self {
        I64(v)
    }
}

impl From<I64> for i64 {
    fn from(v: I64) -> i64 {
        v.0
    }
}

impl std::fmt::Display for I64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SurrealValue for I64 {
    fn kind_of() -> surrealdb::types::Kind {
        surrealdb::types::Kind::Number
    }

    fn is_value(value: &Value) -> bool {
        matches!(value, Value::Number(_))
    }

    fn into_value(self) -> Value {
        Value::Number(Number::Int(self.0))
    }

    fn from_value(value: Value) -> Result<Self, surrealdb::types::Error> {
        let Value::Number(n) = value else {
            return Err(number_conversion_error(&value, "i64"));
        };
        // Validate integrality: floats/decimals must be whole numbers (Q1).
        let integral = match n {
            Number::Int(_) => true,
            other => other.to_f64().map(|f| f.fract() == 0.0).unwrap_or(false),
        };
        if !integral {
            return Err(surrealdb::types::Error::internal(format!(
                "expected integral number, got {n}"
            )));
        }
        n.to_int()
            .map(I64)
            .ok_or_else(|| surrealdb::types::Error::internal(format!("cannot coerce {n} to i64")))
    }
}

/// Embedding vector (Q12): `array<float>` in rows, JSON vector in writes.
///
/// Stored server-side as f32-precision floats behind the HNSW
/// `DIMENSION 768 DIST COSINE` indexes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Embedding(pub Vec<f32>);

impl From<Vec<f32>> for Embedding {
    fn from(v: Vec<f32>) -> Self {
        Embedding(v)
    }
}

impl SurrealValue for Embedding {
    fn kind_of() -> surrealdb::types::Kind {
        surrealdb::types::Kind::Array(Box::new(surrealdb::types::Kind::Number), None)
    }

    fn is_value(value: &Value) -> bool {
        matches!(value, Value::Array(a) if a.iter().all(|v| matches!(v, Value::Number(_))))
    }

    fn into_value(self) -> Value {
        let arr: Vec<Value> = self
            .0
            .into_iter()
            .map(|f| Value::Number(Number::Float(f64::from(f))))
            .collect();
        Value::from_t(arr)
    }

    fn from_value(value: Value) -> Result<Self, surrealdb::types::Error> {
        let Value::Array(arr) = value else {
            return Err(number_conversion_error(&value, "embedding"));
        };
        let mut out = Vec::with_capacity(arr.len());
        for v in arr.into_iter() {
            let Value::Number(n) = v else {
                return Err(number_conversion_error(&v, "embedding element"));
            };
            let f = n.to_f64().filter(|f| f.is_finite()).ok_or_else(|| {
                surrealdb::types::Error::internal(format!("cannot coerce {n} to f32"))
            })?;
            out.push(f as f32);
        }
        Ok(Embedding(out))
    }
}

/// Decodes one raw value into a typed row, with the SQL excerpt attached on failure.
pub fn decode_row<T: SurrealValue>(value: Value, sql: &str) -> Result<T, DbError> {
    T::from_value(value).map_err(|e| DbError::decode(e, sql))
}

/// Decodes a statement result into typed rows.
///
/// Accepts both the array shape (`SELECT * FROM t`) and a single-object shape
/// (`SELECT * FROM t:id`); `NONE` yields an empty vector.
pub fn decode_row_list<T: SurrealValue>(value: &Value, sql: &str) -> Result<Vec<T>, DbError> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .cloned()
            .map(|v| decode_row::<T>(v, sql))
            .collect(),
        Value::None | Value::Null => Ok(Vec::new()),
        other => Ok(vec![decode_row::<T>(other.clone(), sql)?]),
    }
}

/// Decodes a single typed value from a statement result (e.g. `SELECT count`).
pub fn decode_single<T: SurrealValue>(value: &Value, sql: &str) -> Result<T, DbError> {
    decode_row::<T>(value.clone(), sql)
}

/// Decodes the result of `SELECT VALUE id FROM …` (Q7) into canonical record-ID
/// strings, consistent with [`crate::ids::format_record_id`] outputs.
pub fn decode_select_value_ids(value: &Value, sql: &str) -> Result<Vec<String>, DbError> {
    let Value::Array(arr) = value else {
        return Err(DbError::decode("expected array of ids", sql));
    };
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        match v {
            Value::RecordId(r) => out.push(ids::format_record(r)),
            Value::String(s) => out.push(s.clone()),
            other => {
                return Err(DbError::decode(
                    format!("expected record id, got {}", other.kind()),
                    sql,
                ));
            }
        }
    }
    Ok(out)
}

fn number_conversion_error(value: &Value, target: &str) -> surrealdb::types::Error {
    surrealdb::types::ConversionError::from_value(surrealdb::types::Kind::Number, value)
        .with_context(format!("cannot decode {target}"))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f64_accepts_decimal_int_and_float() {
        // Q1: SurrealQL literal `0.7` arrives as Number::Decimal.
        let decimal = Value::Number(Number::Decimal(Decimal::new(70, 2)));
        let decoded = F64::from_value(decimal).expect("decimal");
        assert_eq!(decoded.get(), 0.7);

        let int = F64::from_value(Value::Number(Number::Int(12))).expect("int");
        assert_eq!(int.get(), 12.0);

        let float = F64::from_value(Value::Number(Number::Float(0.25))).expect("float");
        assert_eq!(float.get(), 0.25);

        let err = F64::from_value(Value::String("x".into()));
        assert!(err.is_err());
    }

    #[test]
    fn i64_validates_integrality() {
        assert_eq!(
            I64::from_value(Value::Number(Number::Int(7)))
                .unwrap()
                .get(),
            7
        );
        assert_eq!(
            I64::from_value(Value::Number(Number::Float(12.0)))
                .unwrap()
                .get(),
            12
        );
        assert!(I64::from_value(Value::Number(Number::Float(12.5))).is_err());
    }

    #[test]
    fn embedding_roundtrip() {
        let e = Embedding(vec![0.1f32, -0.2, 3.5]);
        let v = e.clone().into_value();
        let back = Embedding::from_value(v).expect("embedding");
        assert_eq!(back, e);
        assert!(!Embedding::is_value(&Value::String("no".into())));
    }

    #[test]
    fn decode_ids_from_record_and_string_shapes() {
        let rows = Value::from_t(vec![
            Value::RecordId(RecordId::new("commit", "abc123")),
            Value::RecordId(RecordId::new("issue", RecordIdKey::Number(42))),
            Value::from_t("memory:⟨ws_1⟩"),
        ]);
        let ids = decode_select_value_ids(&rows, "SELECT VALUE id FROM commit;").expect("ids");
        assert_eq!(ids, vec!["commit:⟨abc123⟩", "issue:42", "memory:⟨ws_1⟩"]);
    }
}
