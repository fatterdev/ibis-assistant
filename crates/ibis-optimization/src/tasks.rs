//! E16 — pure task-store model and SQL builders.
//!
//! A durable `task` row is the envelope of a long pass: lease/fencing,
//! checkpoint and recovery decisions live here as pure functions, so the
//! runner (`ibis-server`) only binds values and executes statements. Nothing
//! in this module touches the database; the live UNIQUE/NONE probe lives in
//! `crates/ibis-db/tests/live_surreal.rs`.

use ibis_db::ids::is_safe_record_id;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use surrealdb::types::SurrealValue;

use crate::observations::ObservationPassReport;

/// The only task kind in v1. Kinds are a Rust whitelist (not a DB union) so
/// adding one does not require a `REMOVE`/`DEFINE` cycle.
pub const TASK_KIND_MEMORY_CONSOLIDATE: &str = "memory_consolidate";

/// Shipped policy defaults (mirror `ibis_core::config::TasksConfig`).
pub const DEFAULT_LEASE_SECS: u64 = 30;
pub const DEFAULT_HEARTBEAT_SECS: u64 = 10;
pub const DEFAULT_MAX_RECOVERIES: u32 = 3;
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// Clamp bounds from the E16 contract (§4.7).
pub const LEASE_SECS_MIN: u64 = 10;
pub const LEASE_SECS_MAX: u64 = 600;
pub const MAX_RECOVERIES_MIN: u32 = 1;
pub const MAX_RECOVERIES_MAX: u32 = 10;
pub const RETENTION_DAYS_MIN: u64 = 1;
pub const RETENTION_DAYS_MAX: u64 = 365;

/// Validation failures for task identifiers and active keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskModelError {
    EmptyKind,
    EmptyProject,
    EmptyTaskId,
    UnsafeRecordId,
    NonTerminalFinish,
}

impl std::fmt::Display for TaskModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskModelError::EmptyKind => write!(f, "task kind must not be empty"),
            TaskModelError::EmptyProject => write!(f, "task project must not be empty"),
            TaskModelError::EmptyTaskId => write!(f, "task id must not be empty"),
            TaskModelError::UnsafeRecordId => write!(f, "task id is not a safe record id"),
            TaskModelError::NonTerminalFinish => {
                write!(f, "finish status must be completed or failed")
            }
        }
    }
}

impl std::error::Error for TaskModelError {}

/// Clamped runtime policy derived from `tasks.*` (`ibis_core::config`).
/// Fields are private so every construction goes through [`TaskPolicy::new`]
/// (or [`TaskPolicy::default`]) and cannot bypass the clamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskPolicy {
    lease_secs: u64,
    heartbeat_secs: u64,
    max_recoveries: u32,
    retention_days: u64,
}

impl Default for TaskPolicy {
    fn default() -> Self {
        TaskPolicy {
            lease_secs: DEFAULT_LEASE_SECS,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            max_recoveries: DEFAULT_MAX_RECOVERIES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl TaskPolicy {
    /// Applies the documented clamps: lease `[10, 600]`, heartbeat
    /// `[1, lease/2]`, recoveries `[1, 10]`, retention `[1, 365]`.
    pub fn new(
        lease_secs: u64,
        heartbeat_secs: u64,
        max_recoveries: u32,
        retention_days: u64,
    ) -> Self {
        let lease_secs = lease_secs.clamp(LEASE_SECS_MIN, LEASE_SECS_MAX);
        let heartbeat_secs = heartbeat_secs.clamp(1, (lease_secs / 2).max(1));
        TaskPolicy {
            lease_secs,
            heartbeat_secs,
            max_recoveries: max_recoveries.clamp(MAX_RECOVERIES_MIN, MAX_RECOVERIES_MAX),
            retention_days: retention_days.clamp(RETENTION_DAYS_MIN, RETENTION_DAYS_MAX),
        }
    }

    /// Duration literal interpolated into claim/heartbeat SQL (an integer,
    /// never user content).
    pub fn lease_literal(&self) -> String {
        format!("{}s", self.lease_secs)
    }

    /// Retention literal interpolated into the prune SQL.
    pub fn retention_literal(&self) -> String {
        format!("{}d", self.retention_days)
    }

    pub fn lease_secs(&self) -> u64 {
        self.lease_secs
    }

    pub fn heartbeat_secs(&self) -> u64 {
        self.heartbeat_secs
    }

    pub fn max_recoveries(&self) -> u32 {
        self.max_recoveries
    }

    pub fn retention_days(&self) -> u64 {
        self.retention_days
    }
}

/// Task lifecycle states, mirroring the `task_status` DB type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(TaskStatus::Queued),
            "running" => Some(TaskStatus::Running),
            "completed" => Some(TaskStatus::Completed),
            "failed" => Some(TaskStatus::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Failed)
    }

    /// Terminal states are immutable; `running` may requeue on shutdown or
    /// recovery and finish as either terminal state.
    pub fn can_transition_to(self, next: TaskStatus) -> bool {
        match self {
            TaskStatus::Queued => next == TaskStatus::Running,
            TaskStatus::Running => matches!(
                next,
                TaskStatus::Queued | TaskStatus::Completed | TaskStatus::Failed
            ),
            TaskStatus::Completed | TaskStatus::Failed => false,
        }
    }
}

/// What boot recovery should do with a task row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    Requeue,
    Fail,
    Skip,
}

