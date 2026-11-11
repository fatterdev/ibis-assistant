//! Flat JSON configuration shared by every runtime mode (personal/plugin/server).
//!
//! Mirrors `internal/config : defaults → file → env
//! overrides, plugin isolation, and service-mode path finalization.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_DB_DATA_PATH: &str = "db";
pub const DEFAULT_LOGS_ROOT: &str = "./logs";
pub const DEFAULT_DB_URL: &str = "ws://localhost:8000/rpc";
pub const DEFAULT_LOOPBACK_DB_URL: &str = "ws://127.0.0.1:8000/rpc";
pub const DEFAULT_PLUGIN_DB_URL: &str = "ws://127.0.0.1:18000/rpc";
pub const DEFAULT_PLUGIN_LOG_FILE: &str = "plugin.log";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MemoryConfig {
    #[serde(rename = "projection_enabled")]
    pub projection_enabled: bool,
    #[serde(rename = "git_store")]
    pub git_store: GitStoreConfig,
    /// Per-kind `details` enforcement (C7): `warn` (default) accepts partial
    /// details with a warning, `strict` rejects writes missing the kind's
    /// required keys. Unknown values resolve to warn with a warning.
    #[serde(rename = "schema_enforcement")]
    pub schema_enforcement: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            projection_enabled: false,
            git_store: GitStoreConfig::default(),
            schema_enforcement: "warn".to_string(),
        }
    }
}

/// Governs the git-tree claim store (`.ibis/claims/`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GitStoreConfig {
    /// Personal-claims policy: `db_only` (default on team/server) | `stub`
    /// (default in personal mode) | `in_repo`. Empty resolves by runtime mode.
    #[serde(rename = "personal_claims")]
    pub personal_claims: String,
}

/// Kill-switches for the git-indexed world state. Defaults keep the optional
/// layers enabled — the flags are operator escape hatches, not feature toggles.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorldStateConfig {
    /// Turns off the hot-ref code byte cache (`file_at_commit`).
    #[serde(rename = "code_cache_disabled")]
    pub code_cache_disabled: bool,
}

/// Retrieval knobs (`search.*`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SearchConfig {
    /// Channel fusion for `ask_project`: `additive` (legacy default) |
    /// `rrf` (Reciprocal Rank Fusion, A2). Unknown values resolve to
    /// additive with a warning.
    #[serde(rename = "fusion")]
    pub fusion: String,
    /// Candidate window (WQ-7): per-channel fetch size above the final 15-hit
    /// cut, shared by the temporal rescore and the future A3 reranker.
    #[serde(rename = "candidates")]
    pub candidates: u32,
    /// Query expansion (A4): `off` | `symbol_variant` (default). The variant
    /// embedding fires only when linked symbols resolve (zero cost otherwise).
    /// Unknown values resolve to symbol_variant with a warning.
    #[serde(rename = "query_expansion")]
    pub query_expansion: String,
    /// Post-fusion reranker (A3): `off` (default) | `local` (one local-first
    /// scoring pass over the top candidate-window hits, `bulk_local` hint).
    /// Unknown values resolve to off with a warning.
    #[serde(rename = "rerank")]
    pub rerank: String,
    /// Agentic ReAct loop brakes (E14): `search.agentic.*` — max steps,
    /// token budget, repeated-search cap and the citation completion check.
    /// Raw values are clamped/warned in
    /// `ibis_search::agentic::AgenticSettings`.
    #[serde(rename = "agentic")]
    pub agentic: AgenticConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            fusion: "additive".to_string(),
            candidates: 50,
            query_expansion: "symbol_variant".to_string(),
            rerank: "off".to_string(),
            agentic: AgenticConfig::default(),
        }
    }
}

/// Agentic loop knobs (`search.agentic`, E14). Retrocompatible: a config
/// without the block loads the defaults; unknown keys are ignored. The
/// resolution (clamp + warn) lives in `ibis_search::agentic::AgenticSettings`,
/// so these raw values never fail the boot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgenticConfig {
    /// Hard cap on `generate_content` calls per ask (clamped to [1, 10];
    /// default 5 — the historical `AGENTIC_MAX_STEPS`).
    #[serde(rename = "max_steps")]
    pub max_steps: u32,
    /// Raw-token budget per ask: `0` = brake off; positive values are
    /// clamped to [4000, 500000] (default 120000 — the observed worst case
    /// of 5 steps is ~110–140k cumulative tokens).
    #[serde(rename = "token_budget")]
    pub token_budget: u32,
    /// Maximum consecutive executions of the same normalized query
    /// (clamped to [1, 5]; default 2). The brake is not disableable: the
    /// next repetition gets a nudge, the one after stops the loop.
    #[serde(rename = "max_repeated_searches")]
    pub max_repeated_searches: u32,
    /// Citation-based completion check: `off` | `warn` (default) | `enforce`.
    /// Unknown/empty values resolve to `warn`.
    #[serde(rename = "completion_check")]
    pub completion_check: String,
    /// Coverage threshold (base-1000 rounded) used by `enforce` and logged
    /// by `warn` (clamped to [0.0, 1.0]; default 0.60).
    #[serde(rename = "completion_min_coverage")]
    pub completion_min_coverage: f64,
}

impl Default for AgenticConfig {
    fn default() -> Self {
        AgenticConfig {
            max_steps: 5,
            token_budget: 120_000,
            max_repeated_searches: 2,
            completion_check: "warn".to_string(),
            completion_min_coverage: 0.60,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(rename = "working_repo_path")]
    pub working_repo_path: String,
    pub url: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SMTPConfig {
    pub enabled: bool,
    pub host: String,
    pub port: i64,
    pub user: String,
    pub password: String,
    pub from: String,
    pub to: String,
    /// ssl_tls, starttls, none
    pub encryption: String,
    #[serde(rename = "aggregation_window")]
    pub aggregation_window: String,
    #[serde(rename = "emergency_severity_threshold")]
    pub emergency_severity_threshold: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LogIngestionConfig {
    pub http: HTTPIngestionConfig,
    pub polling: Vec<PollingSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HTTPIngestionConfig {
    pub enabled: bool,
    #[serde(rename = "api_key")]
    pub api_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PollingSource {
    pub name: String,
    pub enabled: bool,
    /// ftp, sftp, smb
    pub protocol: String,
    pub host: String,
    pub port: i64,
    pub user: String,
    pub password: String,
    #[serde(rename = "remote_dir")]
    pub remote_dir: String,
    #[serde(rename = "file_pattern")]
    pub file_pattern: String,
    #[serde(rename = "archive_dir")]
    pub archive_dir: String,
    /// e.g. "15m"
    #[serde(rename = "poll_interval")]
    pub poll_interval: String,
    #[serde(rename = "project_name")]
    pub project_name: String,
    /// Expected base64-encoded public key of the SFTP server. If empty, host
    /// key verification is skipped (InsecureIgnoreHostKey).
    #[serde(rename = "known_host_key")]
    pub known_host_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GeminiKeyConfig {
    pub key: String,
    pub rpm: i64,
    pub tpm: i64,
    pub rpd: i64,
    pub owner: String,
    #[serde(rename = "allow_overage")]
    pub allow_overage: bool,
}

/// Daemon-side consolidation loop knobs (`ai.consolidation`, F4 §B.5):
/// opt-in, off by default — the loop runs only in the daemon
/// (`ibis run` / `ibis serve --team`), never on client surfaces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// Turns the daemon-owned consolidation loop on (CLI `--consolidate`
    /// forces it on regardless of this value).
    pub enabled: bool,
    /// Seconds between consolidation passes (clamped to a 60s minimum).
    #[serde(rename = "interval_secs")]
    pub interval_secs: u64,
    /// Minimum cosine similarity for a consolidation duplicate candidate to
    /// reach the distillation prompt (C9): candidates scoring below it are
    /// dropped before the LLM call. Clamped to [0.0, 1.0] at use.
    pub dup_similarity_min: f64,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        ConsolidationConfig {
            enabled: false,
            interval_secs: 3600,
            // C9: duplicate candidates below this cosine similarity never
            // reach the distillation prompt.
            dup_similarity_min: 0.80,
        }
    }
}

/// E16 — durable task store knobs (`tasks`). The values are kept raw here;
/// `ibis_optimization::tasks::TaskPolicy` applies the documented clamps at
/// use (lease [10, 600]s, heartbeat [1, lease/2]s, recoveries [1, 10],
/// retention [1, 365] days).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TasksConfig {
    /// Lease duration in seconds; a dead owner is reclaimable afterwards.
    pub lease_secs: u64,
    /// Heartbeat cadence in seconds (clamped to at least two beats per lease).
    pub heartbeat_secs: u64,
    /// Reclaims after a crash before the task is declared failed.
    pub max_recoveries: u32,
    /// Retention of terminal tasks, pruned at boot.
    pub retention_days: u64,
}

impl Default for TasksConfig {
    fn default() -> Self {
        TasksConfig {
            lease_secs: 30,
            heartbeat_secs: 10,
            max_recoveries: 3,
            retention_days: 30,
        }
    }
}

/// G20 — per-host harness profiles (`harness`). The default profile is the
/// current behavior; an empty `profiles` list changes nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessConfig {
    pub default: HarnessProfile,
    /// Ordered precedence: the first `match` hit wins.
    pub profiles: Vec<HarnessProfile>,
}

/// One harness profile. `match` is a list of case-insensitive substrings on
/// `clientInfo.name`; `sampling` is `auto` (default) or `never`;
/// `hidden_tool_groups` is a subset of the server's `TOOL_GROUPS`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessProfile {
    #[serde(rename = "match")]
    pub match_names: Vec<String>,
    /// Log label; an empty name falls back to the match string / "default".
    pub name: String,
    pub sampling: String,
    pub hidden_tool_groups: Vec<String>,
}

impl Default for HarnessProfile {
    fn default() -> Self {
        HarnessProfile {
            match_names: Vec::new(),
            name: String::new(),
            sampling: "auto".to_string(),
            hidden_tool_groups: Vec::new(),
        }
    }
}

/// Bounded decisions (`ai.decide`, D11): shadow mode is on by default — the
/// consolidation computes the typed `is_noise` decision next to the
/// generative verdict and logs `decide_shadow`, without changing behavior.
/// Activation (routing the decision through `decide()`) is a later phase
/// driven by the shadow agreement data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DecideConfig {
    pub shadow: bool,
}

