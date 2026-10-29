//! Settings engine.
//!
//! Probes local hardware (Ollama reachability + system GPU), derives the
//! model tier, builds UI/wizard recommendations, and applies user choices to
//! the config (with a diff summary). All doc comments note the original source.
//!
//! F-003: the original GPU probe read
//! `Win32_VideoController.AdapterRAM` — a uint32 CIM property capped around
//! 4 GiB, so large-VRAM adapters were misdetect­ed into the wrong tier. The
//! Rust probe reads the 64-bit `HardwareInformation.qwMemorySize` registry
//! value directly (no WMI spawn), keeping `nvidia-smi` as fallback; tier
//! detection is unit-tested with stubbed probes.

use async_trait::async_trait;
use threski_core::config::{Config, ReasoningConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use crate::gpu::{SystemGpuProbeImpl, NVIDIA_SMI_TIMEOUT, OLLAMA_PROBE_HTTP_TIMEOUT};

/// The local model auto-selection band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    S,
    M,
    L,
    XL,
}

impl Tier {
    /// Scenario: string form (`TierS` == "S", …).
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::S => "S",
            Tier::M => "M",
            Tier::L => "L",
            Tier::XL => "XL",
        }
    }
}

impl Default for Tier {
    /// Scenario: zero-value fallback: the probe always ends with a non-empty tier and
    /// `""` was coerced to `TierS`
    fn default() -> Self {
        Tier::S
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of a best-effort hardware probe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareReport {
    #[serde(rename = "vram_mib")]
    pub vram_mib: i64,
    #[serde(rename = "gpu_name", skip_serializing_if = "String::is_empty", default)]
    pub gpu_name: String,
    /// amd, nvidia, apple, unknown
    #[serde(
        rename = "gpu_vendor",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub gpu_vendor: String,
    /// ROCm, CUDA, Vulkan, cpu, unknown
    #[serde(
        rename = "ollama_library",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub ollama_library: String,
    #[serde(rename = "ollama_reachable")]
    pub ollama_reachable: bool,
    #[serde(rename = "cpu_only")]
    pub cpu_only: bool,
    pub tier: Tier,
    #[serde(rename = "recommended_model")]
    pub recommended_model: String,
    #[serde(
        rename = "diagnostic",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub diagnostic: String,
}

/// One recommended config field with a human reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldRec {
    pub path: String,
    pub value: serde_json::Value,
    pub reason: String,
}

/// The settings-engine output for the UI/wizard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recommendations {
    pub probe: HardwareReport,
    pub fields: Vec<FieldRec>,
    pub summary: String,
}

/// Optional overrides from wizard/GUI. `Option<bool>`
/// fields distinguish "not specified" from explicit `false`.
#[derive(Debug, Clone, Default)]
pub struct UserChoices {
    /// hybrid|ollama|gemini|openai_compat|claude|none
    pub ai_mode: String,
    pub always_smallest_local: bool,
    pub use_cloud_when_no_gpu: Option<bool>,
    pub gemini_key: String,
    pub openai_key: String,
    pub openai_base_url: String,
    pub openai_model: String,
    pub claude_key: String,
    pub claude_model: String,
    /// auto|granite4.1:3b|explicit tag
    pub local_model: String,
    pub discovery_root: String,
    pub logs_root: String,
    pub runtime_mode: String,
    pub working_folder: String,
    pub port: i64,
    pub use_ide_provided_models: Option<bool>,
    pub memory_projection_enabled: Option<bool>,
    /// db_only|stub|in_repo
    pub memory_git_store_personal_claims: String,
}

/// One changed path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub from: String,
    pub to: String,
}

/// Ollama reachability probe result (`probeOllama return tuple).
#[derive(Debug, Clone, Default)]
pub struct OllamaProbeResult {
    pub library: String,
    pub vram_mib: i64,
    pub name: String,
    pub reachable: bool,
}

/// Seam for the Ollama reachability probe — faked in tests (the HTTP call is
/// not injectable; the Rust probe keeps the seam narrow so tier detection is
/// testable end to end).
#[async_trait]
pub trait OllamaProbe: Send + Sync {
    async fn probe(&self, base_url: &str) -> OllamaProbeResult;
}