/// Pure recovery matrix (§4.2/§4.4): only an expired `running` lease is
/// actionable; terminal and queued rows are skipped, and the recovery budget
/// decides between requeue and terminal failure.
pub fn recovery_decision(
    status: TaskStatus,
    lease_expired: bool,
    recoveries: u32,
    max_recoveries: u32,
) -> RecoveryDecision {
    if status != TaskStatus::Running || !lease_expired {
        return RecoveryDecision::Skip;
    }
    if recoveries < max_recoveries {
        RecoveryDecision::Requeue
    } else {
        RecoveryDecision::Fail
    }
}

/// Checkpoint persisted on the task row: observability plus the budget ledger,
/// never the source of truth for resuming (the backlog is).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
pub struct TaskCheckpoint {
    pub last_item_id: Option<String>,
    pub items_done: u64,
    pub items_total: u64,
    pub summaries_used: u64,
    pub scopes_done: u64,
    pub scopes_total: u64,
}

impl TaskCheckpoint {
    /// Always returns the complete object, including fields not yet known
    /// (`last_item_id: null`, counters at zero).
    pub fn to_json(&self) -> Value {
        json!({
            "last_item_id": self.last_item_id,
            "items_done": self.items_done,
            "items_total": self.items_total,
            "summaries_used": self.summaries_used,
            "scopes_done": self.scopes_done,
            "scopes_total": self.scopes_total,
        })
    }
}

/// Snapshot of the parameters a task was born with. The resume must use these
/// values, not the live config (E16 §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
pub struct TaskParams {
    pub max_items: u64,
    pub max_llm_summaries: u64,
    pub mode: String,
}

/// Observer seam for a long pass: `consolidate_loop` and `observations_pass`
/// call it at item/scope boundaries. Implementations must be cheap and
/// non-blocking (the server impl buffers DB writes in a writer task).
pub trait PassCheckpoint: Send + Sync {
    /// false = lease lost or shutdown: the pass stops at the next item boundary.
    fn should_continue(&self) -> bool;
    /// After an item was persisted (`ok` | `noise` | `error`).
    fn item_done(&self, item_id: &str, outcome: &str, items_total: usize);
    /// `items_total` is known right after the backlog select; default no-op.
    fn items_total(&self, _total: usize) {}
    /// After reconcile + synthesis of one scope.
    fn scope_done(&self, report: &ObservationPassReport);
    /// Remaining synthesis budget (`None` = use the config cap).
    fn remaining_summaries(&self) -> Option<usize>;
}

/// Remaining synthesis budget for a task: `cap - used`, saturating at zero.
pub fn remaining_summaries(cap: usize, used: usize) -> usize {
    cap.saturating_sub(used)
}

/// A fresh 32-hex task id (UUID v4, simple form).
pub fn new_task_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Renders and validates `task:⟨<32 hex>⟩`; the id is interpolated into SQL
/// only after `is_safe_record_id` accepts it.
pub fn task_record_id(task_id: &str) -> Result<String, TaskModelError> {
    let id = task_id.trim();
    if id.is_empty() {
        return Err(TaskModelError::EmptyTaskId);
    }
    // Accept the plain key, the full `task:<key>` form and the guillemet
    // rendering returned by `record_id_to_plain`/the driver.
    let key = id.strip_prefix("task:").unwrap_or(id);
    let key = key.trim_matches(|c| c == '\u{27e8}' || c == '\u{27e9}');
    if key.len() != 32 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(TaskModelError::UnsafeRecordId);
    }
    let record = ibis_db::ids::format_record_id("task", key);
    if !is_safe_record_id(&record) {
        return Err(TaskModelError::UnsafeRecordId);
    }
    Ok(record)
}

