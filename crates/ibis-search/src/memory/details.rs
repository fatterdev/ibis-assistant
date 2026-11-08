//! Typed memory `details` (C7, ddofds fase 2 parte 3): the per-kind
//! structured attributes stored next to the free-text memory and filterable
//! at retrieval.
//!
//! Rubric (L3 §C7 — required keys are enforced, the rest is optional):
//! - `decision`   → `scope` (required), `alternatives[]`, `tradeoffs`
//! - `fix`        → `root_cause` (required), `symptom`, `remedy`
//! - `constraint` → `applies_to[]` (required), `until`
//! - `rule`       → `enforcement` (required)
//!
//! The stored shape is `details: option<object> FLEXIBLE`; this module owns
//! the boundary conversion (tool JSON → bound Surreal values → dotted field
//! paths) so the rest of the memory lane never hand-digests JSON.

use ibis_db::value::{Number, Value};

/// Policy behind `memory.schema_enforcement` (`warn` | `strict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaEnforcement {
    /// Log a warning and accept the partial `details` (pre-1.0 rollout).
    #[default]
    Warn,
    /// Reject the write when a per-kind required key is missing.
    Strict,
}

impl SchemaEnforcement {
    /// Parses the config value (`None` = unknown).
    pub fn parse(value: &str) -> Option<SchemaEnforcement> {
        match value.trim().to_lowercase().as_str() {
            "" | "warn" => Some(SchemaEnforcement::Warn),
            "strict" => Some(SchemaEnforcement::Strict),
            _ => None,
        }
    }

    /// Resolves the config value; unknown non-empty values log a warning and
    /// fall back to the warn default.
    pub fn resolve(value: &str) -> SchemaEnforcement {
        match SchemaEnforcement::parse(value) {
            Some(mode) => mode,
            None => {
                if !value.trim().is_empty() {
                    tracing::warn!("unknown memory.schema_enforcement value {value:?}; using warn");
                }
                SchemaEnforcement::Warn
            }
        }
    }

    /// Config-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            SchemaEnforcement::Warn => "warn",
            SchemaEnforcement::Strict => "strict",
        }
    }
}

/// The minimal required key per kind (the L3 rubric; other rubric keys stay
/// optional so details never has to be complete to be useful).
pub fn required_details_keys(kind: &str) -> &'static [&'static str] {
    match kind.trim().to_lowercase().as_str() {
        "decision" => &["scope"],
        "fix" => &["root_cause"],
        "constraint" => &["applies_to"],
        "rule" => &["enforcement"],
        _ => &[],
    }
}

fn key_is_empty(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Validates the per-kind required keys. `null` details pass (the field is
/// optional); a non-object details value fails.
pub fn validate_memory_details(kind: &str, details: &serde_json::Value) -> Result<(), String> {
    if details.is_null() {
        return Ok(());
    }
    let serde_json::Value::Object(map) = details else {
        return Err("details must be a JSON object".to_string());
    };
    let missing: Vec<&str> = required_details_keys(kind)
        .iter()
        .copied()
        .filter(|key| map.get(*key).is_none_or(key_is_empty))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "details for kind {kind:?} missing required key(s): {}",
            missing.join(", ")
        ))
    }
}

/// Details keys become dotted field names, so they must be identifiers
/// (`[A-Za-z0-9_]+`) — anything else could smuggle SurrealQL through the
/// field path.
pub fn key_is_safe(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn scalar_value(value: &serde_json::Value) -> Option<Value> {
    match value {
        serde_json::Value::String(s) => Some(Value::String(s.clone())),
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Number(Number::Int(i)))
            } else {
                n.as_f64().map(|f| Value::Number(Number::Float(f)))
            }
        }
        _ => None,
    }
}

/// One scalar `details` equality-filter value (string / number / boolean).
pub fn details_filter_value(value: &serde_json::Value) -> Option<Value> {
    scalar_value(value)
}

fn flatten_into(
    details: &serde_json::Value,
    prefix: &str,
    include_null: bool,
    out: &mut Vec<(String, Value)>,
) -> Result<(), String> {
    let serde_json::Value::Object(map) = details else {
        return Err("details must be a JSON object".to_string());
    };
    for (key, value) in map {
        if !key_is_safe(key) {
            return Err(format!(
                "details key {key:?} must match [A-Za-z0-9_]+ (it becomes a field path)"
            ));
        }
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            serde_json::Value::Object(_) => flatten_into(value, &path, include_null, out)?,
            serde_json::Value::Array(items) => {
                let mut converted = Vec::with_capacity(items.len());
                for item in items {
                    let Some(v) = scalar_value(item) else {
                        return Err(format!(
                            "details.{path} arrays may hold only strings, numbers or booleans"
                        ));
                    };
                    converted.push(v);
                }
                out.push((path, Value::from_t(converted)));
            }
            serde_json::Value::Null => {
                if include_null {
                    out.push((path, Value::None));
                }
            }
            other => match scalar_value(other) {
                Some(v) => out.push((path, v)),
                None => return Err(format!("details.{path} has an unsupported value")),
            },
        }
    }
    Ok(())
}

/// Flattens a details object into `(dotted_path, bound_value)` pairs, one per
/// leaf; `null` leaves are skipped. Nested objects flatten recursively
/// (`details.a.b`), arrays must hold scalars.
pub fn details_fields(details: &serde_json::Value) -> Result<Vec<(String, Value)>, String> {
    let mut out = Vec::new();
    flatten_into(details, "", false, &mut out)?;
    Ok(out)
}