impl Default for DecideConfig {
    fn default() -> Self {
        DecideConfig { shadow: true }
    }
}

/// D12 — tiered LLM response cache (`ai.response_cache`). v1 is the
/// exact-match tier only: byte-identical prompt + routing identity +
/// invalidation context. Opt-in, best-effort (a missing DB store or a lookup
/// failure degrades to the live provider), never applied to `ask_project`
/// (call sites opt in by setting `GenerationConfig::response_cache`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ResponseCacheConfig {
    /// Master switch. Off by default, like the B6 embedding disk cache.
    pub enabled: bool,
    /// TTL seconds for the boot-time prune (default 7 days).
    pub ttl_secs: u64,
}

impl Default for ResponseCacheConfig {
    fn default() -> Self {
        ResponseCacheConfig {
            enabled: false,
            ttl_secs: 7 * 24 * 60 * 60,
        }
    }
}

/// Derived-observation engine knobs (`ai.observations`, C8): opt-in, off by
/// default — `enabled = false` turns off both the ingest hook
/// (`InvalidateOnly`) and the consolidation-pass synthesis (`Full`), and
/// existing derived rows stay inert. Bottoms out in
/// `ibis_search::memory::observations::ObservationSettings::from_config`
/// (clamp + family whitelist; never fails the boot).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ObservationsConfig {
    /// Master switch (both the hook and the pass).
    pub enabled: bool,
    /// Minimum component size (singletons never persist).
    #[serde(rename = "min_cluster_size")]
    pub min_cluster_size: u32,
    /// Components larger than this are dropped, never truncated.
    #[serde(rename = "max_members")]
    pub max_members: u32,
    /// Signature degree above which the signature is excluded (hub defense).
    #[serde(rename = "hub_degree_max")]
    pub hub_degree_max: u32,
    /// Per-repo cluster cap per reconcile pass.
    #[serde(rename = "max_clusters_per_repo")]
    pub max_clusters_per_repo: u32,
    /// Eligible memories examined per pass (Q1 bound).
    #[serde(rename = "max_members_examined")]
    pub max_members_examined: u32,
    /// Scope cap per project pass: `team` + up to `max_scopes - 1` owners.
    #[serde(rename = "max_scopes_per_project")]
    pub max_scopes_per_project: u32,
    /// LLM summaries per pass (the token-spending cap).
    #[serde(rename = "max_llm_summaries_per_pass")]
    pub max_llm_summaries_per_pass: u32,
    /// Family whitelist (`fix_root_cause` | `issue` | `symbol` | `file`);
    /// unknown names are dropped with a warning at resolution.
    pub families: Vec<String>,
    /// D11 shadow measurement of the deterministic accept filters (log-only,
    /// default off; rubric `observation_accept.v1`).
    #[serde(rename = "decide_shadow")]
    pub decide_shadow: bool,
}