/// `"{kind}:{project_lower}"`, the UNIQUE guard for active tasks. Empty or
/// whitespace-only inputs are rejected.
pub fn active_key(kind: &str, project: &str) -> Result<String, TaskModelError> {
    let kind = kind.trim();
    if kind.is_empty() {
        return Err(TaskModelError::EmptyKind);
    }
    let project = project.trim().to_lowercase();
    if project.is_empty() {
        return Err(TaskModelError::EmptyProject);
    }
    Ok(format!("{kind}:{project}"))
}

/// `CREATE` statement for a queued task. `$kind`, `$project`, `$params`,
/// `$active_key` and `$actor` are bound by the caller.
pub fn create_task_sql(task_id: &str) -> Result<String, TaskModelError> {
    let id = task_record_id(task_id)?;
    Ok(format!(
        "CREATE {id} SET kind = $kind, status = 'queued', project = $project, \
         params = $params, active_key = $active_key, requested_by = $actor;"
    ))
}

/// Atomic claim: one statement, the winner is whoever gets rows back.
pub fn claim_task_sql(task_id: &str, policy: &TaskPolicy) -> Result<String, TaskModelError> {
    let id = task_record_id(task_id)?;
    let lease_secs = policy.lease_secs;
    Ok(format!(
        "UPDATE {id} SET status = 'running', lease_owner = $owner, \
         lease_epoch = lease_epoch + 1, lease_expires_at = time::now() + {lease_secs}s, \
         claims = claims + 1, claimed_at = time::now(), updated_at = time::now() \
         WHERE status = 'queued' OR (status = 'running' AND lease_expires_at < time::now()) \
         RETURN AFTER;"
    ))
}

/// Heartbeat with fencing on owner + epoch; zero rows means the lease was lost.
pub fn heartbeat_task_sql(task_id: &str, policy: &TaskPolicy) -> Result<String, TaskModelError> {
    let id = task_record_id(task_id)?;
    let lease_secs = policy.lease_secs;
    Ok(format!(
        "UPDATE {id} SET lease_expires_at = time::now() + {lease_secs}s, \
         updated_at = time::now() \
         WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running' \
         RETURN AFTER;"
    ))
}

/// Checkpoint write with the same fencing; `$checkpoint` is the object from
/// [`TaskCheckpoint::to_json`].
pub fn checkpoint_task_sql(task_id: &str) -> Result<String, TaskModelError> {
    let id = task_record_id(task_id)?;
    Ok(format!(
        "UPDATE {id} SET checkpoint = $checkpoint, updated_at = time::now() \
         WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running' \
         RETURN AFTER;"
    ))
}

/// Terminal finish; clears `active_key` so the UNIQUE slot is released. The
/// terminal state is a validated enum, not a free string: `queued`/`running`
/// are rejected before any SQL is built.
pub fn finish_task_sql(task_id: &str, terminal: TaskStatus) -> Result<String, TaskModelError> {
    if !terminal.is_terminal() {
        return Err(TaskModelError::NonTerminalFinish);
    }
    let id = task_record_id(task_id)?;
    let terminal = terminal.as_str();
    Ok(format!(
        "UPDATE {id} SET status = '{terminal}', finished_at = time::now(), \
         updated_at = time::now(), active_key = NONE, error = $error, \
         checkpoint = $checkpoint \
         WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running' \
         RETURN AFTER;"
    ))
}

/// Clean shutdown requeue: keeps `active_key`, consumes no recovery.
pub fn requeue_task_sql(task_id: &str) -> Result<String, TaskModelError> {
    let id = task_record_id(task_id)?;
    Ok(format!(
        "UPDATE {id} SET status = 'queued', lease_owner = NONE, \
         lease_expires_at = NONE, updated_at = time::now() \
         WHERE lease_owner = $owner AND lease_epoch = $epoch AND status = 'running' \
         RETURN AFTER;"
    ))
}

/// Boot recovery: expired leases with budget left return to the queue.
pub fn recovery_requeue_sql() -> &'static str {
    "UPDATE task SET status = 'queued', lease_owner = NONE, lease_expires_at = NONE, \
     recoveries = recoveries + 1, updated_at = time::now() \
     WHERE status = 'running' AND lease_expires_at < time::now() \
     AND recoveries < $max_recoveries;"
}