/// Like [`details_fields`] but keeps `null` leaves as `Value::None` so an
/// update can clear individual keys (`details.key = NONE`).
pub fn details_fields_including_null(
    details: &serde_json::Value,
) -> Result<Vec<(String, Value)>, String> {
    let mut out = Vec::new();
    flatten_into(details, "", true, &mut out)?;
    Ok(out)
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::None | Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Number(n) => {
            if let Some(i) = n.to_int() {
                serde_json::Value::Number(i.into())
            } else if let Some(f) = n.to_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            }
        }
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        _ => serde_json::Value::Null,
    }
}

/// Decodes a projected `details` value into plain JSON for the API surface.
pub fn details_json_from_value(value: &Value) -> serde_json::Value {
    value_to_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_parses_config_values() {
        assert_eq!(
            SchemaEnforcement::parse("warn"),
            Some(SchemaEnforcement::Warn)
        );
        assert_eq!(
            SchemaEnforcement::parse("STRICT"),
            Some(SchemaEnforcement::Strict)
        );
        assert_eq!(SchemaEnforcement::parse(""), Some(SchemaEnforcement::Warn));
        assert_eq!(SchemaEnforcement::parse("nonsense"), None);
        assert_eq!(
            SchemaEnforcement::resolve("nonsense"),
            SchemaEnforcement::Warn
        );
        assert_eq!(SchemaEnforcement::default(), SchemaEnforcement::Warn);
        assert_eq!(SchemaEnforcement::Warn.as_str(), "warn");
        assert_eq!(SchemaEnforcement::Strict.as_str(), "strict");
    }

    #[test]
    fn required_matrix_is_the_l3_rubric() {
        assert_eq!(required_details_keys("decision"), &["scope"]);
        assert_eq!(required_details_keys("fix"), &["root_cause"]);
        assert_eq!(required_details_keys("constraint"), &["applies_to"]);
        assert_eq!(required_details_keys("rule"), &["enforcement"]);
        assert!(required_details_keys("fact").is_empty());
        assert!(required_details_keys("gotcha").is_empty());
        // Case/whitespace tolerant like the kind normalization.
        assert_eq!(required_details_keys(" Decision "), &["scope"]);
    }

    #[test]
    fn validation_checks_only_the_minimal_key() {
        let full = serde_json::json!({"scope": "billing", "tradeoffs": "x"});
        assert!(validate_memory_details("decision", &full).is_ok());
        // The other rubric keys stay optional.
        assert!(
            validate_memory_details("decision", &serde_json::json!({"scope": "billing"})).is_ok()
        );
        let missing = validate_memory_details("decision", &serde_json::json!({"tradeoffs": "x"}));
        assert!(missing.unwrap_err().contains("scope"));
        // Empty required values count as missing.
        assert!(validate_memory_details("fix", &serde_json::json!({"root_cause": "  "})).is_err());
        assert!(
            validate_memory_details("constraint", &serde_json::json!({"applies_to": []})).is_err()
        );
        // Optional kinds accept anything object-shaped; null passes.
        assert!(validate_memory_details("fact", &serde_json::json!({})).is_ok());
        assert!(validate_memory_details("decision", &serde_json::Value::Null).is_ok());
        assert!(validate_memory_details("decision", &serde_json::json!(["no"])).is_err());
    }

    #[test]
    fn flatten_emits_dotted_paths_and_scalars() {
        let details = serde_json::json!({
            "scope": "billing",
            "tradeoffs": "latency vs cost",
            "alternatives": ["cache", "batch"],
            "nested": {"a": true, "b": 2.5},
            "empty": null
        });
        let fields = details_fields(&details).expect("flatten");
        let paths: Vec<&str> = fields.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"scope"));
        assert!(paths.contains(&"alternatives"));
        assert!(paths.contains(&"nested.a"));
        assert!(paths.contains(&"nested.b"));
        assert!(!paths.contains(&"empty"), "null leaves skip on write");
        let nested_b = fields.iter().find(|(p, _)| p == "nested.b").unwrap();
        assert_eq!(nested_b.1, Value::Number(Number::Float(2.5)));
        let alts = fields.iter().find(|(p, _)| p == "alternatives").unwrap();
        assert!(matches!(&alts.1, Value::Array(a) if a.len() == 2));
    }

    #[test]
    fn flatten_rejects_unsafe_keys_and_non_scalar_arrays() {
        assert!(details_fields(&serde_json::json!({"bad key": 1})).is_err());
        assert!(details_fields(&serde_json::json!({"a; DROP": 1})).is_err());
        assert!(details_fields(&serde_json::json!({"items": [{"x": 1}]})).is_err());
        assert!(details_fields(&serde_json::json!(["not", "an", "object"])).is_err());
    }

    #[test]
    fn including_null_keeps_clears() {
        let fields = details_fields_including_null(&serde_json::json!({"scope": null, "x": 1}))
            .expect("flatten");
        assert_eq!(fields.len(), 2);
        let scope = fields.iter().find(|(p, _)| p == "scope").unwrap();
        assert_eq!(scope.1, Value::None);
    }

    #[test]
    fn json_roundtrip_through_surreal_values() {
        let details = serde_json::json!({
            "scope": "billing",
            "count": 3,
            "ratio": 0.5,
            "ok": true,
            "alternatives": ["a", "b"]
        });
        let fields = details_fields(&details).expect("flatten");
        let mut map = ibis_db::value::Object::new();
        for (path, value) in fields {
            if path == "scope" {
                map.insert("scope".to_string(), value);
            }
        }
        let decoded = details_json_from_value(&Value::Object(map));
        assert_eq!(decoded, serde_json::json!({"scope": "billing"}));
        assert_eq!(
            details_json_from_value(&Value::from_t(vec![
                Value::String("a".into()),
                Value::Number(Number::Int(2)),
            ])),
            serde_json::json!(["a", 2])
        );
        assert_eq!(
            details_json_from_value(&Value::None),
            serde_json::Value::Null
        );
    }
}
