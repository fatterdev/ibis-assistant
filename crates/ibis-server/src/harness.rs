//! G20 — runtime harness profiles: resolve `clientInfo.name` to the ordered
//! `harness.profiles` list and expose the sampling policy plus the hidden
//! tool groups. The default profile is the current behavior.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use ibis_core::config::{HarnessConfig, HarnessProfile};

use crate::registry::{canonical_group, group_of};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingPolicy {
    Auto,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHarnessProfile {
    pub name: String,
    pub sampling: SamplingPolicy,
    pub hidden_groups: Vec<&'static str>,
}

impl ResolvedHarnessProfile {
    /// True when the profile hides the group the tool belongs to.
    pub fn hides(&self, tool_name: &str) -> bool {
        group_of(tool_name).is_some_and(|group| self.hidden_groups.contains(&group))
    }

    pub fn hides_group(&self, group: &str) -> bool {
        self.hidden_groups.contains(&group)
    }

    pub fn default_for(cfg: &HarnessConfig) -> Self {
        resolve(cfg, None)
    }
}

/// Warns once per distinct anomaly: resolution runs per request, and an
/// invalid config must not spam the log.
fn warn_once(key: String, message: String) {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    if warned.lock().unwrap().insert(key) {
        tracing::warn!("{message}");
    }
}

fn profile_label(profile: &HarnessProfile, fallback: &str) -> String {
    if !profile.name.trim().is_empty() {
        profile.name.trim().to_string()
    } else if let Some(first) = profile.match_names.first() {
        let first = first.trim();
        if first.is_empty() {
            fallback.to_string()
        } else {
            first.to_string()
        }
    } else {
        fallback.to_string()
    }
}

/// First profile whose `match` substring is contained in the normalized
/// `clientInfo.name`; `None`/unknown → default. Pure and testable.
pub fn resolve(cfg: &HarnessConfig, client_name: Option<&str>) -> ResolvedHarnessProfile {
    let normalized = client_name
        .map(|name| name.trim().to_lowercase())
        .unwrap_or_default();
    let profile = cfg
        .profiles
        .iter()
        .find(|profile| {
            profile.match_names.iter().any(|needle| {
                let needle = needle.trim().to_lowercase();
                !needle.is_empty() && normalized.contains(&needle)
            })
        })
        .unwrap_or(&cfg.default);
    let label = profile_label(profile, "default");

    let sampling = match profile.sampling.trim().to_lowercase().as_str() {
        "never" => SamplingPolicy::Never,
        "auto" | "" => SamplingPolicy::Auto,
        other => {
            warn_once(
                format!("sampling:{other}"),
                format!("harness profile '{label}': unknown sampling '{other}'; using auto"),
            );
            SamplingPolicy::Auto
        }
    };

    let mut hidden_groups: Vec<&'static str> = Vec::new();
    for group in &profile.hidden_tool_groups {
        match canonical_group(group.trim()) {
            Some(group) => {
                if !hidden_groups.contains(&group) {
                    hidden_groups.push(group);
                }
            }
            None => warn_once(
                format!("group:{}", group.trim()),
                format!(
                    "harness profile '{label}': unknown tool group '{}'; ignored",
                    group.trim()
                ),
            ),
        }
    }

    ResolvedHarnessProfile {
        name: label,
        sampling,
        hidden_groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(matches: &[&str], name: &str, sampling: &str, groups: &[&str]) -> HarnessProfile {
        HarnessProfile {
            match_names: matches.iter().map(|value| value.to_string()).collect(),
            name: name.to_string(),
            sampling: sampling.to_string(),
            hidden_tool_groups: groups.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn config(profiles: Vec<HarnessProfile>) -> HarnessConfig {
        HarnessConfig {
            default: HarnessProfile::default(),
            profiles,
        }
    }

    #[test]
    fn profile_matrix() {
        let cfg = config(vec![
            profile(&["visual studio code"], "vscode", "auto", &[]),
            profile(&["cursor"], "cursor", "auto", &[]),
            profile(&["copilot"], "copilot", "never", &["settings"]),
        ]);
        assert_eq!(resolve(&cfg, Some("Visual Studio Code")).name, "vscode");
        assert_eq!(resolve(&cfg, Some("Cursor")).name, "cursor");
        assert_eq!(resolve(&cfg, Some("copilot-cli")).name, "copilot");
        assert_eq!(resolve(&cfg, Some("unknown")).name, "default");
        assert_eq!(resolve(&cfg, None).name, "default");
    }

    #[test]
    fn first_match_wins() {
        let cfg = config(vec![
            profile(&["code"], "first", "auto", &[]),
            profile(&["visual studio code"], "second", "auto", &[]),
        ]);
        assert_eq!(resolve(&cfg, Some("Visual Studio Code")).name, "first");
    }

    #[test]
    fn unknown_sampling_falls_back_to_auto() {
        let cfg = config(vec![profile(&["cursor"], "cursor", "sometimes", &[])]);
        assert_eq!(resolve(&cfg, Some("cursor")).sampling, SamplingPolicy::Auto);
    }

    #[test]
    fn unknown_group_is_ignored() {
        let cfg = config(vec![profile(&["cursor"], "cursor", "auto", &["nope"])]);
        assert!(resolve(&cfg, Some("cursor")).hidden_groups.is_empty());
    }

    #[test]
    fn default_profile_hides_nothing() {
        let resolved = ResolvedHarnessProfile::default_for(&HarnessConfig::default());
        assert_eq!(resolved.sampling, SamplingPolicy::Auto);
        assert!(resolved.hidden_groups.is_empty());
        assert!(!resolved.hides("settings_get"));
    }

    #[test]
    fn client_name_is_normalized() {
        let cfg = config(vec![profile(&["cursor"], "cursor", "auto", &[])]);
        assert_eq!(resolve(&cfg, Some("  CURSOR \n")).name, "cursor");
        assert_eq!(resolve(&cfg, Some("CurSoR")).name, "cursor");
    }

    #[test]
    fn hidden_group_hides_only_its_tools() {
        let cfg = config(vec![profile(
            &["copilot"],
            "copilot",
            "never",
            &["settings", "optimize"],
        )]);
        let resolved = resolve(&cfg, Some("copilot"));
        assert_eq!(resolved.sampling, SamplingPolicy::Never);
        assert!(resolved.hides("settings_get"));
        assert!(resolved.hides("settings_apply"));
        assert!(resolved.hides("settings_open_ui"));
        assert!(resolved.hides("optimize_knowledge"));
        assert!(!resolved.hides("memory_search"));
        assert!(!resolved.hides("ticket_search"));
        assert!(!resolved.hides_group("ticketing"));
    }

    #[test]
    fn ticketing_group_is_addressable() {
        let cfg = config(vec![profile(
            &["copilot"],
            "copilot",
            "auto",
            &["ticketing"],
        )]);
        let resolved = resolve(&cfg, Some("copilot"));
        assert!(resolved.hides("ticket_search"));
        assert!(!resolved.hides("memory_search"));
    }
}