/// One system-GPU probe result (`probeSystemGPU return tuple).
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub vram_mib: i64,
    pub name: String,
    /// amd, nvidia, intel, unknown
    pub vendor: String,
}

/// Seam for the system-GPU probe — the F-003 test seam: production reads the
/// Windows registry (64-bit `qwMemorySize`) then `nvidia-smi`; tests stub it
/// with fixed values to pin tier detection.
#[async_trait]
pub trait SystemGpuProbe: Send + Sync {
    async fn probe(&self) -> Option<GpuInfo>;
}

/// Production Ollama probe: `GET {base}/api/version` with a 2s timeout
/// (`probeOllama). Ollama exposes no stable public GPU
/// JSON, so the library is left "unknown" and the system probe fills VRAM.
pub struct HttpOllamaProbe;

#[async_trait]
impl OllamaProbe for HttpOllamaProbe {
    async fn probe(&self, base_url: &str) -> OllamaProbeResult {
        let base = if base_url.trim().is_empty() {
            "http://127.0.0.1:11434"
        } else {
            base_url.trim_end_matches('/')
        };
        let client = match reqwest::Client::builder()
            .timeout(OLLAMA_PROBE_HTTP_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(_) => return OllamaProbeResult::default(),
        };
        match client.get(format!("{base}/api/version")).send().await {
            Ok(resp) if resp.status().as_u16() == 200 => OllamaProbeResult {
                library: "unknown".to_string(),
                reachable: true,
                ..Default::default()
            },
            _ => OllamaProbeResult::default(),
        }
    }
}

/// Default hooks used by [`probe`].
pub struct ProductionProbes;

#[async_trait]
impl OllamaProbe for ProductionProbes {
    async fn probe(&self, base_url: &str) -> OllamaProbeResult {
        HttpOllamaProbe.probe(base_url).await
    }
}

#[async_trait]
impl SystemGpuProbe for ProductionProbes {
    async fn probe(&self) -> Option<GpuInfo> {
        crate::gpu::system_gpu_probe().await
    }
}

/// Gathers hardware + Ollama status (`Probe).
///
/// `ollama_url` may be empty (falls back to `http://127.0.0.1:11434`).
pub async fn probe(ollama_url: &str) -> HardwareReport {
    probe_with(ollama_url, &ProductionProbes, Some(&ProductionProbes)).await
}

/// Synchronous variant for CLI contexts outside a tokio runtime (the
/// configuration wizard). Must not be called from inside a runtime.
pub fn probe_blocking(ollama_url: &str) -> HardwareReport {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for hardware probe")
        .block_on(probe(ollama_url))
}

/// [`probe`] with injectable hooks — the F-003 test seam.
pub async fn probe_with(
    ollama_url: &str,
    ollama_probe: &dyn OllamaProbe,
    gpu_probe: Option<&dyn SystemGpuProbe>,
) -> HardwareReport {
    let mut rep = HardwareReport::default();

    let ollama = ollama_probe.probe(ollama_url).await;
    if ollama.reachable {
        rep.ollama_reachable = true;
        rep.ollama_library = ollama.library.clone();
        if !ollama.name.is_empty() {
            rep.gpu_name = ollama.name.clone();
        }
        if ollama.vram_mib > 0 {
            rep.vram_mib = ollama.vram_mib;
        }
        let lib = ollama.library.to_lowercase();
        if lib == "cpu" || ollama.library.is_empty() {
            rep.cpu_only = true;
        } else {
            rep.cpu_only = false;
            if lib.contains("rocm") {
                rep.gpu_vendor = "amd".to_string();
            } else if lib.contains("cuda") {
                rep.gpu_vendor = "nvidia".to_string();
            }
        }
    }

    if rep.vram_mib == 0 {
        match gpu_probe {
            Some(gpu_probe) => match gpu_probe.probe().await {
                Some(gpu) if gpu.vram_mib > 0 => {
                    rep.vram_mib = gpu.vram_mib;
                    if rep.gpu_name.is_empty() {
                        rep.gpu_name = gpu.name.clone();
                    }
                    if rep.gpu_vendor.is_empty() {
                        rep.gpu_vendor = gpu.vendor.clone();
                    }
                    // Discrete GPU present but Ollama on CPU
                    if rep.ollama_reachable
                        && (rep.ollama_library.eq_ignore_ascii_case("cpu")
                            || rep.ollama_library.is_empty())
                    {
                        rep.cpu_only = true;
                        rep.diagnostic = "gpu_present_but_ollama_cpu".to_string();
                    } else if !rep.ollama_reachable {
                        // Assume GPU usable until Ollama says otherwise.
                        rep.cpu_only = false;
                    }
                }
                _ => {
                    if !rep.ollama_reachable {
                        rep.cpu_only = true;
                    }
                }
            },
            None => {
                if !rep.ollama_reachable {
                    rep.cpu_only = true;
                }
            }
        }
    }

    rep.tier = tier_for(rep.vram_mib, rep.cpu_only);
    rep.recommended_model = default_model_for_tier(rep.tier);
    rep
}

