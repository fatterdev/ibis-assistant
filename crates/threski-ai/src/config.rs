//! Crate-local mirror of the flat `config.json` `ai{…}` section
//! (
//! + ).
//!
//! NOTE: this duplicates the public config owned by `threski-core` on purpose —
//! this crate must not depend on other crates. The nesting is authoritative:
//! `ai.reasoning.clouds.gemini` is an **object containing `keys`**, not a bare
//! array; a flattened codec would reject existing Gemini configurations.

use std::collections::HashMap;

use serde::Deserialize;

use crate::types::KeyConfig;

pub(crate) const DEFAULT_MODEL_S: &str = "granite4.1:3b";
pub(crate) const DEFAULT_MODEL_M: &str = "qwen2.5-coder:7b";
pub(crate) const DEFAULT_MODEL_L: &str = "gemma4:12b";
pub(crate) const DEFAULT_MODEL_XL: &str = "muse-glimmer";
pub(crate) const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub(crate) const DEFAULT_OPENAI_MODEL: &str = "gpt-4.1";
pub(crate) const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4";
pub(crate) const DEFAULT_FALLBACK_ORDER: [&str; 3] = ["gemini", "openai_compat", "claude"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub embedding: EmbeddingConfig,
    pub reasoning: ReasoningConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub provider: String,
    pub model: String,
    pub url: String,
    pub auto_start: bool,
    pub auto_update: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    /// hybrid | ollama | gemini | openai_compat | claude | cursor | none
    pub provider: String,
    /// `auto` or an explicit model name
    pub model: String,
    /// legacy `keys[]`, migrated into `clouds.gemini.keys`
    pub keys: Vec<GeminiKeyConfig>,
    pub clouds: CloudsConfig,
    pub routing: RoutingConfig,
    /// S/M/L/XL local model overrides
    pub model_overrides: HashMap<String, String>,
    pub context_only_fallback: bool,
    pub always_smallest_local: bool,
    pub use_ide_provided_models: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CloudsConfig {
    pub gemini: GeminiCloudConfig,
    pub openai_compat: OpenAICompatCloudConfig,
    pub claude: ClaudeCloudConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GeminiCloudConfig {
    pub keys: Vec<GeminiKeyConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GeminiKeyConfig {
    pub key: String,
    pub rpm: i64,
    pub tpm: i64,
    pub rpd: i64,
    pub owner: String,
    pub allow_overage: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OpenAICompatCloudConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ClaudeCloudConfig {
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub mode: String,
    pub local_provider: String,
    /// auto | gemini | openai_compat | claude
    pub cloud_provider: String,
    pub cloud_fallback_order: Vec<String>,
    pub use_cloud_when_no_gpu: bool,
    pub local_timeout_ms: i64,
}

impl From<&GeminiKeyConfig> for KeyConfig {
    fn from(c: &GeminiKeyConfig) -> Self {
        KeyConfig {
            key: c.key.clone(),
            rpm: c.rpm,
            tpm: c.tpm,
            rpd: c.rpd,
            owner: c.owner.clone(),
            flush_interval: None,
            allow_overage: c.allow_overage,
        }
    }
}

impl ReasoningConfig {
    /// Scenario: NormalizeReasoningConfig : migrates
    /// legacy keys into clouds, fills routing defaults and model overrides.
    pub fn normalize(&mut self, default_rpm: i64) {
        if self.provider.trim().is_empty() {
            self.provider = "hybrid".to_string();
        }
        if self.model.trim().is_empty() {
            self.model = "auto".to_string();
        }

        if self.clouds.gemini.keys.is_empty() && !self.keys.is_empty() {
            self.clouds.gemini.keys = self.keys.clone();
        }
        if self.keys.is_empty() && !self.clouds.gemini.keys.is_empty() {
            self.keys = self.clouds.gemini.keys.clone();
        }
        for key in &mut self.clouds.gemini.keys {
            if key.rpm <= 0 && default_rpm > 0 {
                key.rpm = default_rpm;
            }
        }

        if self.routing.mode.is_empty() {
            self.routing.mode = "auto".to_string();
        }
        if self.routing.local_provider.is_empty() {
            self.routing.local_provider = "ollama".to_string();
        }
        if self.routing.cloud_provider.is_empty() {
            self.routing.cloud_provider = "auto".to_string();
        }
        if self.routing.cloud_fallback_order.is_empty() {
            self.routing.cloud_fallback_order = DEFAULT_FALLBACK_ORDER
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        if self.routing.local_timeout_ms <= 0 {
            self.routing.local_timeout_ms = 120_000;
        }

        let defaults: [(&str, &str); 4] = [
            ("S", DEFAULT_MODEL_S),
            ("M", DEFAULT_MODEL_M),
            ("L", DEFAULT_MODEL_L),
            ("XL", DEFAULT_MODEL_XL),
        ];
        for (tier, value) in defaults {
            let empty = self
                .model_overrides
                .get(tier)
                .is_none_or(|v| v.trim().is_empty());
            if empty {
                self.model_overrides
                    .insert(tier.to_string(), value.to_string());
            }
        }

        if self.clouds.openai_compat.base_url.trim().is_empty() {
            self.clouds.openai_compat.base_url = DEFAULT_OPENAI_BASE_URL.to_string();
        }
        if self.clouds.openai_compat.model.trim().is_empty() {
            self.clouds.openai_compat.model = DEFAULT_OPENAI_MODEL.to_string();
        }
        if self.clouds.claude.model.trim().is_empty() {
            self.clouds.claude.model = DEFAULT_CLAUDE_MODEL.to_string();
        }
    }

    /// Gemini keys from `clouds` or the legacy `keys` field
    ///
    pub fn gemini_keys_effective(&self) -> &[GeminiKeyConfig] {
        if !self.clouds.gemini.keys.is_empty() {
            &self.clouds.gemini.keys
        } else {
            &self.keys
        }
    }

    /// Whether a named cloud has usable credentials.
    pub fn cloud_has_credentials(&self, name: &str) -> bool {
        match name.trim().to_lowercase().as_str() {
            "gemini" => self
                .gemini_keys_effective()
                .iter()
                .any(|k| !k.key.trim().is_empty()),
            "openai_compat" | "openai" => !self.clouds.openai_compat.api_key.trim().is_empty(),
            "claude" | "anthropic" => !self.clouds.claude.api_key.trim().is_empty(),
            _ => false,
        }
    }

    /// Cloud names with credentials, in fallback order.
    pub fn configured_clouds(&self) -> Vec<String> {
        let order: &[String] = if self.routing.cloud_fallback_order.is_empty() {
            &fallback_order_default()
        } else {
            &self.routing.cloud_fallback_order
        };
        order
            .iter()
            .filter(|name| self.cloud_has_credentials(name))
            .cloned()
            .collect()
    }

    /// Primary cloud id for routing.
    pub fn resolve_cloud_primary(&self) -> String {
        let forced = self.routing.cloud_provider.trim().to_lowercase();
        if !forced.is_empty() && forced != "auto" && self.cloud_has_credentials(&forced) {
            return forced;
        }
        self.configured_clouds()
            .first()
            .cloned()
            .unwrap_or_default()
    }
}

fn fallback_order_default() -> Vec<String> {
    DEFAULT_FALLBACK_ORDER
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_ai_json() -> &'static str {
        r#"{
          "embedding": {
            "provider": "ollama",
            "model": "nomic-embed-text",
            "url": "http://127.0.0.1:11434",
            "auto_start": true,
            "auto_update": true
          },
          "reasoning": {
            "provider": "hybrid",
            "model": "auto",
            "context_only_fallback": true,
            "clouds": {
              "gemini": {
                "keys": [
                  {"key": "AIZA", "rpm": 15, "tpm": 30000, "rpd": 1500, "owner": "default", "allow_overage": true}
                ]
              },
              "openai_compat": {
                "base_url": "https://api.openai.com/v1",
                "api_key": "",
                "model": "gpt-4.1"
              },
              "claude": {"api_key": "", "model": "claude-sonnet-4"}
            },
            "routing": {
              "mode": "auto",
              "local_provider": "ollama",
              "cloud_provider": "auto",
              "cloud_fallback_order": ["gemini", "openai_compat", "claude"],
              "use_cloud_when_no_gpu": true,
              "local_timeout_ms": 120000
            },
            "model_overrides": {"S": "granite4.1:3b", "M": "qwen2.5-coder:7b", "L": "gemma4:12b", "XL": "muse-glimmer"}
          }
        }"#
    }

    #[test]
    fn parses_config_master_nesting() {
        let cfg: AiConfig = serde_json::from_str(full_ai_json()).unwrap();
        assert_eq!(cfg.embedding.model, "nomic-embed-text");
        assert!(cfg.embedding.auto_start);
        assert!(cfg.reasoning.context_only_fallback);
        assert_eq!(cfg.reasoning.clouds.gemini.keys.len(), 1);
        assert_eq!(cfg.reasoning.clouds.gemini.keys[0].rpm, 15);
        assert!(cfg.reasoning.clouds.gemini.keys[0].allow_overage);
        assert!(cfg.reasoning.routing.use_cloud_when_no_gpu);
        assert_eq!(cfg.reasoning.routing.local_timeout_ms, 120_000);
        assert_eq!(cfg.reasoning.model_overrides["XL"], "muse-glimmer");
    }

    #[test]
    fn flattened_gemini_array_is_rejected() {
        let bad = r#"{"reasoning":{"clouds":{"gemini":[{"key":"k"}]}}}"#;
        assert!(serde_json::from_str::<AiConfig>(bad).is_err());
    }

    #[test]
    fn normalize_fills_defaults() {
        let mut cfg: ReasoningConfig = serde_json::from_str("{}").unwrap();
        cfg.normalize(15);
        assert_eq!(cfg.provider, "hybrid");
        assert_eq!(cfg.model, "auto");
        assert_eq!(cfg.routing.mode, "auto");
        assert_eq!(cfg.routing.local_provider, "ollama");
        assert_eq!(cfg.routing.cloud_provider, "auto");
        assert_eq!(
            cfg.routing.cloud_fallback_order,
            vec!["gemini", "openai_compat", "claude"]
        );
        assert_eq!(cfg.routing.local_timeout_ms, 120_000);
        assert_eq!(cfg.model_overrides["S"], "granite4.1:3b");
        assert_eq!(cfg.model_overrides["M"], "qwen2.5-coder:7b");
        assert_eq!(cfg.model_overrides["L"], "gemma4:12b");
        assert_eq!(cfg.model_overrides["XL"], "muse-glimmer");
        assert_eq!(
            cfg.clouds.openai_compat.base_url,
            "https://api.openai.com/v1"
        );
        assert_eq!(cfg.clouds.openai_compat.model, "gpt-4.1");
        assert_eq!(cfg.clouds.claude.model, "claude-sonnet-4");
    }

    #[test]
    fn normalize_migrates_legacy_keys_into_clouds() {
        let mut cfg: ReasoningConfig =
            serde_json::from_str(r#"{"keys":[{"key":"K1","rpm":0,"tpm":1,"rpd":2,"owner":"o"}]}"#)
                .unwrap();
        cfg.normalize(10);
        assert_eq!(cfg.clouds.gemini.keys.len(), 1);
        assert_eq!(cfg.clouds.gemini.keys[0].key, "K1");
        assert_eq!(cfg.clouds.gemini.keys[0].rpm, 10);
        assert_eq!(cfg.keys.len(), 1);
    }

    fn reasoning_json() -> &'static str {
        r#"{
            "provider": "hybrid",
            "model": "auto",
            "context_only_fallback": true,
            "clouds": {
              "gemini": {
                "keys": [
                  {"key": "AIZA", "rpm": 15, "tpm": 30000, "rpd": 1500, "owner": "default", "allow_overage": true}
                ]
              },
              "openai_compat": {
                "base_url": "https://api.openai.com/v1",
                "api_key": "",
                "model": "gpt-4.1"
              },
              "claude": {"api_key": "", "model": "claude-sonnet-4"}
            },
            "routing": {
              "mode": "auto",
              "local_provider": "ollama",
              "cloud_provider": "auto",
              "cloud_fallback_order": ["gemini", "openai_compat", "claude"],
              "use_cloud_when_no_gpu": true,
              "local_timeout_ms": 120000
            },
            "model_overrides": {"S": "granite4.1:3b", "M": "qwen2.5-coder:7b", "L": "gemma4:12b", "XL": "muse-glimmer"}
        }"#
    }

    #[test]
    fn cloud_credential_accessors() {
        let mut cfg: ReasoningConfig = serde_json::from_str(reasoning_json()).unwrap();
        assert!(cfg.cloud_has_credentials("gemini"));
        assert!(cfg.cloud_has_credentials("GEMINI "));
        assert!(!cfg.cloud_has_credentials("openai_compat"));
        assert!(!cfg.cloud_has_credentials("claude"));
        cfg.clouds.claude.api_key = "sk-ant".into();
        assert!(cfg.cloud_has_credentials("anthropic"));
        assert_eq!(cfg.configured_clouds(), vec!["gemini", "claude"]);
        assert_eq!(cfg.resolve_cloud_primary(), "gemini");
        cfg.routing.cloud_provider = "claude".into();
        assert_eq!(cfg.resolve_cloud_primary(), "claude");
        cfg.routing.cloud_provider = "openai_compat".into();
        assert_eq!(cfg.resolve_cloud_primary(), "gemini");
    }

    #[test]
    fn empty_config_resolves_no_clouds() {
        let cfg = ReasoningConfig::default();
        assert_eq!(cfg.configured_clouds(), Vec::<String>::new());
        assert_eq!(cfg.resolve_cloud_primary(), "");
        assert!(cfg.gemini_keys_effective().is_empty());
    }
}
