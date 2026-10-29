//! Reasoning config normalization and cloud credential resolution.

use std::collections::HashMap;

use crate::config::GeminiKeyConfig;

fn model_overrides_defaults() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("S", "granite4.1:3b"),
        ("M", "qwen2.5-coder:7b"),
        ("L", "gemma4:12b"),
        ("XL", "muse-glimmer"),
    ])
}

/// Migrates legacy keys into clouds and fills routing defaults.
pub fn normalize_reasoning_config(r: &mut crate::config::ReasoningConfig, default_rpm: i64) {
    if r.provider.trim().is_empty() {
        r.provider = "hybrid".into();
    }
    if r.model.trim().is_empty() {
        r.model = "auto".into();
    }

    // Migrate legacy Keys / GeminiKeys-shaped data into clouds.gemini.
    if r.clouds.gemini.keys.is_empty() && !r.keys.is_empty() {
        r.clouds.gemini.keys = r.keys.clone();
    }
    // Keep legacy Keys in sync for older code paths that still read them.
    if r.keys.is_empty() && !r.clouds.gemini.keys.is_empty() {
        r.keys = r.clouds.gemini.keys.clone();
    }
    for k in r.clouds.gemini.keys.iter_mut() {
        if k.rpm <= 0 && default_rpm > 0 {
            k.rpm = default_rpm;
        }
    }

    if r.routing.mode.is_empty() {
        r.routing.mode = "auto".into();
    }
    if r.routing.local_provider.is_empty() {
        r.routing.local_provider = "ollama".into();
    }
    if r.routing.cloud_provider.is_empty() {
        r.routing.cloud_provider = "auto".into();
    }
    if r.routing.cloud_fallback_order.is_empty() {
        r.routing.cloud_fallback_order = vec!["gemini", "openai_compat", "claude"]
            .into_iter()
            .map(String::from)
            .collect();
    }
    if r.routing.local_timeout_ms <= 0 {
        r.routing.local_timeout_ms = 120_000;
    }
    // use_cloud_when_no_gpu: default true only via defaults; preserve JSON false.

    let defaults = model_overrides_defaults();
    for (k, v) in defaults {
        if r.model_overrides
            .get(k)
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            r.model_overrides.insert(k.to_string(), v.to_string());
        }
    }
    if r.clouds.openai_compat.base_url.trim().is_empty() {
        r.clouds.openai_compat.base_url = "https://api.openai.com/v1".into();
    }
    if r.clouds.openai_compat.model.trim().is_empty() {
        r.clouds.openai_compat.model = "gpt-4.1".into();
    }
    if r.clouds.claude.model.trim().is_empty() {
        r.clouds.claude.model = "claude-sonnet-4".into();
    }
}

impl crate::config::ReasoningConfig {
    /// Gemini API keys from clouds or legacy Keys.
    pub fn gemini_keys_effective(&self) -> &[GeminiKeyConfig] {
        if !self.clouds.gemini.keys.is_empty() {
            return &self.clouds.gemini.keys;
        }
        &self.keys
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

    /// Cloud names that have credentials, in fallback order.
    pub fn configured_clouds(&self) -> Vec<String> {
        let order: Vec<String> = if self.routing.cloud_fallback_order.is_empty() {
            vec!["gemini", "openai_compat", "claude"]
                .into_iter()
                .map(String::from)
                .collect()
        } else {
            self.routing.cloud_fallback_order.clone()
        };
        order
            .into_iter()
            .filter(|name| self.cloud_has_credentials(name))
            .collect()
    }

    /// The primary cloud id for routing.
    pub fn resolve_cloud_primary(&self) -> String {
        let forced = self.routing.cloud_provider.trim().to_lowercase();
        if !forced.is_empty() && forced != "auto" && self.cloud_has_credentials(&forced) {
            return forced;
        }
        let configured = self.configured_clouds();
        configured.into_iter().next().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CloudsConfig, GeminiCloudConfig, ReasoningConfig};

    #[test]
    fn fills_all_defaults() {
        let mut r = ReasoningConfig::default();
        normalize_reasoning_config(&mut r, 100);
        assert_eq!(r.provider, "hybrid");
        assert_eq!(r.model, "auto");
        assert_eq!(r.routing.mode, "auto");
        assert_eq!(r.routing.local_provider, "ollama");
        assert_eq!(r.routing.cloud_provider, "auto");
        assert_eq!(r.routing.local_timeout_ms, 120_000);
        assert_eq!(r.model_overrides.get("S").unwrap(), "granite4.1:3b");
        assert_eq!(r.clouds.openai_compat.base_url, "https://api.openai.com/v1");
        assert_eq!(r.clouds.openai_compat.model, "gpt-4.1");
        assert_eq!(r.clouds.claude.model, "claude-sonnet-4");
    }

    #[test]
    fn migrates_legacy_keys_into_clouds_and_backfills_rpm() {
        let mut r = ReasoningConfig {
            keys: vec![GeminiKeyConfig {
                key: "k".into(),
                rpm: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        normalize_reasoning_config(&mut r, 55);
        assert_eq!(r.clouds.gemini.keys.len(), 1);
        assert_eq!(r.clouds.gemini.keys[0].rpm, 55);
        // Legacy Keys are backfilled from clouds BEFORE the RPM loop, so the
        // backfilled copy keeps its original rpm (parity).
        assert_eq!(r.keys[0].rpm, 0);
    }

    #[test]
    fn preserves_existing_cloud_rpm() {
        let mut r = ReasoningConfig {
            clouds: CloudsConfig {
                gemini: GeminiCloudConfig {
                    keys: vec![GeminiKeyConfig {
                        key: "k".into(),
                        rpm: 30,
                        ..Default::default()
                    }],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        normalize_reasoning_config(&mut r, 100);
        assert_eq!(r.clouds.gemini.keys[0].rpm, 30);
        assert_eq!(r.keys[0].rpm, 30);
    }

    #[test]
    fn cloud_credential_queries() {
        let mut r = ReasoningConfig::default();
        assert!(!r.cloud_has_credentials("gemini"));
        assert!(!r.cloud_has_credentials("openai"));
        assert!(!r.cloud_has_credentials("anthropic"));
        assert!(r.configured_clouds().is_empty());
        assert_eq!(r.resolve_cloud_primary(), "");

        r.clouds.claude.api_key = "ck".into();
        assert!(r.cloud_has_credentials("claude"));
        assert_eq!(r.configured_clouds(), vec!["claude".to_string()]);
        assert_eq!(r.resolve_cloud_primary(), "claude");
    }

    #[test]
    fn forced_cloud_only_when_credentialed() {
        let mut r = ReasoningConfig::default();
        r.routing.cloud_provider = "gemini".into();
        assert_eq!(r.resolve_cloud_primary(), "");
        r.clouds.openai_compat.api_key = "ok".into();
        r.routing.cloud_provider = "openai_compat".into();
        assert_eq!(r.resolve_cloud_primary(), "openai_compat");
    }
}
