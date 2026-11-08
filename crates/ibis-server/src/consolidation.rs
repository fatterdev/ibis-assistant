//! F4 §B.5 — the daemon-owned consolidation loop.
//!
//! The consolidation backlog is drained by the long-lived runtime, never by
//! client surfaces: the daemon (`ibis run` / `ibis serve --team`) discovers
//! the projects that still hold raw observations and runs one
//! [`ibis_optimization::Optimizer::consolidate_loop`] pass per project on a
//! fixed cadence. Opt-in only (`--consolidate` / `ai.consolidation.enabled`,
//! off by default locally — the team runbook recommends ON); the weekly
//! `memory_consolidate` automation stays the explicit human trigger, and
//! client surfaces (stdio plugin/IDE-host children) never start the loop
//! (T3: one runtime owns consolidation, so duplicate-supersede disappears
//! with the single-runtime topology).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use ibis_core::config::Config;

use crate::state::AppState;

/// Floor for `ai.consolidation.interval_secs`: a pass of LLM calls under a
/// 60s cadence would only burn rate limits.
const MIN_INTERVAL_SECS: u64 = 60;

/// Observations drained per project pass (the `memory_consolidate` tool
/// default, `DEFAULT_CONSOLIDATION_ITEMS`): the loop drains gradually,
/// never a bulk sweep.
const MAX_ITEMS_PER_PROJECT: usize = 20;

/// Gate for the daemon-owned loop: opt-in, and only the long-lived HTTP
/// daemon runs it — stdio children (plugin/IDE-host spawn contract) and
/// plugin isolation never do.
pub fn enabled(cfg: &Config) -> bool {
    cfg.ai.consolidation.enabled && cfg.mode != "stdio" && !cfg.is_plugin()
}

/// Starts the loop when the gate passes; when an operator opted in but the
/// runtime cannot honor it, the reason is logged (the setting must never be
/// silently dead).
pub fn start(state: &Arc<AppState>, shutdown: CancellationToken) {
    let cfg = state.config();
    if !enabled(&cfg) {
        return;
    }
    if state.db().is_none() {
        tracing::warn!(
            "Consolidation loop enabled but no database is connected — not started (degraded mode)"
        );
        return;
    }
    let interval = Duration::from_secs(cfg.ai.consolidation.interval_secs.max(MIN_INTERVAL_SECS));
    tracing::info!(
        "Consolidation loop started (daemon-owned, interval {}s, up to {MAX_ITEMS_PER_PROJECT} observations per project pass)",
        interval.as_secs()
    );
    let state = state.clone();
    tokio::spawn(async move {
        // First tick fires immediately: a daemon restart drains its backlog
        // right away. `Delay` keeps a slow pass from bursting catch-up ticks,
        // and the single-task select keeps passes strictly sequential.
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => run_pass(&state).await,
            }
        }
        tracing::info!("Consolidation loop stopped");
    });
}

/// One pass: discover the projects that still hold `current` observations
/// and drain each one (store-driven, so team clients can capture for repos
/// the server config does not know).
async fn run_pass(state: &Arc<AppState>) {
    let mut optimizer = ibis_optimization::Optimizer::new(
        state.db(),
        Some(Arc::new(crate::state::ClientAIAdapter(state.ai.clone()))
            as Arc<dyn ibis_ai::AIProvider>),
        Some(state.search()),
    );
    // C9: the duplicate-lookup similarity floor is config-driven.
    optimizer.dup_similarity_min = state.config().ai.consolidation.dup_similarity_min;
    optimizer.schema_enforcement = ibis_search::memory::details::SchemaEnforcement::resolve(
        &state.config().memory.schema_enforcement,
    );
    let projects = match optimizer.pending_observation_projects().await {
        Ok(projects) => projects,
        Err(err) => {
            tracing::warn!("ConsolidationLoop: project discovery failed: {err}");
            return;
        }
    };
    if projects.is_empty() {
        return;
    }
    tracing::info!("ConsolidationLoop: pass over {} project(s)", projects.len());
    for project in projects {
        match optimizer
            .consolidate_loop(&project, MAX_ITEMS_PER_PROJECT)
            .await
        {
            Err(err) => tracing::warn!("ConsolidationLoop: pass for {project} failed: {err}"),
            Ok(()) => {
                tracing::info!("ConsolidationLoop: pass for {project} completed");
                // Same projection refresh as the memory_consolidate tool.
                state.maybe_project_memory(&project).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off by default: nothing runs until the operator opts in (F4 §B.5).
    #[test]
    fn default_config_keeps_the_loop_off() {
        assert!(!enabled(&Config::default()));
    }

    /// Opt-in via config engages the loop on the daemon — personal and
    /// server runtime modes alike (both are long-lived daemons).
    #[test]
    fn enabled_config_runs_on_the_daemon() {
        let mut cfg = Config::default();
        cfg.ai.consolidation.enabled = true;
        assert!(enabled(&cfg));
        cfg.runtime_mode = "personal".into();
        assert!(enabled(&cfg));
        cfg.runtime_mode = "server".into();
        assert!(enabled(&cfg));
    }

    /// stdio children (the IDE-host/plugin spawn contract) are client
    /// surfaces: they never consolidate, T3.
    #[test]
    fn stdio_surfaces_never_run_the_loop() {
        let mut cfg = Config::default();
        cfg.ai.consolidation.enabled = true;
        cfg.mode = "stdio".into();
        assert!(!enabled(&cfg), "stdio is a client surface");
    }

    /// Plugin isolation is a client surface too, whatever the config says.
    #[test]
    fn plugin_isolation_never_runs_the_loop() {
        let mut cfg = Config::default();
        cfg.ai.consolidation.enabled = true;
        cfg.runtime_mode = "plugin".into();
        assert!(!enabled(&cfg));
    }
}