impl Default for ObservationsConfig {
    fn default() -> Self {
        ObservationsConfig {
            enabled: false,
            min_cluster_size: 2,
            max_members: 12,
            hub_degree_max: 10,
            max_clusters_per_repo: 25,
            max_members_examined: 500,
            max_scopes_per_project: 8,
            max_llm_summaries_per_pass: 10,
            families: vec![
                "fix_root_cause".to_string(),
                "issue".to_string(),
                "symbol".to_string(),
                "file".to_string(),
            ],
            decide_shadow: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AIConfig {
    pub embedding: EmbeddingConfig,
    pub reasoning: ReasoningConfig,
    pub consolidation: ConsolidationConfig,
    pub decide: DecideConfig,
    pub response_cache: ResponseCacheConfig,
    pub observations: ObservationsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// ollama, gemini
    pub provider: String,
    pub model: String,
    pub url: String,
    #[serde(rename = "auto_start")]
    pub auto_start: bool,
    #[serde(rename = "auto_update")]
    pub auto_update: bool,
    /// B6: persistent L2 embedding cache on SurrealDB (opt-in; best-effort —
    /// when off or the DB is absent the provider degrades to the HTTP path).
    #[serde(rename = "disk_cache")]
    pub disk_cache: bool,
    /// B6: TTL seconds for the L2 cache boot-time prune (default 30 days).
    #[serde(rename = "disk_cache_ttl_secs")]
    pub disk_cache_ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReasoningConfig {
    /// hybrid, ollama, gemini, openai_compat, claude, cursor, ide_sampling, none
    /// (`ide_sampling`/`mcp_sampling` = reasoning via MCP
    /// `sampling/createMessage` with the lm-bridge → ollama fallback chain;
    /// the default in plugin runtime mode).
    pub provider: String,
    /// auto or an explicit Ollama/cloud model
    pub model: String,
    /// legacy; migrated into clouds.gemini
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<GeminiKeyConfig>,
    #[serde(skip_serializing_if = "clouds_is_empty")]
    pub clouds: CloudsConfig,
    #[serde(skip_serializing_if = "routing_is_empty")]
    pub routing: RoutingConfig,
    /// S/M/L/XL
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub model_overrides: HashMap<String, String>,
    #[serde(rename = "context_only_fallback")]
    pub context_only_fallback: bool,
    #[serde(rename = "always_smallest_local")]
    pub always_smallest_local: bool,
    /// Enables the Cursor/VS Code LM bridge (provider "cursor") when the host RPC is up.
    #[serde(rename = "use_ide_provided_models")]
    pub use_ide_provided_models: bool,
}

fn clouds_is_empty(c: &CloudsConfig) -> bool {
    c.gemini.keys.is_empty()
        && c.openai_compat.base_url.is_empty()
        && c.openai_compat.api_key.is_empty()
        && c.openai_compat.model.is_empty()
        && c.claude.api_key.is_empty()
        && c.claude.model.is_empty()
}

fn routing_is_empty(r: &RoutingConfig) -> bool {
    r.mode.is_empty()
        && r.local_provider.is_empty()
        && r.cloud_provider.is_empty()
        && r.cloud_fallback_order.is_empty()
        && !r.use_cloud_when_no_gpu
        && r.local_timeout_ms == 0
}

/// Holds multi-cloud credentials (plug-and-play via wizard/GUI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CloudsConfig {
    #[serde(skip_serializing_if = "gemini_keys_is_empty")]
    pub gemini: GeminiCloudConfig,
    #[serde(skip_serializing_if = "openai_compat_is_empty")]
    pub openai_compat: OpenAICompatCloudConfig,
    #[serde(skip_serializing_if = "claude_is_empty")]
    pub claude: ClaudeCloudConfig,
}

fn gemini_keys_is_empty(g: &GeminiCloudConfig) -> bool {
    g.keys.is_empty()
}

fn openai_compat_is_empty(o: &OpenAICompatCloudConfig) -> bool {
    o.base_url.is_empty() && o.api_key.is_empty() && o.model.is_empty()
}

fn claude_is_empty(c: &ClaudeCloudConfig) -> bool {
    c.api_key.is_empty() && c.model.is_empty()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GeminiCloudConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<GeminiKeyConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpenAICompatCloudConfig {
    #[serde(rename = "base_url")]
    pub base_url: String,
    #[serde(rename = "api_key")]
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClaudeCloudConfig {
    #[serde(rename = "api_key")]
    pub api_key: String,
    pub model: String,
}

/// Controls hybrid local/cloud routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutingConfig {
    /// auto
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub local_provider: String,
    /// auto|gemini|openai_compat|claude
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cloud_provider: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cloud_fallback_order: Vec<String>,
    #[serde(rename = "use_cloud_when_no_gpu")]
    pub use_cloud_when_no_gpu: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub local_timeout_ms: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// All application configuration.
///
/// The `Default` impl carries the `Load defaults; serde's container
/// `#[serde(default)]` overlays the file JSON on top of them, mirroring 's
/// `json.Decoder.Decode` into a pre-populated struct.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub port: i64,
    pub bind_address: String,
    /// personal, server, plugin
    #[serde(rename = "runtime_mode")]
    pub runtime_mode: String,
    /// sse, stdio
    pub mode: String,
    #[serde(rename = "db_url")]
    pub db_url: String,
    #[serde(rename = "db_namespace")]
    pub db_namespace: String,
    #[serde(rename = "db_database")]
    pub db_database: String,
    #[serde(rename = "db_user")]
    pub db_user: String,
    #[serde(rename = "db_password")]
    pub db_password: String,
    pub ai: AIConfig,
    /// Legacy fallback
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gemini_keys: Vec<GeminiKeyConfig>,
    /// Legacy fallback
    #[serde(rename = "gemini_rpm", skip_serializing_if = "is_zero")]
    pub gemini_default_rpm: i64,
    #[serde(rename = "redmine_url")]
    pub redmine_url: String,
    #[serde(rename = "redmine_key")]
    pub redmine_key: String,
    #[serde(rename = "redmine_concurrency")]
    pub redmine_concurrency: i64,
    #[serde(rename = "code_concurrency")]
    pub code_concurrency: i64,
    #[serde(rename = "db_timeout")]
    pub db_timeout: i64,
    #[serde(rename = "db_data_path")]
    pub db_data_path: String,
    #[serde(rename = "db_auto_update")]
    pub db_auto_update: bool,
    #[serde(rename = "discovery_root")]
    pub discovery_root: String,
    /// seconds
    #[serde(rename = "poll_interval")]
    pub poll_interval: i64,
    #[serde(rename = "auto_scan")]
    pub auto_scan: bool,
    /// Manual list override (absolute paths; match by folder basename)
    #[serde(rename = "git_repos")]
    pub git_repos: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectConfig>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub log_file: String,
    /// DEBUG, INFO, WARN, ERROR
    #[serde(rename = "log_level")]
    pub log_level: String,
    /// bytes
    #[serde(rename = "max_file_size")]
    pub max_file_size: i64,
    /// bytes/characters for commit diff chunking
    #[serde(rename = "max_delta_size")]
    pub max_delta_size: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_dirs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_extensions: Vec<String>,
    #[serde(rename = "logs_root")]
    pub logs_root: String,
    #[serde(skip_serializing_if = "git_tokens_is_empty")]
    pub git_tokens: HashMap<String, String>,
    pub smtp: SMTPConfig,
    #[serde(rename = "log_ingestion")]
    pub log_ingestion: LogIngestionConfig,
    pub memory: MemoryConfig,
    #[serde(rename = "world_state")]
    pub world_state: WorldStateConfig,
    #[serde(rename = "search")]
    pub search: SearchConfig,
    /// E16 — durable task store knobs (`tasks.*`).
    pub tasks: TasksConfig,
    /// F17 — opt-in OpenTelemetry OTLP span export (`telemetry.*`). The
    /// feature `otel` is required for the exporter to be compiled in.
    pub telemetry: ibis_logger::TelemetryConfig,
    /// G20 — per-host harness profiles (`harness.*`). Default = current
    /// behavior (no hidden groups, sampling `auto`).
    pub harness: HarnessConfig,
    /// True if a config file was successfully loaded (not serialized).
    #[serde(skip)]
    pub config_loaded: bool,
    /// Path to the file that was loaded (not serialized).
    #[serde(skip)]
    pub config_path: String,
    /// Opened folder / IBIS_WORKSPACE / -workspace (not serialized).
    #[serde(skip)]
    pub workspace: String,
}

fn git_tokens_is_empty(m: &HashMap<String, String>) -> bool {
    m.is_empty()
}

impl Default for SMTPConfig {
    fn default() -> Self {
        SMTPConfig {
            enabled: false,
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            from: String::new(),
            to: String::new(),
            encryption: "none".into(),
            aggregation_window: "1h".into(),
            emergency_severity_threshold: 9,
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        EmbeddingConfig {
            provider: "ollama".into(),
            model: "nomic-embed-text".into(),
            url: "http://127.0.0.1:11434".into(),
            auto_start: true,
            auto_update: true,
            // B6: opt-in L2 disk cache — off by default.
            disk_cache: false,
            disk_cache_ttl_secs: 30 * 24 * 60 * 60,
        }
    }
}

impl Default for RoutingConfig {
    fn default() -> Self {
        RoutingConfig {
            mode: "auto".into(),
            local_provider: "ollama".into(),
            cloud_provider: "auto".into(),
            cloud_fallback_order: vec!["gemini", "openai_compat", "claude"]
                .into_iter()
                .map(String::from)
                .collect(),
            use_cloud_when_no_gpu: true,
            local_timeout_ms: 120_000,
        }
    }
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        ReasoningConfig {
            provider: "hybrid".into(),
            model: "auto".into(),
            keys: Vec::new(),
            context_only_fallback: true,
            always_smallest_local: false,
            use_ide_provided_models: false,
            routing: RoutingConfig::default(),
            model_overrides: HashMap::from([
                ("S".into(), "granite4.1:3b".into()),
                ("M".into(), "qwen2.5-coder:7b".into()),
                ("L".into(), "gemma4:12b".into()),
                ("XL".into(), "muse-glimmer".into()),
            ]),
            clouds: CloudsConfig::default(),
        }
    }
}

impl Default for CloudsConfig {
    fn default() -> Self {
        CloudsConfig {
            gemini: GeminiCloudConfig::default(),
            openai_compat: OpenAICompatCloudConfig {
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4.1".into(),
                ..Default::default()
            },
            claude: ClaudeCloudConfig {
                model: "claude-sonnet-4".into(),
                ..Default::default()
            },
        }
    }
}

fn default_ignored_dirs() -> Vec<String> {
    vec![
        ".git",
        "node_modules",
        "bin",
        "obj",
        "vendor",
        ".idea",
        ".vscode",
        "dist",
        "build",
        "coverage",
        "target",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_ignored_files() -> Vec<String> {
    vec![
        "package-lock.json",
        "yarn.lock",
        "go.sum",
        "go.mod",
        "Gemfile.lock",
        "pnpm-lock.yaml",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_supported_extensions() -> Vec<String> {
    vec![
        ".go", ".py", ".js", ".ts", ".tsx", ".jsx", ".md", ".cs", ".java", ".cpp", ".h", ".c",
        ".html", ".htm", ".css", ".scss", ".sql", ".vue", ".jsp", ".erb", ".ejs", ".svelte",
        ".astro", ".php", ".rb",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Shipped defaults ( Load).
pub fn default_config() -> Config {
    Config::default()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 3030,
            mode: "sse".into(),
            db_url: DEFAULT_DB_URL.into(),
            db_namespace: "ibis".into(),
            db_database: "analysis".into(),
            db_user: "root".into(),
            db_password: "root".into(),
            ai: AIConfig {
                embedding: EmbeddingConfig {
                    provider: "ollama".into(),
                    model: "nomic-embed-text".into(),
                    url: "http://127.0.0.1:11434".into(),
                    auto_start: true,
                    auto_update: true,
                    // B6: opt-in L2 disk cache — off by default.
                    disk_cache: false,
                    disk_cache_ttl_secs: 30 * 24 * 60 * 60,
                },
                reasoning: ReasoningConfig {
                    provider: "hybrid".into(),
                    model: "auto".into(),
                    context_only_fallback: true,
                    routing: RoutingConfig {
                        mode: "auto".into(),
                        local_provider: "ollama".into(),
                        cloud_provider: "auto".into(),
                        cloud_fallback_order: vec!["gemini", "openai_compat", "claude"]
                            .into_iter()
                            .map(String::from)
                            .collect(),
                        use_cloud_when_no_gpu: true,
                        local_timeout_ms: 120_000,
                    },
                    model_overrides: HashMap::from([
                        ("S".into(), "granite4.1:3b".into()),
                        ("M".into(), "qwen2.5-coder:7b".into()),
                        ("L".into(), "gemma4:12b".into()),
                        ("XL".into(), "muse-glimmer".into()),
                    ]),
                    clouds: CloudsConfig {
                        openai_compat: OpenAICompatCloudConfig {
                            base_url: "https://api.openai.com/v1".into(),
                            model: "gpt-4.1".into(),
                            ..Default::default()
                        },
                        claude: ClaudeCloudConfig {
                            model: "claude-sonnet-4".into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                consolidation: ConsolidationConfig::default(),
                decide: DecideConfig::default(),
                response_cache: ResponseCacheConfig::default(),
                observations: ObservationsConfig::default(),
            },
            gemini_default_rpm: 100,
            redmine_concurrency: 10,
            code_concurrency: 5,
            db_timeout: 300,
            db_data_path: "db".into(),
            discovery_root: ".".into(),
            poll_interval: 300,
            logs_root: "./logs".into(),
            auto_scan: true,
            log_level: "INFO".into(),
            db_auto_update: true,
            max_file_size: 10 * 1024 * 1024,
            max_delta_size: 8000,
            ignored_dirs: default_ignored_dirs(),
            ignored_files: default_ignored_files(),
            supported_extensions: default_supported_extensions(),
            smtp: SMTPConfig {
                enabled: false,
                encryption: "none".into(),
                aggregation_window: "1h".into(),
                emergency_severity_threshold: 9,
                ..Default::default()
            },
            log_ingestion: LogIngestionConfig::default(),
            memory: MemoryConfig::default(),
            world_state: WorldStateConfig::default(),
            search: SearchConfig::default(),
            tasks: TasksConfig::default(),
            telemetry: ibis_logger::TelemetryConfig::default(),
            harness: HarnessConfig::default(),
            bind_address: String::new(),
            runtime_mode: String::new(),
            gemini_keys: Vec::new(),
            redmine_url: String::new(),
            redmine_key: String::new(),
            git_repos: Vec::new(),
            projects: Vec::new(),
            log_file: String::new(),
            git_tokens: HashMap::new(),
            config_loaded: false,
            config_path: String::new(),
            workspace: String::new(),
        }
    }
}

/// host:port for the HTTP listener. Empty bind_address keeps `:port`
/// (all interfaces) unless runtime_mode is set:
/// personal/plugin → 127.0.0.1, server → 0.0.0.0.
impl Config {
    pub fn listen_addr(&self) -> String {
        let mut host = self.bind_address.trim().to_string();
        if host.is_empty() {
            host = match self.runtime_mode.trim().to_lowercase().as_str() {
                "personal" | "plugin" => "127.0.0.1".to_string(),
                "server" => "0.0.0.0".to_string(),
                _ => String::new(),
            };
        }
        join_host_port(&host, self.port)
    }

    /// URL advertised in discovery (loopback for wildcard binds).
    pub fn public_base_url(&self) -> String {
        let mut host = split_host_port(&self.listen_addr()).unwrap_or_default();
        if host.is_empty() || host == "0.0.0.0" || host == "::" {
            host = "127.0.0.1".to_string();
        }
        format!("http://{}", join_host_port(&host, self.port))
    }

    pub fn get_git_token(&self, origin_url: &str) -> String {
        for (prefix, token) in self.git_tokens.iter() {
            if prefix != "*" && prefix != "default" && origin_url.contains(prefix.as_str()) {
                return token.clone();
            }
        }
        if let Some(token) = self.git_tokens.get("*") {
            return token.clone();
        }
        if let Some(token) = self.git_tokens.get("default") {
            return token.clone();
        }
        String::new()
    }

    pub fn is_plugin(&self) -> bool {
        is_plugin_runtime(&self.runtime_mode)
    }

    pub fn live_workspace(&self) -> bool {
        is_live_runtime(&self.runtime_mode)
    }
}

/// net.JoinHostPort equivalent: brackets IPv6 literals.
pub fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// net.SplitHostPort equivalent for the shapes listen_addr produces.
pub fn split_host_port(addr: &str) -> Option<String> {
    let trimmed = addr.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        return Some(host.to_string());
    }
    let idx = trimmed.rfind(':')?;
    if idx == 0 {
        return Some(String::new());
    }
    Some(trimmed[..idx].to_string())
}

/// Lowercased runtime_mode (personal/server/plugin) or empty.
pub fn normalize_runtime_mode(mode: &str) -> String {
    mode.trim().to_lowercase()
}

/// True for personal and plugin (working tree in place, no destructive git).
pub fn is_live_runtime(mode: &str) -> bool {
    matches!(normalize_runtime_mode(mode).as_str(), "personal" | "plugin")
}

/// True when runtime_mode is plugin.
pub fn is_plugin_runtime(mode: &str) -> bool {
    normalize_runtime_mode(mode) == "plugin"
}

/// Isolated data directory for a runtime. IBIS_DATA_DIR overrides.
/// Plugin default: %LOCALAPPDATA%\ibis\plugin (or the user config
/// dir on non-Windows).
pub fn data_dir(runtime: &str) -> String {
    if let Ok(v) = std::env::var("IBIS_DATA_DIR") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    let mut base = std::env::var("LOCALAPPDATA")
        .unwrap_or_default()
        .trim()
        .to_string();
    if base.is_empty() {
        base = user_config_dir().unwrap_or_else(|| std::env::temp_dir().display().to_string());
    }
    let mut name = normalize_runtime_mode(runtime);
    if name.is_empty() {
        name = "plugin".into();
    }
    Path::new(&base)
        .join("ibis")
        .join(name)
        .display()
        .to_string()
}

fn user_config_dir() -> Option<String> {
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return Some(xdg);
            }
        }
        std::env::var("HOME")
            .ok()
            .map(|h| Path::new(&h).join(".config").display().to_string())
    }
}

fn env_looks_like_plugin() -> bool {
    is_plugin_runtime(&std::env::var("RUNTIME_MODE").unwrap_or_default())
}

fn plugin_config_search_paths() -> Vec<String> {
    vec![Path::new(&data_dir("plugin"))
        .join("config.json")
        .display()
        .to_string()]
}

fn to_slash(p: &str) -> String {
    p.replace('\\', "/")
}

fn is_default_relative(path: &str, def: &str) -> bool {
    let p = to_slash(path.trim());
    let d = to_slash(def.trim());
    if p.is_empty() {
        return true;
    }
    p.eq_ignore_ascii_case(&d) || p.eq_ignore_ascii_case(d.strip_prefix("./").unwrap_or(&d))
}

fn is_default_db_url(url: &str) -> bool {
    let u = url.trim();
    u.is_empty()
        || u.eq_ignore_ascii_case(DEFAULT_DB_URL)
        || u.eq_ignore_ascii_case(DEFAULT_LOOPBACK_DB_URL)
}

fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
}

fn is_plugin_default_under_exe(path: &str, leaf: &str) -> bool {
    let path = path.trim();
    if path.is_empty() {
        return false;
    }
    let Some(exe_dir) = current_exe_dir() else {
        return false;
    };
    let want = clean_path(&exe_dir.join(leaf));
    clean_path(Path::new(path)).eq_ignore_ascii_case(&want)
}

/// Remaps default relative data paths into the plugin data dir so a Cursor
/// child does not share SurrealDB or logs with a personal install.
pub fn apply_plugin_isolation(cfg: &mut Config) {
    if !cfg.is_plugin() {
        return;
    }
    let dir = data_dir("plugin");
    let _ = std::fs::create_dir_all(&dir);

    if is_default_relative(&cfg.db_data_path, DEFAULT_DB_DATA_PATH)
        || is_plugin_default_under_exe(&cfg.db_data_path, DEFAULT_DB_DATA_PATH)
    {
        cfg.db_data_path = Path::new(&dir).join("db").display().to_string();
    }
    if is_default_relative(&cfg.logs_root, DEFAULT_LOGS_ROOT)
        || is_plugin_default_under_exe(&cfg.logs_root, "logs")
    {
        cfg.logs_root = Path::new(&dir).join("logs").display().to_string();
    }
    if cfg.log_file.is_empty()
        || is_default_relative(&cfg.log_file, "./server.log")
        || is_default_relative(&cfg.log_file, "server.log")
        || is_plugin_default_under_exe(&cfg.log_file, "server.log")
    {
        cfg.log_file = Path::new(&dir)
            .join(DEFAULT_PLUGIN_LOG_FILE)
            .display()
            .to_string();
    }
    if is_default_db_url(&cfg.db_url) {
        cfg.db_url = DEFAULT_PLUGIN_DB_URL.into();
    }
}

/// Scenario: filepath.Join + filepath.Clean equivalent for resolve_path results:
/// drops "." components and redundant separators, keeps base on ".".
pub fn clean_path(p: &Path) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let mut s = String::new();
    for comp in p.components() {
        match comp {
            std::path::Component::Prefix(pre) => {
                s.push_str(&pre.as_os_str().to_string_lossy());
            }
            std::path::Component::RootDir => {
                if !s.ends_with(sep) {
                    s.push(sep);
                }
            }
            std::path::Component::ParentDir => {
                if !s.is_empty() && !s.ends_with(sep) {
                    s.push(sep);
                }
                s.push_str("..");
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => {
                if !s.is_empty() && !s.ends_with(sep) {
                    s.push(sep);
                }
                s.push_str(&part.to_string_lossy());
            }
        }
    }
    if s.is_empty() {
        ".".to_string()
    } else {
        s
    }
}

/// Custom absolute path check for cross-platform robustness (e.g. D:\path on non-Windows).
fn is_windows_drive_path_str(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn resolve_path(base_dir: &str, path: &str) -> String {
    let is_unix_abs = path.starts_with('/');
    if path.is_empty()
        || is_windows_drive_path_str(path)
        || is_unix_abs
        || Path::new(path).is_absolute()
    {
        return path.to_string();
    }
    let joined = Path::new(base_dir).join(path);
    clean_path(&joined)
}

/// Finalizes paths for service mode: resolve relative paths against the
/// executable directory instead of CWD. Plugin mode uses the isolated
/// user-data directory instead.
fn finalize_config_paths(cfg: &mut Config) {
    let plugin = cfg.is_plugin() || env_looks_like_plugin();
    let base: Option<String> = if plugin {
        let dir = data_dir("plugin");
        let _ = std::fs::create_dir_all(&dir);
        Some(dir)
    } else {
        current_exe_dir().map(|d| d.display().to_string())
    };
    let Some(base) = base else { return };
    cfg.db_data_path = resolve_path(&base, &cfg.db_data_path);
    cfg.discovery_root = resolve_path(&base, &cfg.discovery_root);
    cfg.logs_root = resolve_path(&base, &cfg.logs_root);
    if !cfg.log_file.is_empty() {
        cfg.log_file = resolve_path(&base, &cfg.log_file);
    }
    for p in cfg.projects.iter_mut() {
        if !p.working_repo_path.is_empty() {
            p.working_repo_path = resolve_path(&base, &p.working_repo_path);
        }
    }
}

impl Config {
    /// Configuration loaded from Defaults + File + Env, searching the optional
    /// config paths (empty = default search).
    pub fn load() -> Config {
        Self::load_from(&[])
    }

    /// `Load(paths...)` — search exactly these paths when non-empty.
    pub fn load_from(paths: &[String]) -> Config {
        let mut cfg = default_config();

        let mut search_paths: Vec<String> = paths.to_vec();
        if search_paths.is_empty() {
            if env_looks_like_plugin() {
                // Do not load a personal install's config.json from CWD or next to the exe.
                search_paths = plugin_config_search_paths();
            } else {
                search_paths.push("config.json".into());
                // Running as a service: the CWD might be wrong; try next to the executable.
                if let Some(exe_dir) = current_exe_dir() {
                    search_paths.push(exe_dir.join("config.json").display().to_string());
                }
            }
        }

        for p in &search_paths {
            if let Ok(contents) = std::fs::read_to_string(p) {
                match serde_json::from_str::<Config>(&contents) {
                    Ok(parsed) => {
                        cfg = parsed;
                        cfg.config_loaded = true;
                        cfg.config_path = std::fs::canonicalize(p)
                            .map(|c| c.display().to_string())
                            .unwrap_or_else(|_| p.clone());
                        break;
                    }
                    Err(err) => {
                        // Found a file but it's invalid. Print to stderr because
                        // the logger isn't initialized yet.
                        let mut stderr = std::io::stderr();
                        let _ = writeln!(stderr, "Error parsing config file {p}: {err}");
                    }
                }
            }
        }

        // Env overrides (highest priority before CLI flags).
        if let Some(v) = env_parse::<i64>("PORT") {
            cfg.port = v;
        }
        if let Some(v) = env_str("KNOWLEDGE_MODE") {
            cfg.mode = v;
        }
        if let Some(v) = env_str("MODE") {
            cfg.mode = v;
        }
        if let Some(v) = env_str("RUNTIME_MODE") {
            cfg.runtime_mode = v;
        }
        if let Some(v) = env_str("BIND_ADDRESS") {
            cfg.bind_address = v;
        }
        if let Some(v) = env_str("IBIS_WORKSPACE") {
            cfg.workspace = v;
        }

        // DB
        if let Some(v) = env_str("SURREAL_URL") {
            cfg.db_url = v;
        }
        if let Some(v) = env_str("SURREAL_NS") {
            cfg.db_namespace = v;
        }
        if let Some(v) = env_str("SURREAL_DB") {
            cfg.db_database = v;
        }
        if let Some(v) = env_str("SURREAL_USER") {
            cfg.db_user = v;
        }
        if let Some(v) = env_str("SURREAL_PASS") {
            cfg.db_password = v;
        }

        // AI
        if let Some(rpm) = env_parse::<i64>("KNOWLEDGE_RPM") {
            cfg.gemini_default_rpm = rpm;
            for k in cfg.ai.reasoning.keys.iter_mut() {
                k.rpm = rpm;
            }
        }

        if let Some(v) = env_str("EMBEDDING_PROVIDER") {
            cfg.ai.embedding.provider = v;
        }
        if let Some(v) = env_str("EMBEDDING_MODEL") {
            cfg.ai.embedding.model = v;
        }
        if let Some(v) = env_str("EMBEDDING_URL") {
            cfg.ai.embedding.url = v;
        }
        if let Some(v) = env_str("REASONING_PROVIDER") {
            cfg.ai.reasoning.provider = v;
        }
        if let Some(v) = env_str("REASONING_MODEL") {
            cfg.ai.reasoning.model = v;
        }

        // Legacy fallback migration
        if !cfg.gemini_keys.is_empty() && cfg.ai.reasoning.keys.is_empty() {
            cfg.ai.reasoning.keys = cfg.gemini_keys.clone();
        } else if !cfg.ai.reasoning.keys.is_empty() && cfg.gemini_keys.is_empty() {
            cfg.gemini_keys = cfg.ai.reasoning.keys.clone();
        }

        crate::reasoning::normalize_reasoning_config(&mut cfg.ai.reasoning, cfg.gemini_default_rpm);

        // Optional ENV key overrides only when file/clouds still empty
        // (plug-and-play prefers wizard/GUI).
        if let Some(v) = env_str("GEMINI_API_KEY") {
            if !cfg.ai.reasoning.cloud_has_credentials("gemini") {
                let keys: Vec<GeminiKeyConfig> = v
                    .split(',')
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(|p| GeminiKeyConfig {
                        key: p.to_string(),
                        rpm: cfg.gemini_default_rpm,
                        ..Default::default()
                    })
                    .collect();
                cfg.ai.reasoning.keys = keys.clone();
                cfg.gemini_keys = keys.clone();
                cfg.ai.reasoning.clouds.gemini.keys = keys;
            }
        }
        if let Some(v) = env_str("OPENAI_API_KEY") {
            if cfg
                .ai
                .reasoning
                .clouds
                .openai_compat
                .api_key
                .trim()
                .is_empty()
            {
                cfg.ai.reasoning.clouds.openai_compat.api_key = v;
            }
        }
        if let Some(v) = env_str("OPENAI_BASE_URL") {
            cfg.ai.reasoning.clouds.openai_compat.base_url = v;
        }
        if let Some(v) = env_str("ANTHROPIC_API_KEY") {
            if cfg.ai.reasoning.clouds.claude.api_key.trim().is_empty() {
                cfg.ai.reasoning.clouds.claude.api_key = v;
            }
        }

        // Legacy REDMINE_URL/REDMINE_API_KEY environment fallbacks intentionally
        // removed. Ticketing providers are configured via config/ticketing_config.json.
        if let Some(v) = env_parse::<i64>("REDMINE_CONCURRENCY") {
            cfg.redmine_concurrency = v;
        }
        if let Some(v) = env_parse::<i64>("CODE_CONCURRENCY") {
            cfg.code_concurrency = v;
        }
        if let Some(v) = env_parse::<i64>("DB_TIMEOUT") {
            cfg.db_timeout = v;
        }

        // Discovery
        if let Some(v) = env_str("DISCOVERY_ROOT") {
            cfg.discovery_root = v;
        }
        if let Some(v) = env_str("LOGS_ROOT") {
            cfg.logs_root = v;
        }
        if std::env::var("AUTO_SCAN").unwrap_or_default() == "true" {
            cfg.auto_scan = true;
        }
        if let Some(v) = env_str("GIT_TOKEN") {
            cfg.git_tokens.insert("*".into(), v);
        }
        if let Some(v) = env_str("LOG_FILE") {
            cfg.log_file = v;
        }
        if let Some(v) = env_str("LOG_LEVEL") {
            cfg.log_level = v.to_uppercase();
        }
        if let Ok(v) = std::env::var("DB_AUTO_UPDATE") {
            cfg.db_auto_update = v == "true";
        }

        // Memory claims (F4 §B.7): the VSIX knob `ibis.claims.personalMode`
        // reaches spawned children and `ibis start` daemons as this override.
        if let Some(v) = env_str("IBIS_CLAIMS_PERSONAL_MODE") {
            match normalize_personal_claims_level(&v) {
                Some(mode) => cfg.memory.git_store.personal_claims = mode,
                None => {
                    let mut stderr = std::io::stderr();
                    let _ = writeln!(
                        stderr,
                        "Ignoring IBIS_CLAIMS_PERSONAL_MODE={v}: expected db_only, stub, or in_repo"
                    );
                }
            }
        }

        if cfg.is_plugin() || env_looks_like_plugin() {
            cfg.runtime_mode = "plugin".into();
            apply_plugin_isolation(&mut cfg);
        }

        finalize_config_paths(&mut cfg);

        cfg
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Normalizes an `IBIS_CLAIMS_PERSONAL_MODE` value to a personal-claims
/// policy level (`db_only` | `stub` | `in_repo`, case-insensitive). Anything
/// else is rejected so a typo never silently flips the policy.
fn normalize_personal_claims_level(value: &str) -> Option<String> {
    let v = value.trim().to_lowercase();
    matches!(v.as_str(), "db_only" | "stub" | "in_repo").then_some(v)
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_str(key).and_then(|v| v.parse().ok())
}

/// Returns a Config populated with all defaults, without reading any file.
pub fn new_default_config() -> Config {
    Config::load()
}

/// Explicit `memory.projection_enabled` choice in the loaded config file, if
/// any. Profile defaults (the `--team` serve profile) must not override an
/// operator's explicit `false`; `None` means the key is absent (or the file
/// is unreadable/unparseable), so the profile default applies.
pub fn projection_explicitly_set(config_path: &str) -> Option<bool> {
    let contents = std::fs::read_to_string(config_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value.get("memory")?.get("projection_enabled")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addr_and_public_url() {
        let personal = Config {
            port: 3030,
            runtime_mode: "personal".into(),
            ..Default::default()
        };
        assert_eq!(personal.listen_addr(), "127.0.0.1:3030");
        let server = Config {
            port: 3030,
            runtime_mode: "server".into(),
            ..Default::default()
        };
        assert_eq!(server.listen_addr(), "0.0.0.0:3030");
        let custom = Config {
            port: 9,
            runtime_mode: "personal".into(),
            bind_address: "10.0.0.5".into(),
            ..Default::default()
        };
        assert_eq!(custom.listen_addr(), "10.0.0.5:9");
        assert_eq!(personal.public_base_url(), "http://127.0.0.1:3030");
    }

    #[test]
    fn default_listen_addr_is_bare_port() {
        let cfg = Config {
            port: 3030,
            ..Default::default()
        };
        assert_eq!(cfg.listen_addr(), ":3030");
    }

    #[test]
    fn memory_git_store_personal_claims_json() {
        let cfg: Config =
            serde_json::from_str(r#"{"memory": {"projection_enabled": true, "git_store": {"personal_claims": "in_repo"}}}"#)
                .unwrap();
        assert!(cfg.memory.projection_enabled);
        assert_eq!(cfg.memory.git_store.personal_claims, "in_repo");
    }

    /// C7: `memory.schema_enforcement` defaults to `warn` and accepts the
    /// `strict` opt-in.
    #[test]
    fn memory_schema_enforcement_config() {
        let cfg: Config =
            serde_json::from_str(r#"{"memory": {"schema_enforcement": "strict"}}"#).unwrap();
        assert_eq!(cfg.memory.schema_enforcement, "strict");
        assert_eq!(default_config().memory.schema_enforcement, "warn");
        let empty: Config = serde_json::from_str(r#"{"memory": {}}"#).unwrap();
        assert_eq!(empty.memory.schema_enforcement, "warn");
    }

    #[test]
    fn personal_claims_env_level_normalization() {
        assert_eq!(
            normalize_personal_claims_level("DB_ONLY").as_deref(),
            Some("db_only")
        );
        assert_eq!(
            normalize_personal_claims_level(" stub ").as_deref(),
            Some("stub")
        );
        assert_eq!(
            normalize_personal_claims_level("in_repo").as_deref(),
            Some("in_repo")
        );
        assert_eq!(normalize_personal_claims_level("bogus"), None);
        assert_eq!(normalize_personal_claims_level(""), None);
    }

    #[test]
    fn projection_explicitly_set_reads_only_the_file_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"port": 4040}"#).unwrap();
        assert_eq!(projection_explicitly_set(path.to_str().unwrap()), None);
        std::fs::write(&path, r#"{"memory": {"projection_enabled": false}}"#).unwrap();
        assert_eq!(
            projection_explicitly_set(path.to_str().unwrap()),
            Some(false)
        );
        assert_eq!(projection_explicitly_set(""), None);
    }

    #[test]
    fn world_state_code_cache_disabled() {
        let cfg: Config =
            serde_json::from_str(r#"{"world_state": {"code_cache_disabled": true}}"#).unwrap();
        assert!(cfg.world_state.code_cache_disabled);
        assert!(!default_config().world_state.code_cache_disabled);
    }

    #[test]
    fn search_fusion_config() {
        let cfg: Config = serde_json::from_str(r#"{"search": {"fusion": "rrf"}}"#).unwrap();
        assert_eq!(cfg.search.fusion, "rrf");
        assert_eq!(default_config().search.fusion, "additive");
        let empty: Config = serde_json::from_str(r#"{"search": {}}"#).unwrap();
        assert_eq!(empty.search.fusion, "additive");
    }

    /// A4: `search.query_expansion` defaults to `symbol_variant` and accepts
    /// the `off` escape hatch.
    #[test]
    fn search_query_expansion_config() {
        let cfg: Config =
            serde_json::from_str(r#"{"search": {"query_expansion": "off"}}"#).unwrap();
        assert_eq!(cfg.search.query_expansion, "off");
        assert_eq!(default_config().search.query_expansion, "symbol_variant");
        let empty: Config = serde_json::from_str(r#"{"search": {}}"#).unwrap();
        assert_eq!(empty.search.query_expansion, "symbol_variant");
    }

    /// A3: `search.rerank` defaults to `off` (activation guided by the A1
    /// eval numbers) and accepts `local`.
    #[test]
    fn search_rerank_config() {
        let cfg: Config = serde_json::from_str(r#"{"search": {"rerank": "local"}}"#).unwrap();
        assert_eq!(cfg.search.rerank, "local");
        assert_eq!(default_config().search.rerank, "off");
        let empty: Config = serde_json::from_str(r#"{"search": {}}"#).unwrap();
        assert_eq!(empty.search.rerank, "off");
    }

    /// E14: `search.agentic.*` defaults (max_steps 5, token_budget 120000,
    /// max_repeated_searches 2, completion_check warn, min coverage 0.60)
    /// and the retrocompatible missing-block path.
    #[test]
    fn agentic_config_defaults() {
        let cfg = default_config();
        assert_eq!(cfg.search.agentic.max_steps, 5);
        assert_eq!(cfg.search.agentic.token_budget, 120_000);
        assert_eq!(cfg.search.agentic.max_repeated_searches, 2);
        assert_eq!(cfg.search.agentic.completion_check, "warn");
        assert!((cfg.search.agentic.completion_min_coverage - 0.60).abs() < f64::EPSILON);

        let empty: Config = serde_json::from_str(r#"{"search": {}}"#).unwrap();
        assert_eq!(empty.search.agentic, AgenticConfig::default());
    }

    /// E14: full JSON override of every `search.agentic` key.
    #[test]
    fn agentic_config_overrides() {
        let cfg: Config = serde_json::from_str(
            r#"{"search": {"agentic": {
                "max_steps": 7,
                "token_budget": 40000,
                "max_repeated_searches": 4,
                "completion_check": "enforce",
                "completion_min_coverage": 0.75
            }}}"#,
        )
        .unwrap();
        assert_eq!(cfg.search.agentic.max_steps, 7);
        assert_eq!(cfg.search.agentic.token_budget, 40_000);
        assert_eq!(cfg.search.agentic.max_repeated_searches, 4);
        assert_eq!(cfg.search.agentic.completion_check, "enforce");
        assert!((cfg.search.agentic.completion_min_coverage - 0.75).abs() < f64::EPSILON);
    }

    /// E14: a partial block keeps the missing keys at their defaults; an
    /// unknown `completion_check` value parses as-is (the resolution and the
    /// warn are `AgenticSettings::from_config`'s job, not the parse).
    #[test]
    fn agentic_config_missing_block_uses_defaults() {
        let cfg: Config =
            serde_json::from_str(r#"{"search": {"agentic": {"max_steps": 3}}}"#).unwrap();
        assert_eq!(cfg.search.agentic.max_steps, 3);
        assert_eq!(cfg.search.agentic.token_budget, 120_000);
        assert_eq!(cfg.search.agentic.completion_check, "warn");

        let cfg: Config =
            serde_json::from_str(r#"{"search": {"agentic": {"completion_check": "bogus"}}}"#)
                .unwrap();
        assert_eq!(cfg.search.agentic.completion_check, "bogus");
    }

    /// `tasks.*` (E16): durable task store defaults and file overlay.
    #[test]
    fn tasks_config_defaults() {
        let dflt = default_config();
        assert_eq!(dflt.tasks.lease_secs, 30);
        assert_eq!(dflt.tasks.heartbeat_secs, 10);
        assert_eq!(dflt.tasks.max_recoveries, 3);
        assert_eq!(dflt.tasks.retention_days, 30);

        let cfg: Config =
            serde_json::from_str(r#"{"tasks": {"lease_secs": 45, "max_recoveries": 5}}"#).unwrap();
        assert_eq!(cfg.tasks.lease_secs, 45);
        assert_eq!(cfg.tasks.heartbeat_secs, 10);
        assert_eq!(cfg.tasks.max_recoveries, 5);
        assert_eq!(cfg.tasks.retention_days, 30);
    }

    /// `telemetry.*` (F17): OTLP export is off by default; the file overlays
    /// individual keys and keeps the remaining defaults.
    #[test]
    fn telemetry_config_defaults() {
        let dflt = default_config();
        assert!(!dflt.telemetry.otlp.enabled);
        assert_eq!(dflt.telemetry.otlp.endpoint, "http://127.0.0.1:4318");
        assert_eq!(dflt.telemetry.otlp.protocol, "http/protobuf");
        assert_eq!(dflt.telemetry.otlp.service_name, "ibis");
        assert!((dflt.telemetry.otlp.sample_ratio - 0.1).abs() < f64::EPSILON);
        assert!(dflt.telemetry.otlp.headers.is_empty());

        let cfg: Config = serde_json::from_str(
            r#"{"telemetry":{"otlp":{"enabled":true,"sample_ratio":1.0,"headers":{"x-api-key":"secret"}}}}"#,
        )
        .unwrap();
        assert!(cfg.telemetry.otlp.enabled);
        assert_eq!(cfg.telemetry.otlp.sample_ratio, 1.0);
        assert_eq!(cfg.telemetry.otlp.protocol, "http/protobuf");
        assert_eq!(
            cfg.telemetry
                .otlp
                .headers
                .get("x-api-key")
                .map(String::as_str),
            Some("secret")
        );
    }

    /// `harness.*` (G20): default = current behavior; profiles overlay with
    /// `match`, sampling and hidden groups.
    #[test]
    fn harness_config_defaults() {
        let dflt = default_config();
        assert!(dflt.harness.profiles.is_empty());
        assert_eq!(dflt.harness.default.sampling, "auto");
        assert!(dflt.harness.default.hidden_tool_groups.is_empty());

        let cfg: Config = serde_json::from_str(
            r#"{"harness":{"default":{"sampling":"never"},"profiles":[
                {"match":["cursor"],"name":"cursor","sampling":"never","hidden_tool_groups":["settings"]}
            ]}}"#,
        )
        .unwrap();
        assert_eq!(cfg.harness.default.sampling, "never");
        assert_eq!(cfg.harness.profiles.len(), 1);
        assert_eq!(cfg.harness.profiles[0].match_names, vec!["cursor"]);
        assert_eq!(cfg.harness.profiles[0].hidden_tool_groups, vec!["settings"]);

        // `settings_apply` serializes the whole Config: the `match` rename and
        // the profiles must survive a round-trip.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"match\""), "match rename lost: {json}");
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.harness, cfg.harness);
    }

    /// `ai.consolidation` (F4 §B.5): off by default, the file overlays the
    /// knob, and a partial `ai` object keeps the default interval.
    #[test]
    fn ai_consolidation_config() {
        let dflt = default_config();
        assert!(!dflt.ai.consolidation.enabled);
        assert_eq!(dflt.ai.consolidation.interval_secs, 3600);
        // C9: the duplicate-lookup similarity floor ships at 0.80.
        assert!((dflt.ai.consolidation.dup_similarity_min - 0.80).abs() < f64::EPSILON);

        let cfg: Config = serde_json::from_str(
            r#"{"ai": {"consolidation": {"enabled": true, "interval_secs": 600}}}"#,
        )
        .unwrap();
        assert!(cfg.ai.consolidation.enabled);
        assert_eq!(cfg.ai.consolidation.interval_secs, 600);
        assert!((cfg.ai.consolidation.dup_similarity_min - 0.80).abs() < f64::EPSILON);

        // The C9 knob overlays like the rest of the consolidation object.
        let cfg: Config =
            serde_json::from_str(r#"{"ai": {"consolidation": {"dup_similarity_min": 0.9}}}"#)
                .unwrap();
        assert!((cfg.ai.consolidation.dup_similarity_min - 0.9).abs() < f64::EPSILON);

        // Missing consolidation key under a present `ai` → defaults apply.
        let cfg: Config =
            serde_json::from_str(r#"{"ai": {"reasoning": {"provider": "ollama"}}}"#).unwrap();
        assert!(!cfg.ai.consolidation.enabled);
        assert_eq!(cfg.ai.consolidation.interval_secs, 3600);
        assert!((cfg.ai.consolidation.dup_similarity_min - 0.80).abs() < f64::EPSILON);
    }

    /// `ai.decide.shadow` (D11): shadow mode ships on and overlays like the
    /// rest of the `ai` object.
    #[test]
    fn ai_decide_shadow_config() {
        let dflt = default_config();
        assert!(dflt.ai.decide.shadow);

        let cfg: Config = serde_json::from_str(r#"{"ai": {"decide": {"shadow": false}}}"#).unwrap();
        assert!(!cfg.ai.decide.shadow);

        // Missing decide key under a present `ai` → shadow stays on.
        let cfg: Config =
            serde_json::from_str(r#"{"ai": {"consolidation": {"enabled": true}}}"#).unwrap();
        assert!(cfg.ai.decide.shadow);
    }

    /// `ai.response_cache` (D12): opt-in, 7-day TTL, overlays like the rest
    /// of the `ai` object.
    #[test]
    fn ai_response_cache_config() {
        let dflt = default_config();
        assert!(!dflt.ai.response_cache.enabled);
        assert_eq!(dflt.ai.response_cache.ttl_secs, 7 * 24 * 60 * 60);

        let cfg: Config = serde_json::from_str(
            r#"{"ai": {"response_cache": {"enabled": true, "ttl_secs": 3600}}}"#,
        )
        .unwrap();
        assert!(cfg.ai.response_cache.enabled);
        assert_eq!(cfg.ai.response_cache.ttl_secs, 3600);

        // Partial object: the missing key keeps its default.
        let cfg: Config =
            serde_json::from_str(r#"{"ai": {"response_cache": {"enabled": true}}}"#).unwrap();
        assert!(cfg.ai.response_cache.enabled);
        assert_eq!(cfg.ai.response_cache.ttl_secs, 7 * 24 * 60 * 60);

        // Missing section under a present `ai` → cache stays off.
        let cfg: Config = serde_json::from_str(r#"{"ai": {"decide": {"shadow": false}}}"#).unwrap();
        assert!(!cfg.ai.response_cache.enabled);
    }

    /// C8: `ai.observations` is opt-in, off by default, and overlays like
    /// the rest of the `ai` object.
    #[test]
    fn ai_observations_config() {
        let dflt = default_config();
        assert!(!dflt.ai.observations.enabled);
        assert_eq!(dflt.ai.observations.min_cluster_size, 2);
        assert_eq!(dflt.ai.observations.max_members, 12);
        assert_eq!(dflt.ai.observations.hub_degree_max, 10);
        assert_eq!(dflt.ai.observations.max_clusters_per_repo, 25);
        assert_eq!(dflt.ai.observations.max_members_examined, 500);
        assert_eq!(dflt.ai.observations.max_scopes_per_project, 8);
        assert_eq!(dflt.ai.observations.max_llm_summaries_per_pass, 10);
        assert_eq!(
            dflt.ai.observations.families,
            vec![
                "fix_root_cause".to_string(),
                "issue".to_string(),
                "symbol".to_string(),
                "file".to_string()
            ]
        );
        assert!(!dflt.ai.observations.decide_shadow);

        let cfg: Config = serde_json::from_str(
            r#"{"ai": {"observations": {
                "enabled": true,
                "max_llm_summaries_per_pass": 3,
                "families": ["file", "bogus"]
            }}}"#,
        )
        .unwrap();
        assert!(cfg.ai.observations.enabled);
        assert_eq!(cfg.ai.observations.max_llm_summaries_per_pass, 3);
        assert_eq!(
            cfg.ai.observations.families,
            vec!["file".to_string(), "bogus".to_string()],
            "resolution (and the warn) happens in ibis-search"
        );
        // Partial object keeps the other defaults.
        assert_eq!(cfg.ai.observations.max_members, 12);

        // Missing section under a present `ai` → stays off.
        let cfg: Config = serde_json::from_str(r#"{"ai": {"decide": {"shadow": false}}}"#).unwrap();
        assert!(!cfg.ai.observations.enabled);
    }

    #[test]
    fn unknown_keys_ignored_and_defaults_kept() {
        // Overlay semantics: unknown keys ignored, missing keys keep defaults.
        let cfg: Config = serde_json::from_str(r#"{"nope": 1, "port": 4040}"#).unwrap();
        assert_eq!(cfg.port, 4040);
        assert_eq!(cfg.db_namespace, "ibis");
        assert_eq!(cfg.gemini_default_rpm, 100);
    }

    #[test]
    fn gemini_clouds_nesting_is_object_with_keys() {
        // ai.reasoning.clouds.gemini is an OBJECT containing keys — not a bare array.
        let cfg: Config = serde_json::from_str(
            r#"{"ai": {"reasoning": {"clouds": {"gemini": {"keys": [{"key": "k", "rpm": 15}]}}}}}"#,
        )
        .unwrap();
        assert_eq!(cfg.ai.reasoning.clouds.gemini.keys.len(), 1);
        assert_eq!(cfg.ai.reasoning.clouds.gemini.keys[0].rpm, 15);
    }

    #[test]
    fn resolve_path_table() {
        let base_dir = "C:\\app\\bin";
        let cases = vec![
            (
                "Absolute Path remains same",
                "D:\\data\\file.db",
                "D:\\data\\file.db",
            ),
            ("Empty Path remains empty", "", ""),
            ("Unix absolute path", "/var/log/file.db", "/var/log/file.db"),
            (
                "Windows mixed slash absolute path",
                "E:/data\\file.db",
                "E:/data\\file.db",
            ),
        ];
        for (name, path, expected) in cases {
            let got = resolve_path(base_dir, path);
            assert_eq!(got, expected, "case {name}");
        }
        let rel = resolve_path(base_dir, "data/file.db");
        // The base is a Windows-style path; the host MAIN_SEPARATOR decides the
        // join output ("\" on Windows, "/" elsewhere).
        let expected = if cfg!(windows) {
            "C:\\app\\bin\\data\\file.db"
        } else {
            "C:\\app\\bin/data/file.db"
        };
        assert_eq!(rel, expected);
        let dot = resolve_path(base_dir, ".");
        assert_eq!(dot, base_dir);
    }

    #[test]
    fn normalize_and_live_runtime() {
        assert_eq!(normalize_runtime_mode(" Personal "), "personal");
        assert!(is_live_runtime("plugin"));
        assert!(is_live_runtime("PERSONAL"));
        assert!(!is_live_runtime("server"));
        assert!(!is_live_runtime(""));
        assert!(is_plugin_runtime("PLUGIN"));
    }

    #[test]
    fn git_token_matching() {
        let cfg = Config {
            git_tokens: HashMap::from([
                ("github.com".to_string(), "gh".to_string()),
                ("default".to_string(), "def".to_string()),
            ]),
            ..Default::default()
        };
        assert_eq!(cfg.get_git_token("https://github.com/x/y"), "gh");
        assert_eq!(cfg.get_git_token("https://gitlab.com/x/y"), "def");
        let star = Config {
            git_tokens: HashMap::from([("*".to_string(), "any".to_string())]),
            ..Default::default()
        };
        assert_eq!(star.get_git_token("https://gitlab.com/x"), "any");
    }
}