/// Tier selection ( — the two nested ifs collapse to the
/// same table: CPU-only is always S; otherwise thresholds by VRAM).
pub fn tier_for(vram_mib: i64, cpu_only: bool) -> Tier {
    if cpu_only {
        return Tier::S;
    }
    if vram_mib >= 22 * 1024 {
        Tier::XL
    } else if vram_mib >= 10 * 1024 {
        Tier::L
    } else if vram_mib >= 6 * 1024 {
        Tier::M
    } else {
        Tier::S
    }
}

/// The product default Ollama tag for a tier.
pub fn default_model_for_tier(tier: Tier) -> String {
    match tier {
        Tier::M => "qwen2.5-coder:7b".to_string(),
        Tier::L => "gemma4:12b".to_string(),
        Tier::XL => "muse-glimmer".to_string(),
        Tier::S => "granite4.1:3b".to_string(),
    }
}

/// Picks the Ollama chat model from config + probe.
pub fn resolve_local_model(r: &ReasoningConfig, probe: &HardwareReport) -> String {
    if r.always_smallest_local {
        if let Some(m) = r.model_overrides.get("S").filter(|m| !m.is_empty()) {
            return m.clone();
        }
        return "granite4.1:3b".to_string();
    }
    let model = r.model.trim();
    if !model.is_empty() && !model.eq_ignore_ascii_case("auto") {
        return model.to_string();
    }
    if let Some(m) = r
        .model_overrides
        .get(probe.tier.as_str())
        .filter(|m| !m.is_empty())
    {
        return m.clone();
    }
    default_model_for_tier(probe.tier)
}

/// Builds recommendations from existing config + probe (`Recommend,
/// ). The original version normalized the caller's config in
/// place; the Rust version normalizes a local clone (same output, no hidden
/// mutation).
pub fn recommend(existing: &Config, probe: &HardwareReport) -> Recommendations {
    let mut cfg = existing.clone();
    threski_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);

    let mut rec = Recommendations {
        probe: probe.clone(),
        fields: Vec::new(),
        summary: String::new(),
    };
    let r = cfg.ai.reasoning.clone();

    let mut provider = r.provider.clone();
    if provider.is_empty() || provider == "gemini" {
        // Suggest hybrid for new UX without forcing apply
        provider = "hybrid".to_string();
        rec.fields.push(FieldRec {
            path: "ai.reasoning.provider".to_string(),
            value: json!("hybrid"),
            reason: "Hybrid is the product default: local for bulk, cloud for hard questions"
                .to_string(),
        });
    }

    let local_model = resolve_local_model(&r, probe);
    rec.fields.push(FieldRec {
        path: "ai.reasoning.resolved_local_model".to_string(),
        value: json!(local_model),
        reason: format!(
            "Tier {} from probe (VRAM≈{} MiB, cpu_only={})",
            probe.tier, probe.vram_mib, probe.cpu_only
        ),
    });

    let primary = r.resolve_cloud_primary();
    if primary.is_empty() {
        rec.fields.push(FieldRec {
            path: "ai.reasoning.routing.cloud_provider".to_string(),
            value: json!("auto"),
            reason:
                "No cloud keys configured yet; hybrid will use local only until you add keys in the wizard or Settings UI"
                    .to_string(),
        });
    } else {
        rec.fields.push(FieldRec {
            path: "ai.reasoning.routing.cloud_provider".to_string(),
            value: json!("auto"),
            reason: format!("Resolved cloud primary: {primary} (from configured keys)"),
        });
    }

    let mut use_cloud = r.routing.use_cloud_when_no_gpu;
    if provider == "hybrid" && r.routing.mode.is_empty() {
        use_cloud = true;
    }
    rec.fields.push(FieldRec {
        path: "ai.reasoning.routing.use_cloud_when_no_gpu".to_string(),
        value: json!(use_cloud),
        reason: "On CPU-only PCs, use the cloud for hard questions".to_string(),
    });

    rec.summary = format!(
        "Tier {} → {}; cloud primary={}; provider={}",
        probe.tier,
        local_model,
        or_empty(&primary, "(none)"),
        provider
    );
    rec
}

