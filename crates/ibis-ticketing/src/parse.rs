//! Provider-fields JSON parsing.

/// Parses the `provider_fields_json` MCP argument into a raw field map
/// Empty input yields an empty map; a
/// non-object JSON value (array, scalar) is an error — decoded into
/// `map[string]interface{}`, which rejects JSON arrays/scalars the same way.
pub fn parse_provider_fields_json(
    raw: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, crate::types::TicketingError> {
    if raw.is_empty() {
        return Ok(serde_json::Map::new());
    }
    serde_json::from_str(raw)
        .map_err(|e| crate::types::TicketingError::InvalidProviderFieldsJson(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Scenario: TestParseProviderFieldsJSON.
    #[test]
    fn parses_object_fields() {
        let fields = parse_provider_fields_json(r#"{"status_id":3,"custom":"x"}"#).unwrap();
        assert_eq!(fields.get("custom").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(
            fields.get("status_id").and_then(serde_json::Value::as_i64),
            Some(3)
        );
    }

    // Scenario: TestParseProviderFieldsJSONEmpty.
    #[test]
    fn empty_input_yields_empty_map() {
        let fields = parse_provider_fields_json("").unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn invalid_json_and_non_object_rejected() {
        assert!(parse_provider_fields_json("{bad").is_err());
        assert!(parse_provider_fields_json("[1,2]").is_err());
        assert!(parse_provider_fields_json("42").is_err());
    }

    #[test]
    fn nested_values_pass_through() {
        let fields = parse_provider_fields_json(r#"{"priority":{"name":"High"}}"#).unwrap();
        assert_eq!(fields.get("priority"), Some(&json!({"name":"High"})));
    }
}