/// Boot recovery: expired leases out of budget fail and release the slot.
pub fn recovery_fail_sql() -> &'static str {
    "UPDATE task SET status = 'failed', error = 'lease_expired', finished_at = time::now(), \
     active_key = NONE, recoveries = recoveries + 1, updated_at = time::now() \
     WHERE status = 'running' AND lease_expires_at < time::now() \
     AND recoveries >= $max_recoveries;"
}

/// Prune of terminal tasks past retention. `RETURN BEFORE` is the 4.0 contract
/// (DELETE … RETURN AFTER renders NONE); the caller logs the returned length.
pub fn prune_task_sql(retention_days: u64) -> String {
    let days = retention_days.clamp(RETENTION_DAYS_MIN, RETENTION_DAYS_MAX);
    format!(
        "DELETE task WHERE status IN ['completed', 'failed'] \
         AND finished_at < time::now() - {days}d RETURN BEFORE;"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn active_key_is_normalized() {
        assert_eq!(
            active_key(TASK_KIND_MEMORY_CONSOLIDATE, " App ").unwrap(),
            "memory_consolidate:app"
        );
        assert_eq!(
            active_key(TASK_KIND_MEMORY_CONSOLIDATE, "MiXeD").unwrap(),
            "memory_consolidate:mixed"
        );
        assert_eq!(
            active_key(TASK_KIND_MEMORY_CONSOLIDATE, "  "),
            Err(TaskModelError::EmptyProject)
        );
        assert_eq!(active_key("  ", "app"), Err(TaskModelError::EmptyKind));
    }

    #[test]
    fn task_id_is_32_hex() {
        let id = new_task_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let record = task_record_id(&id).unwrap();
        assert_eq!(record, format!("task:\u{27e8}{id}\u{27e9}"));
        assert_eq!(
            task_record_id(&format!("task:{id}")).unwrap(),
            record,
            "full record-id form is accepted and normalized"
        );
        assert!(is_safe_record_id(&record));
        assert_eq!(task_record_id("  "), Err(TaskModelError::EmptyTaskId));
        // Short/unsafe ids never reach SQL interpolation.
        assert_eq!(task_record_id("abc"), Err(TaskModelError::UnsafeRecordId));
        assert_eq!(
            task_record_id("0123456789abcdef0123456789abcde;"),
            Err(TaskModelError::UnsafeRecordId)
        );
    }

    #[test]
    fn claim_sql_shape() {
        let sql = claim_task_sql(ID, &TaskPolicy::default()).unwrap();
        assert!(sql.contains("status = 'queued'"));
        assert!(sql.contains("lease_expires_at < time::now()"));
        assert!(sql.contains("lease_epoch = lease_epoch + 1"));
        assert!(sql.contains("RETURN AFTER"));
        assert!(sql.contains("30s"));
    }

    #[test]
    fn heartbeat_sql_shape() {
        let sql = heartbeat_task_sql(ID, &TaskPolicy::default()).unwrap();
        assert!(sql.contains("lease_owner = $owner"));
        assert!(sql.contains("lease_epoch = $epoch"));
        assert!(sql.contains("status = 'running'"));
        assert!(sql.contains("30s"));
    }

    #[test]
    fn finish_sql_clears_active_key() {
        let sql = finish_task_sql(ID, TaskStatus::Completed).unwrap();
        assert!(sql.contains("active_key = NONE"));
        assert!(sql.contains("finished_at = time::now()"));
        assert!(sql.contains("error = $error"));
        assert!(sql.contains("status = 'completed'"));
        assert_eq!(
            finish_task_sql(ID, TaskStatus::Queued),
            Err(TaskModelError::NonTerminalFinish)
        );
        assert_eq!(
            finish_task_sql(ID, TaskStatus::Running),
            Err(TaskModelError::NonTerminalFinish)
        );
    }

    #[test]
    fn recovery_and_prune_sql_shape() {
        let requeue = recovery_requeue_sql();
        assert!(requeue.contains("status = 'queued'"));
        assert!(requeue.contains("recoveries = recoveries + 1"));
        assert!(requeue.contains("$max_recoveries"));
        let fail = recovery_fail_sql();
        assert!(fail.contains("status = 'failed'"));
        assert!(fail.contains("active_key = NONE"));
        assert!(fail.contains("$max_recoveries"));

        let prune = prune_task_sql(30);
        assert!(prune.contains("status IN ['completed', 'failed']"));
        assert!(prune.contains("30d"));
        assert!(prune.contains("RETURN BEFORE"));
    }

    #[test]
    fn recovery_matrix() {
        // Expired running lease with budget left → requeue.
        assert_eq!(
            recovery_decision(TaskStatus::Running, true, 0, 3),
            RecoveryDecision::Requeue
        );
        // Expired running lease out of budget → failed.
        assert_eq!(
            recovery_decision(TaskStatus::Running, true, 3, 3),
            RecoveryDecision::Fail
        );
        // Live lease → nothing to do.
        assert_eq!(
            recovery_decision(TaskStatus::Running, false, 0, 3),
            RecoveryDecision::Skip
        );
        // Queued and terminal rows are never recovered.
        assert_eq!(
            recovery_decision(TaskStatus::Queued, true, 0, 3),
            RecoveryDecision::Skip
        );
        assert_eq!(
            recovery_decision(TaskStatus::Completed, true, 0, 3),
            RecoveryDecision::Skip
        );
    }

    #[test]
    fn terminal_states_are_immutable() {
        let all = [
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::Completed,
            TaskStatus::Failed,
        ];
        for terminal in [TaskStatus::Completed, TaskStatus::Failed] {
            assert!(terminal.is_terminal());
            for next in all {
                assert!(
                    !terminal.can_transition_to(next),
                    "{terminal:?} must not transition to {next:?}"
                );
            }
        }
        assert!(TaskStatus::Queued.can_transition_to(TaskStatus::Running));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Completed));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Failed));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Queued));
    }

    #[test]
    fn remaining_summaries_saturates() {
        assert_eq!(remaining_summaries(10, 10), 0);
        assert_eq!(remaining_summaries(10, 3), 7);
        assert_eq!(remaining_summaries(0, 5), 0);
        assert_eq!(remaining_summaries(10, 0), 10);
    }

    #[test]
    fn checkpoint_defaults() {
        let checkpoint = TaskCheckpoint::default();
        let value = checkpoint.to_json();
        assert_eq!(value["last_item_id"], Value::Null);
        assert_eq!(value["items_done"], 0);
        assert_eq!(value["items_total"], 0);
        assert_eq!(value["summaries_used"], 0);
        assert_eq!(value["scopes_done"], 0);
        assert_eq!(value["scopes_total"], 0);
        assert_eq!(value.as_object().map(|o| o.len()), Some(6));
    }

    #[test]
    fn clamp_matrix() {
        let low = TaskPolicy::new(0, 0, 0, 0);
        assert_eq!(low.lease_secs(), LEASE_SECS_MIN);
        assert_eq!(low.heartbeat_secs(), 1);
        assert_eq!(low.max_recoveries(), MAX_RECOVERIES_MIN);
        assert_eq!(low.retention_days(), RETENTION_DAYS_MIN);

        let high = TaskPolicy::new(10_000, 10_000, 99, 10_000);
        assert_eq!(high.lease_secs(), LEASE_SECS_MAX);
        assert_eq!(high.heartbeat_secs(), LEASE_SECS_MAX / 2);
        assert_eq!(high.max_recoveries(), MAX_RECOVERIES_MAX);
        assert_eq!(high.retention_days(), RETENTION_DAYS_MAX);

        let default = TaskPolicy::default();
        assert_eq!(default.lease_secs(), DEFAULT_LEASE_SECS);
        assert_eq!(default.heartbeat_secs(), DEFAULT_HEARTBEAT_SECS);
        assert_eq!(default.lease_literal(), "30s");
        assert_eq!(default.retention_literal(), "30d");

        // Heartbeat never exceeds half the (clamped) lease.
        let policy = TaskPolicy::new(20, 20, 3, 30);
        assert_eq!(policy.heartbeat_secs(), 10);
    }

    /// The shipped `tasks.*` defaults and the pure policy defaults must not
    /// drift: the server builds the policy from the config values.
    #[test]
    fn config_defaults_match_policy() {
        let cfg = ibis_core::config::TasksConfig::default();
        let policy = TaskPolicy::new(
            cfg.lease_secs,
            cfg.heartbeat_secs,
            cfg.max_recoveries,
            cfg.retention_days,
        );
        assert_eq!(policy, TaskPolicy::default());
    }
}