fn or_empty<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

/// Merges user choices into a copy of the existing config (`Apply,
/// ). deep-copied through a JSON round-trip which also
/// dropped the non-serialized `config_loaded`/`config_path` bookkeeping — the
/// Rust port mirrors that by clearing them on the copy. Infallible in Rust
/// (the `error return covered only JSON-marshalling, which cannot fail);
/// the vestigial `probe` parameter was dropped (finding K-07).
pub fn apply(existing: &Config, choices: &UserChoices) -> (Config, Vec<DiffEntry>) {
    // shallow copy via JSON round-trip onto defaults.
    let mut cfg = existing.clone();
    let before = cfg.ai.reasoning.clone();
    cfg.config_loaded = false;
    cfg.config_path = String::new();

    if !choices.ai_mode.is_empty() {
        cfg.ai.reasoning.provider = choices.ai_mode.clone();
    } else if cfg.ai.reasoning.provider.is_empty() {
        cfg.ai.reasoning.provider = "hybrid".to_string();
    }
    cfg.ai.reasoning.model = "auto".to_string();
    let local = choices.local_model.trim().to_string();
    if !local.is_empty() && !local.eq_ignore_ascii_case("auto") {
        if local == "granite4.1:3b" || choices.always_smallest_local {
            cfg.ai.reasoning.always_smallest_local = true;
            cfg.ai.reasoning.model = "auto".to_string();
        } else {
            cfg.ai.reasoning.model = local.clone();
            cfg.ai.reasoning.always_smallest_local = false;
        }
    }
    if choices.always_smallest_local {
        cfg.ai.reasoning.always_smallest_local = true;
    }
    if let Some(use_cloud) = choices.use_cloud_when_no_gpu {
        cfg.ai.reasoning.routing.use_cloud_when_no_gpu = use_cloud;
    } else if cfg.ai.reasoning.provider == "hybrid" {
        cfg.ai.reasoning.routing.use_cloud_when_no_gpu = true;
    }
    cfg.ai.reasoning.context_only_fallback = true;
    cfg.ai.reasoning.routing.cloud_provider = "auto".to_string();

    let gemini_key = choices.gemini_key.trim();
    if !gemini_key.is_empty() {
        let keys = vec![threski_core::config::GeminiKeyConfig {
            key: gemini_key.to_string(),
            rpm: cfg.gemini_default_rpm,
            owner: "default".to_string(),
            ..Default::default()
        }];
        cfg.ai.reasoning.clouds.gemini.keys = keys.clone();
        cfg.ai.reasoning.keys = keys.clone();
        cfg.gemini_keys = keys;
    }
    let openai_key = choices.openai_key.trim();
    if !openai_key.is_empty() {
        cfg.ai.reasoning.clouds.openai_compat.api_key = openai_key.to_string();
    }
    let openai_base = choices.openai_base_url.trim();
    if !openai_base.is_empty() {
        cfg.ai.reasoning.clouds.openai_compat.base_url = openai_base.to_string();
    }
    let openai_model = choices.openai_model.trim();
    if !openai_model.is_empty() {
        cfg.ai.reasoning.clouds.openai_compat.model = openai_model.to_string();
    }
    let claude_key = choices.claude_key.trim();
    if !claude_key.is_empty() {
        cfg.ai.reasoning.clouds.claude.api_key = claude_key.to_string();
    }
    let claude_model = choices.claude_model.trim();
    if !claude_model.is_empty() {
        cfg.ai.reasoning.clouds.claude.model = claude_model.to_string();
    }
    if let Some(v) = choices.use_ide_provided_models {
        cfg.ai.reasoning.use_ide_provided_models = v;
    }
    if !choices.discovery_root.is_empty() {
        cfg.discovery_root = choices.discovery_root.clone();
    }
    if !choices.logs_root.is_empty() {
        cfg.logs_root = choices.logs_root.clone();
    }
    if !choices.runtime_mode.is_empty() {
        cfg.runtime_mode = choices.runtime_mode.clone();
    }
    if choices.port > 0 {
        cfg.port = choices.port;
    }
    if !choices.working_folder.is_empty() && threski_core::config::is_live_runtime(&cfg.runtime_mode) {
        cfg.projects = vec![threski_core::config::ProjectConfig {
            name: base_name(&choices.working_folder),
            working_repo_path: choices.working_folder.clone(),
            ..Default::default()
        }];
    }
    if let Some(v) = choices.memory_projection_enabled {
        cfg.memory.projection_enabled = v;
    }
    if !choices.memory_git_store_personal_claims.is_empty() {
        cfg.memory.git_store.personal_claims = choices.memory_git_store_personal_claims.clone();
    }

    threski_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);
    let diff = if before != cfg.ai.reasoning {
        vec![DiffEntry {
            path: "ai.reasoning".to_string(),
            from: "(previous)".to_string(),
            to: "(updated)".to_string(),
        }]
    } else {
        Vec::new()
    };
    (cfg, diff)
}

