//! threski-settings — the configuration engine, HTTP settings API, embedded
//! settings SPA and the plugin-mode ephemeral loopback server (
//! `internal/settings`: , , , web/).
//!
//! Surface (the contract):
//! - [`engine`]: hardware probe (F-003: 64-bit VRAM via the Windows registry
//!   instead of the uint32-capped WMI `AdapterRAM`), model-tier selection,
//!   recommendations, and apply-with-diff used by the wizard and the API.
//! - [`http`]: `GET/POST /api/v1/settings`, memory list/export/share — an
//!   axum [`Router`](axum::Router) with **absolute** paths that
//!   `threski_httpserver::mux::handler` merges (`mux.Handle without
//!   StripPrefix).
//! - [`ui`]: the carried SPA embedded via `include_dir` (D13) — a Router with
//!   **relative** routes nested under `/settings/` with the prefix stripped.
//! - [`ephemeral`]: loopback-only settings server with one-shot URL token →
//!   session cookie, 20-min idle shutdown and the 2s post-apply stop, used
//!   when runtime_mode=plugin (stdio has no HTTP).
//!
//! Deliberate deltas (D15, rows K-01..): the remaining credential
//! fields are masked in GET responses; the apply body is size-capped; the
//! probe fallback order is config port first, then 3030/3031 (F-004).

pub mod engine;
pub mod ephemeral;
pub mod gpu;
pub mod http;
pub mod ui;

pub use engine::{
    apply, default_model_for_tier, mask_secret, probe, probe_blocking, probe_with, recommend,
    resolve_local_model, tier_for, DiffEntry, FieldRec, HardwareReport, Recommendations, Tier,
    UserChoices,
};
pub use ephemeral::{
    ensure_ephemeral, ensure_ephemeral_with_memory, open_settings_url, probe_personal_settings,
    stop_ephemeral, SettingsError, EPHEMERAL_IDLE_TIMEOUT, EPHEMERAL_SHUTDOWN_GRACE, TOKEN_HEADER,
};
pub use http::{
    open_url, settings_api_router, MemoryApiError, MemoryService, SearchMemoryAdapter,
    MAX_APPLY_BODY_BYTES,
};
pub use ui::settings_ui_router;
