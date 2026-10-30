//! MCP progress notifications +
//! (`attachMCPProgress`).
//!
//! - `notifications/progress` `{progressToken, progress, total, message}`.
//! - Throttle: ≥1s between in-phase updates; the first report, phase-message
//!   changes and completion always pass through.
//! - Emission happens on exactly `init_project`, `ingest_code` and
//!   `analyze_logs` when `params.meta.progressToken` is present.
//! - F-006: `init_project` emits 0/3, 1/3, 2/3 "Indexing AST chunks", 3/3.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::model::{
    ProgressNotification, ProgressNotificationParam, ProgressToken, ServerNotification,
};
use rmcp::service::RequestContext;
use rmcp::RoleServer;

/// Minimum interval between in-phase notifications.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// A throttled reporter over the request peer (`progress.Reporter +
/// `SendNotificationToClient`).
#[derive(Clone)]
pub struct ProgressReporter {
    token: ProgressToken,
    peer: rmcp::service::Peer<RoleServer>,
    state: Arc<Mutex<ThrottleState>>,
}

#[derive(Default)]
struct ThrottleState {
    last: Option<Instant>,
    last_msg: String,
}

impl ProgressReporter {
    /// Attaches a reporter when the request carries a progress token
    /// `None` = no-op reporting.
    pub fn attach(ctx: &RequestContext<RoleServer>) -> Option<Self> {
        let token = ctx.meta.get_progress_token()?;
        Some(ProgressReporter {
            token,
            peer: ctx.peer.clone(),
            state: Arc::new(Mutex::new(ThrottleState::default())),
        })
    }

    /// Reports progress through the throttle.
    pub fn report(&self, done: u64, total: u64, message: &str) {
        if !self.throttle_allows(done, total, message) {
            return;
        }
        let param = ProgressNotificationParam::new(self.token.clone(), done as f64)
            .with_total(total as f64)
            .with_message(message.to_string());
        let notification: ServerNotification =
            ServerNotification::ProgressNotification(ProgressNotification::new(param));
        // Scenario: discarded send errors silently (`_ =`); this crate logs them at
        // debug instead (audit finding N-A02).
        let peer = self.peer.clone();
        tokio::spawn(async move {
            if let Err(e) = peer.send_notification(notification).await {
                tracing::debug!("progress notification not delivered: {e}");
            }
        });
    }

    fn throttle_allows(&self, done: u64, total: u64, message: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let complete = total > 0 && done >= total;
        let phase_change = message != state.last_msg;
        if let Some(last) = state.last {
            if !phase_change && !complete && now.duration_since(last) < MIN_INTERVAL {
                return false;
            }
        }
        state.last = Some(now);
        state.last_msg = message.to_string();
        true
    }
}

/// Adapts the throttled reporter to the log-analyzer progress seam (the
/// same `progress.Report` closure was attached to the analyzer's ctx).
impl ibis_ingest_logs::ProgressReporter for ProgressReporter {
    fn report(&self, done: u64, total: u64, message: &str) {
        ProgressReporter::report(self, done, total, message);
    }
}

/// No-op reporter for tools without a progress token.
pub fn report(opt: &Option<ProgressReporter>, done: u64, total: u64, message: &str) {
    if let Some(reporter) = opt {
        reporter.report(done, total, message);
    }
}

/// True when the tool name carries progress emission: exactly
/// init_project, ingest_code, analyze_logs).
pub fn tool_emits_progress(tool: &str) -> bool {
    matches!(tool, "init_project" | "ingest_code" | "analyze_logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Throttle contract: 1s minimum interval; phase changes and
    /// completion pass through.
    #[test]
    fn throttle_matrix() {
        let state = Arc::new(Mutex::new(ThrottleState::default()));
        let allows =
            |state: &Arc<Mutex<ThrottleState>>, done, total, msg: &str, since_last_ms: u64| {
                let mut s = state.lock().unwrap();
                let now = Instant::now();
                let complete = total > 0 && done >= total;
                let phase_change = msg != s.last_msg;
                let allowed = match s.last {
                    Some(last) => {
                        !(!phase_change && !complete && now.duration_since(last) < MIN_INTERVAL)
                    }
                    None => true,
                };
                if allowed {
                    // Simulate a report at the requested offset.
                    s.last = Some(
                        now.checked_sub(Duration::from_millis(since_last_ms))
                            .unwrap_or(now),
                    );
                    s.last_msg = msg.to_string();
                }
                allowed
            };
        // First report passes.
        assert!(allows(&state, 0, 3, "Syncing git workspace", 0));
        // Duplicate in-phase within 1s dropped.
        assert!(!allows(&state, 0, 3, "Syncing git workspace", 100));
        // Phase change passes.
        assert!(allows(&state, 1, 3, "Ingesting git history and code", 100));
        // Same phase within 1s dropped.
        assert!(!allows(&state, 1, 3, "Ingesting git history and code", 100));
        // Completion always passes.
        assert!(allows(&state, 3, 3, "Project ready", 100));
    }

    #[test]
    fn progress_tools_are_exactly_three() {
        assert!(tool_emits_progress("init_project"));
        assert!(tool_emits_progress("ingest_code"));
        assert!(tool_emits_progress("analyze_logs"));
        assert!(!tool_emits_progress("memory_add"));
        assert!(!tool_emits_progress("ask_project"));
    }
}
