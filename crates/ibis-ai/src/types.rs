use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// SurrealDB table/edge names used by this crate.
pub const TABLE_COMMIT: &str = "commit";
pub const TABLE_FILE_CHUNK: &str = "file_chunk";
pub const TABLE_COMMIT_CHUNK: &str = "commit_chunk";
pub const TABLE_KEY_USAGE: &str = "key_usage";
pub const EDGE_HAS_COMMIT_CHUNK: &str = "has_commit_chunk";

/// Token estimate used by the gemini rate limiter and the cost tracker
/// (`len(t)/3, plus 1 for non-empty text — bytes, not chars).
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 3 + usize::from(!text.is_empty())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Part {
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Content {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content {
            role: String::new(),
            parts: vec![Part { text: text.into() }],
        }
    }

    /// Concatenation of all part texts ( providers join `Parts`).
    pub fn joined_text(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerationConfig {
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub temperature: f64,
    #[serde(
        rename = "maxOutputTokens",
        default,
        skip_serializing_if = "is_zero_usize"
    )]
    pub max_output_tokens: usize,
    /// Route hint for the hybrid router: `bulk` | `quality` | `auto` | `local_only`.
    #[serde(
        rename = "routeHint",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub route_hint: String,
}

fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    pub prompt_token_count: i64,
    #[serde(rename = "candidatesTokenCount", default)]
    pub candidates_token_count: i64,
    #[serde(rename = "totalTokenCount", default)]
    pub total_token_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    #[serde(default)]
    pub content: Content,
    #[serde(rename = "finishReason", default)]
    pub finish_reason: String,
    /// `json:"-" — never serialized (ide-host bridge wire contract).
    #[serde(skip)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Clone, Default)]
pub struct BatchJobStatus {
    pub name: String,
    pub done: bool,
    pub error: Option<String>,
    pub embeddings: Vec<Vec<f32>>,
    pub candidates: Vec<Candidate>,
}

/// One Gemini API key. `allow_overage` bypasses the
/// RPD hard stop; `flush_interval` overrides the 5s usage-persist cadence.
#[derive(Debug, Clone, Default)]
pub struct KeyConfig {
    pub key: String,
    pub rpm: i64,
    pub tpm: i64,
    pub rpd: i64,
    pub owner: String,
    pub flush_interval: Option<std::time::Duration>,
    pub allow_overage: bool,
}

/// `key_usage:<hash8>_<yyyymmdd>` — the SurrealDB record id Gemini usage is
/// persisted under (`key_usage` table; hash = first 8 hex chars of the
/// SHA-256 of the API key; date has NO hyphens — SurrealDB parse errors
/// otherwise, see ).
pub fn usage_id_for_key(key: &str, date: NaiveDate) -> String {
    let sum = Sha256::digest(key.as_bytes());
    let hash: String = sum
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .concat();
    format!("{}:{}_{}", TABLE_KEY_USAGE, hash, date.format("%Y%m%d"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_is_len_over_three_plus_one() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("aaaaaa"), 3); // 6/3 = 2 + 1
        assert_eq!(estimate_tokens(&"a".repeat(60)), 21);
    }

    #[test]
    fn usage_id_matches_reference_format() {
        let date = NaiveDate::from_ymd_opt(2026, 2, 2).unwrap();
        let id = usage_id_for_key("testkey", date);
        assert!(id.starts_with("key_usage:"));
        let (table, rest) = id.split_once(':').unwrap();
        assert_eq!(table, "key_usage");
        let (hash, date_part) = rest.split_once('_').unwrap();
        assert_eq!(hash.len(), 8);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(date_part, "20260202");
        assert!(!date_part.contains('-'));
    }

    #[test]
    fn generation_config_serializes_with_reference_json_tags() {
        let cfg = GenerationConfig {
            temperature: 0.7,
            max_output_tokens: 100,
            route_hint: "quality".into(),
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["temperature"], 0.7);
        assert_eq!(json["maxOutputTokens"], 100);
        assert_eq!(json["routeHint"], "quality");

        let empty = serde_json::to_value(GenerationConfig::default()).unwrap();
        assert_eq!(empty, serde_json::json!({}));
    }

    #[test]
    fn candidate_skips_usage_metadata_on_wire() {
        let cand = Candidate {
            content: Content::text("hi"),
            finish_reason: "STOP".into(),
            usage_metadata: Some(UsageMetadata::default()),
        };
        let json = serde_json::to_value(&cand).unwrap();
        assert!(json.get("usageMetadata").is_none());
        assert_eq!(json["finishReason"], "STOP");
    }

    #[test]
    fn content_roundtrip_and_join() {
        let c = Content {
            role: "model".into(),
            parts: vec![Part { text: "a".into() }],
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["role"], "model");
        assert_eq!(json["parts"][0]["text"], "a");
        let empty = serde_json::to_value(Content::default()).unwrap();
        assert!(empty.get("role").is_none());
        assert_eq!(Content::text("x").joined_text(), "x");
    }
}