/// `baseName : basename with normalized separators.
pub(crate) fn base_name(p: &str) -> String {
    let p = p.replace('\\', "/");
    match p.rfind('/') {
        Some(i) if i + 1 < p.len() => p[i + 1..].to_string(),
        _ => p,
    }
}

/// Shows only the last 4 characters (`MaskSecret).
/// Scenario: sliced bytes; the Rust version slices chars so multi-byte secrets stay
/// valid UTF-8 (ASCII behavior identical, incl. the `…` prefix).
pub fn mask_secret(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "****".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fixed Ollama probe stub.
    struct FixedOllama {
        library: String,
        reachable: bool,
    }

    #[async_trait]
    impl OllamaProbe for FixedOllama {
        async fn probe(&self, _base: &str) -> OllamaProbeResult {
            OllamaProbeResult {
                library: self.library.clone(),
                reachable: self.reachable,
                ..Default::default()
            }
        }
    }

    /// Fixed system-GPU probe stub recording call counts (F-003 tier tests).
    struct FixedGpu {
        info: Option<GpuInfo>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl SystemGpuProbe for FixedGpu {
        async fn probe(&self) -> Option<GpuInfo> {
            *self.calls.lock().unwrap() += 1;
            self.info.clone()
        }
    }

    fn no_ollama() -> FixedOllama {
        FixedOllama {
            library: String::new(),
            reachable: false,
        }
    }

    /// F-003: 8 GiB VRAM (64-bit value the original uint32 WMI path could not
    /// report) must select tier M, not S.
    #[tokio::test]
    async fn tier_detection_uses_full_64bit_vram() {
        let gpu = FixedGpu {
            info: Some(GpuInfo {
                vram_mib: 8 * 1024,
                name: "AMD Radeon RX 7900".into(),
                vendor: "amd".into(),
            }),
            calls: Mutex::new(0),
        };
        let rep = probe_with("", &no_ollama(), Some(&gpu)).await;
        assert_eq!(rep.vram_mib, 8 * 1024);
        assert_eq!(rep.tier, Tier::M);
        assert_eq!(rep.recommended_model, "qwen2.5-coder:7b");
        assert_eq!(rep.gpu_vendor, "amd");
        assert!(!rep.cpu_only);
        assert_eq!(*gpu.calls.lock().unwrap(), 1);
    }

    /// F-003 tier matrix with stubbed probes.
    #[tokio::test]
    async fn tier_matrix() {
        let cases = [
            (0i64, true, Tier::S),
            (4 * 1024, false, Tier::S),
            (6 * 1024, false, Tier::M),
            (10 * 1024, false, Tier::L),
            (16 * 1024, false, Tier::L),
            (22 * 1024, false, Tier::XL),
            (48 * 1024, false, Tier::XL),
        ];
        for (vram, cpu, want) in cases {
            assert_eq!(tier_for(vram, cpu), want, "vram={vram} cpu={cpu}");
        }
    }

    /// Ollama reachable with an "unknown" library + discrete GPU: the GPU is
    /// assumed usable — no cpu_only flag.
    #[tokio::test]
    async fn ollama_unknown_library_with_discrete_gpu() {
        let ollama = FixedOllama {
            library: "unknown".into(),
            reachable: true,
        };
        let gpu = FixedGpu {
            info: Some(GpuInfo {
                vram_mib: 24 * 1024,
                name: "NVIDIA GeForce RTX".into(),
                vendor: "nvidia".into(),
            }),
            calls: Mutex::new(0),
        };
        let rep = probe_with("", &ollama, Some(&gpu)).await;
        assert!(!rep.cpu_only);
        assert_eq!(rep.tier, Tier::XL);
        assert!(rep.diagnostic.is_empty());
    }

    /// GPU present but Ollama reports the CPU library → cpu_only + diagnostic
    ///
    #[tokio::test]
    async fn gpu_present_but_ollama_cpu() {
        let ollama = FixedOllama {
            library: "cpu".into(),
            reachable: true,
        };
        let gpu = FixedGpu {
            info: Some(GpuInfo {
                vram_mib: 12 * 1024,
                name: "AMD".into(),
                vendor: "amd".into(),
            }),
            calls: Mutex::new(0),
        };
        let rep = probe_with("", &ollama, Some(&gpu)).await;
        assert!(rep.cpu_only);
        assert_eq!(rep.diagnostic, "gpu_present_but_ollama_cpu");
        assert_eq!(rep.tier, Tier::S);
    }

    /// No probes reachable at all → CPU-only S
    #[tokio::test]
    async fn unreachable_probes_cpu_only() {
        let gpu = FixedGpu {
            info: None,
            calls: Mutex::new(0),
        };
        let rep = probe_with("", &no_ollama(), Some(&gpu)).await;
        assert!(rep.cpu_only);
        assert_eq!(rep.tier, Tier::S);
        assert_eq!(rep.recommended_model, "granite4.1:3b");
    }

    /// Scenario: TestDefaultModelForTier.
    #[test]
    fn default_model_per_tier() {
        assert_eq!(default_model_for_tier(Tier::S), "granite4.1:3b");
        assert_eq!(default_model_for_tier(Tier::XL), "muse-glimmer");
    }

    /// Scenario: TestResolveLocalModelAlwaysSmallest.
    #[test]
    fn resolve_local_model_always_smallest() {
        let r = ReasoningConfig {
            model: "auto".into(),
            always_smallest_local: true,
            model_overrides: std::collections::HashMap::from([
                ("S".to_string(), "granite4.1:3b".to_string()),
                ("L".to_string(), "gemma4:12b".to_string()),
            ]),
            ..Default::default()
        };
        let probe = HardwareReport {
            tier: Tier::L,
            vram_mib: 16 * 1024,
            ..Default::default()
        };
        assert_eq!(resolve_local_model(&r, &probe), "granite4.1:3b");
    }

    /// Scenario: TestRecommendCloudPrimary.
    #[test]
    fn recommend_cloud_primary() {
        let mut cfg = threski_core::config::default_config();
        cfg.ai.reasoning.clouds.gemini.keys = vec![threski_core::config::GeminiKeyConfig {
            key: "abc".into(),
            ..Default::default()
        }];
        threski_core::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, 100);
        assert_eq!(cfg.ai.reasoning.resolve_cloud_primary(), "gemini");
        let probe = HardwareReport {
            tier: Tier::M,
            vram_mib: 8 * 1024,
            ..Default::default()
        };
        let rec = recommend(&cfg, &probe);
        assert!(!rec.summary.is_empty());
        assert_eq!(rec.probe.tier, Tier::M);
    }

    /// Scenario: TestApplyMemoryProjectionEnabled.
    #[test]
    fn apply_memory_projection_enabled() {
        let cfg = threski_core::config::default_config();
        assert!(
            !cfg.memory.projection_enabled,
            "projection disabled by default"
        );
        let (out, _) = apply(
            &cfg,
            &UserChoices {
                memory_projection_enabled: Some(true),
                ..Default::default()
            },
        );
        assert!(out.memory.projection_enabled);
    }

    /// Scenario: TestApplyMemoryGitStorePersonalClaims.
    #[test]
    fn apply_memory_git_store_personal_claims() {
        let cfg = threski_core::config::default_config();
        assert!(
            cfg.memory.git_store.personal_claims.is_empty(),
            "personal_claims empty by default"
        );
        let (out, _) = apply(
            &cfg,
            &UserChoices {
                memory_git_store_personal_claims: "in_repo".into(),
                ..Default::default()
            },
        );
        assert_eq!(out.memory.git_store.personal_claims, "in_repo");
        // Empty choice preserves the existing value
        let (empty, _) = apply(&out, &UserChoices::default());
        assert_eq!(empty.memory.git_store.personal_claims, "in_repo");
    }

    /// Scenario: TestApplyGeminiKey (+ MaskSecret assertions).
    #[test]
    fn apply_gemini_key() {
        let cfg = threski_core::config::default_config();
        let (out, _) = apply(
            &cfg,
            &UserChoices {
                ai_mode: "hybrid".into(),
                gemini_key: "secret-key-xyz".into(),
                ..Default::default()
            },
        );
        assert!(out.ai.reasoning.cloud_has_credentials("gemini"));
        assert!(mask_secret("abcdefgh").contains("efgh"));
        assert_eq!(mask_secret("abcdefgh"), "…efgh");
        assert_eq!(mask_secret("abc"), "****");
        assert_eq!(mask_secret(""), "");
    }

    /// Apply produces the single ai.reasoning diff entry only when the
    /// reasoning section actually changed
    #[test]
    fn apply_diff_entry() {
        let cfg = threski_core::config::default_config();
        let (unchanged, diff) = apply(&cfg.clone(), &UserChoices::default());
        assert!(diff.is_empty(), "no-op apply must not diff");
        assert_eq!(unchanged.config_path, "", "round-trip dropped ConfigPath");
        let (_, diff) = apply(
            &cfg,
            &UserChoices {
                claude_key: "sk".into(),
                ..Default::default()
            },
        );
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].path, "ai.reasoning");
        assert_eq!(diff[0].from, "(previous)");
        assert_eq!(diff[0].to, "(updated)");
    }

    /// Scenario: TestNormalizeLegacyKeys.
    #[test]
    fn normalize_legacy_keys() {
        let mut r = ReasoningConfig {
            provider: "gemini".into(),
            keys: vec![threski_core::config::GeminiKeyConfig {
                key: "k1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        threski_core::reasoning::normalize_reasoning_config(&mut r, 50);
        assert_eq!(r.clouds.gemini.keys.len(), 1);
        assert_eq!(r.clouds.gemini.keys[0].key, "k1");
    }

    /// baseName port — note the `i+1 < len` guard keeps a
    /// trailing-slash path unchanged.
    #[test]
    fn base_name_paths() {
        assert_eq!(base_name("C:\\work\\repo"), "repo");
        assert_eq!(base_name("/home/user/repo/"), "/home/user/repo/");
        assert_eq!(base_name("plain"), "plain");
    }
}
