//! E16 — durable task store and runner for long passes.
//!
//! The store is a thin adapter over [`ibis_db::executor::Executor`]; the
//! runner owns the lease/fencing protocol and the checkpoint writer. Both are
//! fail-open at the caller: a bookkeeping error logs a warning and the pass
//! still runs without a lease (§4.6).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use surrealdb::types::{Datetime, RecordId, SurrealValue, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use ibis_db::executor::{Executor, QueryOutcome, Vars};
use ibis_db::ids::record_id_to_plain;
use ibis_optimization::tasks::{
    active_key, checkpoint_task_sql, claim_task_sql, create_task_sql, finish_task_sql,
    heartbeat_task_sql, new_task_id, prune_task_sql, recovery_fail_sql, recovery_requeue_sql,
    remaining_summaries, requeue_task_sql, task_record_id, PassCheckpoint, TaskCheckpoint,
    TaskModelError, TaskParams, TaskPolicy, TaskStatus, TASK_KIND_MEMORY_CONSOLIDATE,
};
use ibis_optimization::Optimizer;

/// A `task` row as decoded from SurrealDB.
#[derive(Debug, Clone, Serialize, SurrealValue)]
pub struct TaskRow {
    pub id: RecordId,
    pub kind: String,
    pub status: String,
    pub project: Option<String>,
    pub params: Option<TaskParams>,
    pub checkpoint: Option<TaskCheckpoint>,
    pub active_key: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub lease_expires_at: Option<Datetime>,
    pub claims: i64,
    pub recoveries: i64,
    pub error: Option<String>,
    pub requested_by: Option<String>,
    pub created_at: Datetime,
    pub updated_at: Datetime,
    pub claimed_at: Option<Datetime>,
    pub finished_at: Option<Datetime>,
}

/// A task to create, with the parameter snapshot it must resume with.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub kind: String,
    pub project: String,
    pub params: TaskParams,
    pub active_key: String,
    pub actor: String,
}

impl NewTask {
    /// Builds the v1 `memory_consolidate` task envelope.
    pub fn memory_consolidate(
        project: &str,
        max_items: usize,
        max_llm_summaries: usize,
        actor: &str,
    ) -> Result<Self, TaskModelError> {
        Ok(NewTask {
            kind: TASK_KIND_MEMORY_CONSOLIDATE.to_string(),
            project: project.trim().to_string(),
            params: TaskParams {
                max_items: max_items as u64,
                max_llm_summaries: max_llm_summaries as u64,
                mode: TASK_KIND_MEMORY_CONSOLIDATE.to_string(),
            },
            active_key: active_key(TASK_KIND_MEMORY_CONSOLIDATE, project)?,
            actor: actor.to_string(),
        })
    }
}

/// Result of `create_or_get`: the row is active either way.
#[derive(Debug, Clone)]
pub enum CreateOutcome {
    Created(TaskRow),
    Existing(TaskRow),
}

/// Boot recovery counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub requeued: usize,
    pub failed: usize,
}

/// Store seam: the runner is tested against a fake implementation.
#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_or_get(&self, task: NewTask) -> Result<CreateOutcome, String>;
    async fn claim(
        &self,
        task_id: &str,
        owner: &str,
        policy: &TaskPolicy,
    ) -> Result<Option<TaskRow>, String>;
    async fn heartbeat(
        &self,
        task_id: &str,
        owner: &str,
        policy: &TaskPolicy,
        epoch: i64,
    ) -> Result<bool, String>;
    async fn checkpoint(
        &self,
        task_id: &str,
        owner: &str,
        epoch: i64,
        checkpoint: &TaskCheckpoint,
    ) -> Result<bool, String>;
    async fn finish(
        &self,
        task_id: &str,
        owner: &str,
        epoch: i64,
        terminal: TaskStatus,
        error: Option<&str>,
        checkpoint: &TaskCheckpoint,
    ) -> Result<bool, String>;
    async fn requeue(&self, task_id: &str, owner: &str, epoch: i64) -> Result<bool, String>;
    async fn recover_expired(&self, max_recoveries: u32) -> Result<RecoveryReport, String>;
    async fn prune(&self, retention_days: u64) -> Result<usize, String>;
    async fn get(&self, task_id: &str) -> Result<Option<TaskRow>, String>;
    async fn active(&self) -> Result<Vec<TaskRow>, String>;
}

/// SurrealDB-backed store: binds values, executes the pure SQL builders, and
/// decodes typed rows.
pub struct DbTaskStore {
    exec: Arc<dyn Executor>,
}

impl DbTaskStore {
    pub fn new(exec: Arc<dyn Executor>) -> Self {
        DbTaskStore { exec }
    }

    async fn query_rows(&self, sql: &str, vars: Vars) -> Result<Vec<TaskRow>, String> {
        let outcome = self
            .exec
            .smart_query(sql, vars)
            .await
            .map_err(|e| e.to_string())?;
        outcome.rows::<TaskRow>(sql).map_err(|e| e.to_string())
    }

    async fn find_active(&self, key: &str) -> Result<Option<TaskRow>, String> {
        let sql = "SELECT * FROM task WHERE active_key = $active_key LIMIT 1;";
        let mut vars = Vars::new();
        vars.insert("active_key".into(), Value::from_t(key));
        Ok(self.query_rows(sql, vars).await?.into_iter().next())
    }

    fn count_rows(outcome: &QueryOutcome) -> usize {
        match outcome.first() {
            Some(Value::Array(rows)) => rows.len(),
            _ => 0,
        }
    }
}

#[async_trait]
impl TaskStore for DbTaskStore {
    async fn create_or_get(&self, task: NewTask) -> Result<CreateOutcome, String> {
        if let Some(row) = self.find_active(&task.active_key).await? {
            return Ok(CreateOutcome::Existing(row));
        }
        let id = new_task_id();
        let sql = create_task_sql(&id).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("kind".into(), Value::from_t(task.kind.as_str()));
        vars.insert("project".into(), Value::from_t(task.project.as_str()));
        vars.insert("params".into(), Value::from_t(task.params.clone()));
        vars.insert("active_key".into(), Value::from_t(task.active_key.as_str()));
        vars.insert("actor".into(), Value::from_t(task.actor.as_str()));
        match self.exec.smart_query(&sql, vars).await {
            Ok(outcome) => {
                let mut rows = outcome.rows::<TaskRow>(&sql).map_err(|e| e.to_string())?;
                rows.pop()
                    .map(CreateOutcome::Created)
                    .ok_or_else(|| "task create returned no row (UNIQUE race?)".to_string())
            }
            Err(err) => {
                // The UNIQUE index resolves the race: return the winner if it
                // exists, otherwise surface the original error.
                if let Some(row) = self.find_active(&task.active_key).await? {
                    Ok(CreateOutcome::Existing(row))
                } else {
                    Err(err.to_string())
                }
            }
        }
    }

    async fn claim(
        &self,
        task_id: &str,
        owner: &str,
        policy: &TaskPolicy,
    ) -> Result<Option<TaskRow>, String> {
        let sql = claim_task_sql(task_id, policy).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("owner".into(), Value::from_t(owner));
        Ok(self.query_rows(&sql, vars).await?.into_iter().next())
    }

    async fn heartbeat(
        &self,
        task_id: &str,
        owner: &str,
        policy: &TaskPolicy,
        epoch: i64,
    ) -> Result<bool, String> {
        let sql = heartbeat_task_sql(task_id, policy).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("owner".into(), Value::from_t(owner));
        vars.insert("epoch".into(), Value::from_t(epoch));
        Ok(!self.query_rows(&sql, vars).await?.is_empty())
    }

    async fn checkpoint(
        &self,
        task_id: &str,
        owner: &str,
        epoch: i64,
        checkpoint: &TaskCheckpoint,
    ) -> Result<bool, String> {
        let sql = checkpoint_task_sql(task_id).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("owner".into(), Value::from_t(owner));
        vars.insert("epoch".into(), Value::from_t(epoch));
        vars.insert("checkpoint".into(), Value::from_t(checkpoint.clone()));
        Ok(!self.query_rows(&sql, vars).await?.is_empty())
    }

    async fn finish(
        &self,
        task_id: &str,
        owner: &str,
        epoch: i64,
        terminal: TaskStatus,
        error: Option<&str>,
        checkpoint: &TaskCheckpoint,
    ) -> Result<bool, String> {
        let sql = finish_task_sql(task_id, terminal).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("owner".into(), Value::from_t(owner));
        vars.insert("epoch".into(), Value::from_t(epoch));
        vars.insert(
            "error".into(),
            error.map(Value::from_t).unwrap_or(Value::None),
        );
        vars.insert("checkpoint".into(), Value::from_t(checkpoint.clone()));
        Ok(!self.query_rows(&sql, vars).await?.is_empty())
    }

    async fn requeue(&self, task_id: &str, owner: &str, epoch: i64) -> Result<bool, String> {
        let sql = requeue_task_sql(task_id).map_err(|e| e.to_string())?;
        let mut vars = Vars::new();
        vars.insert("owner".into(), Value::from_t(owner));
        vars.insert("epoch".into(), Value::from_t(epoch));
        Ok(!self.query_rows(&sql, vars).await?.is_empty())
    }

    async fn recover_expired(&self, max_recoveries: u32) -> Result<RecoveryReport, String> {
        let mut vars = Vars::new();
        vars.insert(
            "max_recoveries".into(),
            Value::from_t(max_recoveries as i64),
        );
        let requeued = self
            .exec
            .smart_query(recovery_requeue_sql(), vars.clone())
            .await
            .map_err(|e| e.to_string())?;
        let failed = self
            .exec
            .smart_query(recovery_fail_sql(), vars)
            .await
            .map_err(|e| e.to_string())?;
        Ok(RecoveryReport {
            requeued: Self::count_rows(&requeued),
            failed: Self::count_rows(&failed),
        })
    }

    async fn prune(&self, retention_days: u64) -> Result<usize, String> {
        let sql = prune_task_sql(retention_days);
        let outcome = self
            .exec
            .smart_query(&sql, Vars::new())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self::count_rows(&outcome))
    }

    async fn get(&self, task_id: &str) -> Result<Option<TaskRow>, String> {
        let id = task_record_id(task_id).map_err(|e| e.to_string())?;
        let sql = format!("SELECT * FROM {id};");
        Ok(self.query_rows(&sql, Vars::new()).await?.into_iter().next())
    }

    async fn active(&self) -> Result<Vec<TaskRow>, String> {
        let sql = "SELECT * FROM task WHERE status IN ['queued', 'running'] \
                   ORDER BY created_at ASC;";
        self.query_rows(sql, Vars::new()).await
    }
}

/// One pass executed under a task lease. `params` is the snapshot stored on
/// the task row (resume must use it, not the live config).
pub trait TaskPass: Send + Sync {
    fn run<'a>(
        &'a self,
        params: &'a TaskParams,
        checkpoint: Option<&'a dyn PassCheckpoint>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Production pass: the consolidation loop with the checkpoint seam.
struct OptimizerPass<'a> {
    optimizer: &'a Optimizer,
    project: &'a str,
}

impl TaskPass for OptimizerPass<'_> {
    fn run<'a>(
        &'a self,
        params: &'a TaskParams,
        checkpoint: Option<&'a dyn PassCheckpoint>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.optimizer
                .consolidate_loop(self.project, params.max_items as usize, checkpoint)
                .await
        })
    }
}

/// Terminal outcome of a runner attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRunOutcome {
    Completed {
        task_id: String,
    },
    Failed {
        task_id: String,
        error: String,
    },
    Requeued {
        task_id: String,
    },
    AlreadyRunning {
        task_id: String,
    },
    LeaseLost {
        task_id: String,
    },
    /// Store unavailable: the pass ran without a lease (§4.6).
    Degraded {
        reason: String,
    },
}

/// A claimed task ready to run, plus the fallback params if the row lost them.
pub struct TaskStart {
    pub row: TaskRow,
    pub created: bool,
    fallback_params: TaskParams,
}

/// Result of the create+claim phase.
pub enum TaskBegin {
    Claimed(Box<TaskStart>),
    AlreadyRunning {
        task_id: String,
        status: String,
        attempts: i64,
    },
    Degraded {
        params: TaskParams,
        reason: String,
    },
}

/// Optional sink for resource notifications (create/claim/finish).
pub trait TaskEvents: Send + Sync {
    fn task_changed(&self, task_id: &str);
}

/// `TaskEvents` adapter: fires `ibis://tasks` and `ibis://tasks/{id}` on the
/// shared subscription registry (state is weak to avoid an Arc cycle).
pub struct ResourceTaskEvents {
    state: std::sync::Weak<crate::state::AppState>,
}

impl ResourceTaskEvents {
    pub fn new(state: std::sync::Weak<crate::state::AppState>) -> Self {
        ResourceTaskEvents { state }
    }
}

impl TaskEvents for ResourceTaskEvents {
    fn task_changed(&self, task_id: &str) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let key = task_id
            .trim_start_matches("task:")
            .trim_matches(|c| c == '\u{27e8}' || c == '\u{27e9}')
            .to_string();
        let forms = [
            task_id.to_string(),
            key.clone(),
            format!("task:\u{27e8}{key}\u{27e9}"),
        ];
        tokio::spawn(async move {
            state.resource_subs.fire(crate::resources::TASKS_URI).await;
            for form in forms {
                state
                    .resource_subs
                    .fire(&format!("{}/{}", crate::resources::TASKS_URI, form))
                    .await;
            }
        });
    }
}

/// Lease/fencing/checkpoint owner for one process.
pub struct TaskRunner {
    store: Arc<dyn TaskStore>,
    policy: TaskPolicy,
    owner: String,
    shutdown: CancellationToken,
    events: Mutex<Option<Arc<dyn TaskEvents>>>,
}

impl TaskRunner {
    pub fn new(store: Arc<dyn TaskStore>, policy: TaskPolicy, shutdown: CancellationToken) -> Self {
        TaskRunner {
            store,
            policy,
            owner: format!("{}:{}", std::process::id(), uuid::Uuid::new_v4().simple()),
            shutdown,
            events: Mutex::new(None),
        }
    }

    /// Installs the resource-notification sink (after `AppState` exists).
    pub fn set_events(&self, events: Option<Arc<dyn TaskEvents>>) {
        *self.events.lock().unwrap() = events;
    }

    fn notify(&self, task_id: &str) {
        if let Some(events) = self.events.lock().unwrap().as_ref() {
            events.task_changed(task_id);
        }
    }

    pub fn policy(&self) -> TaskPolicy {
        self.policy
    }

    /// Active tasks (`queued`/`running`) for the daemon's queued-first drain.
    pub async fn active(&self) -> Result<Vec<TaskRow>, String> {
        self.store.active().await
    }

    /// One task row by record id (resources).
    pub async fn get(&self, task_id: &str) -> Result<Option<TaskRow>, String> {
        self.store.get(task_id).await
    }

    /// Boot recovery + retention prune. Failures are warnings, never fatal.
    pub async fn recover_and_prune(&self) {
        match self
            .store
            .recover_expired(self.policy.max_recoveries())
            .await
        {
            Ok(report) => tracing::info!(
                target: "task_recovered",
                requeued = report.requeued,
                failed = report.failed,
                "task_recovered"
            ),
            Err(err) => tracing::warn!("TaskStore: recovery failed: {err}"),
        }
        match self.store.prune(self.policy.retention_days()).await {
            Ok(pruned) => tracing::info!(target: "task_pruned", pruned, "task_pruned"),
            Err(err) => tracing::warn!("TaskStore: prune failed: {err}"),
        }
    }

    /// Runs one consolidation pass under the task envelope.
    pub async fn run_project(
        &self,
        optimizer: &Optimizer,
        project: &str,
        max_items: usize,
        max_llm_summaries: usize,
        actor: &str,
    ) -> Result<TaskRunOutcome, String> {
        let new = NewTask::memory_consolidate(project, max_items, max_llm_summaries, actor)
            .map_err(|e| e.to_string())?;
        let pass = OptimizerPass { optimizer, project };
        self.run_task(new, &pass).await
    }

    /// Generic runner used by tests with a fake pass/store.
    /// Creates or finds the active task and claims it. Returns before the
    /// pass runs so the tool can answer with the task id.
    pub async fn begin(&self, new: NewTask) -> Result<TaskBegin, String> {
        let fallback_params = new.params.clone();
        let created = match self.store.create_or_get(new).await {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!("TaskStore: create failed, running without lease: {err}");
                return Ok(TaskBegin::Degraded {
                    params: fallback_params,
                    reason: err,
                });
            }
        };
        let (row, created) = match created {
            CreateOutcome::Created(row) => {
                let task_id = record_id_to_plain(&row.id);
                tracing::info!(target: "task_created", task = %task_id, "task_created");
                self.notify(&task_id);
                (row, true)
            }
            CreateOutcome::Existing(row) => (row, false),
        };
        let task_id = record_id_to_plain(&row.id);
        match self.store.claim(&task_id, &self.owner, &self.policy).await {
            Ok(Some(claimed)) => {
                tracing::info!(
                    target: "task_claimed",
                    task = %task_id,
                    epoch = claimed.lease_epoch,
                    "task_claimed"
                );
                self.notify(&task_id);
                Ok(TaskBegin::Claimed(Box::new(TaskStart {
                    row: claimed,
                    created,
                    fallback_params,
                })))
            }
            Ok(None) => {
                tracing::info!(
                    target: "task_claim_skipped",
                    task = %task_id,
                    "task_claim_skipped"
                );
                Ok(TaskBegin::AlreadyRunning {
                    task_id,
                    status: row.status,
                    attempts: row.claims,
                })
            }
            Err(err) => {
                tracing::warn!("TaskStore: claim failed, running without lease: {err}");
                Ok(TaskBegin::Degraded {
                    params: fallback_params,
                    reason: err,
                })
            }
        }
    }

    /// Runs a claimed task with the production consolidation pass.
    pub async fn execute_project(
        &self,
        start: TaskStart,
        optimizer: &Optimizer,
        project: &str,
    ) -> Result<TaskRunOutcome, String> {
        let pass = OptimizerPass { optimizer, project };
        let span = crate::telemetry::active_memory_consolidate_span(project);
        let result = self
            .execute_start(start, &pass)
            .instrument(span.clone())
            .await;
        record_task_outcome(&span, &result);
        result
    }

    /// Runs the pass without a lease when the store failed after start.
    pub async fn run_degraded_project(
        &self,
        params: &TaskParams,
        optimizer: &Optimizer,
        project: &str,
    ) -> Result<TaskRunOutcome, String> {
        let pass = OptimizerPass { optimizer, project };
        self.run_degraded(&pass, params, "task store unavailable".into(), project)
            .await
    }

    pub async fn run_task(
        &self,
        new: NewTask,
        pass: &dyn TaskPass,
    ) -> Result<TaskRunOutcome, String> {
        let project = new.project.clone();
        match self.begin(new).await? {
            TaskBegin::Claimed(start) => {
                let span = crate::telemetry::active_memory_consolidate_span(&project);
                let result = self
                    .execute_start(*start, pass)
                    .instrument(span.clone())
                    .await;
                record_task_outcome(&span, &result);
                result
            }
            TaskBegin::AlreadyRunning { task_id, .. } => {
                Ok(TaskRunOutcome::AlreadyRunning { task_id })
            }
            TaskBegin::Degraded { params, reason } => {
                self.run_degraded(pass, &params, reason, &project).await
            }
        }
    }

    async fn execute_start(
        &self,
        start: TaskStart,
        pass: &dyn TaskPass,
    ) -> Result<TaskRunOutcome, String> {
        let claimed = start.row;
        let task_id = record_id_to_plain(&claimed.id);
        // F17: the runner's `memory.consolidate` span is the current span of
        // the instrumented `execute_start` future.
        let span = tracing::Span::current();
        crate::telemetry::record_task(&span, task_id.as_str(), claimed.recoveries.max(0) as u64);
        let epoch = claimed.lease_epoch;
        let params = claimed.params.clone().unwrap_or(start.fallback_params);
        let cap = params.max_llm_summaries as usize;
        let initial = claimed.checkpoint.clone().unwrap_or_default();
        let initial_summaries = initial.summaries_used;

        let lease_lost = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(initial));
        let span_stats = Arc::new(Mutex::new(crate::telemetry::PassStats::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<TaskCheckpoint>();

        let writer = {
            let store = self.store.clone();
            let task_id = task_id.clone();
            let owner = self.owner.clone();
            let lease_lost = lease_lost.clone();
            tokio::spawn(async move {
                while let Some(checkpoint) = rx.recv().await {
                    match store.checkpoint(&task_id, &owner, epoch, &checkpoint).await {
                        Ok(true) => {}
                        Ok(false) => {
                            lease_lost.store(true, Ordering::SeqCst);
                            tracing::warn!(
                                target: "task_heartbeat_lost",
                                task = %task_id,
                                "task checkpoint skipped: lease lost"
                            );
                            break;
                        }
                        Err(err) => {
                            tracing::warn!("TaskStore: checkpoint failed for {task_id}: {err}")
                        }
                    }
                }
            })
        };

        let heartbeat_stop = CancellationToken::new();
        let heartbeat = {
            let store = self.store.clone();
            let task_id = task_id.clone();
            let owner = self.owner.clone();
            let lease_lost = lease_lost.clone();
            let stop = heartbeat_stop.clone();
            let interval = Duration::from_secs(self.policy.heartbeat_secs());
            let policy = self.policy;
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = ticker.tick() => {
                            match store.heartbeat(&task_id, &owner, &policy, epoch).await {
                                Ok(true) => {}
                                Ok(false) => {
                                    lease_lost.store(true, Ordering::SeqCst);
                                    tracing::warn!(
                                        target: "task_heartbeat_lost",
                                        task = %task_id,
                                        "task_heartbeat_lost"
                                    );
                                    break;
                                }
                                Err(err) => tracing::warn!(
                                    "TaskStore: heartbeat failed for {task_id}: {err}"
                                ),
                            }
                        }
                    }
                }
            })
        };

        let checkpoint = RunnerCheckpoint {
            task_id: task_id.clone(),
            cap,
            base_summaries: initial_summaries,
            state: state.clone(),
            span_stats: span_stats.clone(),
            tx,
            lease_lost: lease_lost.clone(),
            shutdown: self.shutdown.clone(),
        };
        let result = pass.run(&params, Some(&checkpoint)).await;
        drop(checkpoint);
        crate::telemetry::record_pass_stats(&tracing::Span::current(), &span_stats.lock().unwrap());
        let _ = writer.await;
        heartbeat_stop.cancel();
        let _ = heartbeat.await;

        let final_checkpoint = state.lock().unwrap().clone();
        if self.shutdown.is_cancelled() || lease_lost.load(Ordering::SeqCst) {
            match self.store.requeue(&task_id, &self.owner, epoch).await {
                Ok(true) => {
                    tracing::info!(target: "task_requeued", task = %task_id, "task_requeued");
                    self.notify(&task_id);
                    return Ok(TaskRunOutcome::Requeued { task_id });
                }
                Ok(false) => return Ok(TaskRunOutcome::LeaseLost { task_id }),
                Err(err) => {
                    tracing::warn!("TaskStore: requeue failed for {task_id}: {err}");
                    return Ok(TaskRunOutcome::Requeued { task_id });
                }
            }
        }

        let (terminal, error) = match &result {
            Ok(()) => (TaskStatus::Completed, None),
            Err(err) => (TaskStatus::Failed, Some(bounded_error(err))),
        };
        let error_ref = error.as_deref();
        match self
            .store
            .finish(
                &task_id,
                &self.owner,
                epoch,
                terminal,
                error_ref,
                &final_checkpoint,
            )
            .await
        {
            Ok(true) => {
                self.notify(&task_id);
                match terminal {
                    TaskStatus::Completed => tracing::info!(
                        target: "task_completed",
                        task = %task_id,
                        status = terminal.as_str(),
                        "task_completed"
                    ),
                    TaskStatus::Failed => tracing::warn!(
                        target: "task_failed",
                        task = %task_id,
                        status = terminal.as_str(),
                        "task_failed"
                    ),
                    _ => {}
                }
                Ok(match terminal {
                    TaskStatus::Completed => TaskRunOutcome::Completed { task_id },
                    _ => TaskRunOutcome::Failed {
                        task_id,
                        error: error.unwrap_or_default(),
                    },
                })
            }
            Ok(false) => Ok(TaskRunOutcome::LeaseLost { task_id }),
            Err(err) => {
                tracing::warn!("TaskStore: finish failed for {task_id}: {err}");
                Ok(match terminal {
                    TaskStatus::Completed => TaskRunOutcome::Completed { task_id },
                    _ => TaskRunOutcome::Failed {
                        task_id,
                        error: error.unwrap_or_default(),
                    },
                })
            }
        }
    }

    async fn run_degraded(
        &self,
        pass: &dyn TaskPass,
        params: &TaskParams,
        reason: String,
        project: &str,
    ) -> Result<TaskRunOutcome, String> {
        let span = crate::telemetry::active_memory_consolidate_span(project);
        // F17: without a task row there is no `RunnerCheckpoint`; a stats-only
        // checkpoint still fills ibis.items/ibis.summaries/… on the span.
        let stats = StatsCheckpoint::new();
        let result = pass
            .run(params, Some(&stats))
            .instrument(span.clone())
            .await;
        crate::telemetry::record_pass_stats(&span, &stats.snapshot());
        match result {
            Ok(()) => {
                crate::telemetry::record_pass_outcome(
                    &span,
                    crate::telemetry::PassOutcome::Completed,
                );
                Ok(TaskRunOutcome::Degraded { reason })
            }
            Err(err) => {
                crate::telemetry::record_pass_outcome(&span, crate::telemetry::PassOutcome::Failed);
                Err(err)
            }
        }
    }
}

/// Maps the runner's terminal outcome onto the bounded `ibis.outcome` values.
fn record_task_outcome(span: &tracing::Span, result: &Result<TaskRunOutcome, String>) {
    use crate::telemetry::PassOutcome;
    let outcome = match result {
        Ok(TaskRunOutcome::Completed { .. }) | Ok(TaskRunOutcome::Degraded { .. }) => {
            PassOutcome::Completed
        }
        Ok(TaskRunOutcome::Requeued { .. }) => PassOutcome::Requeued,
        Ok(TaskRunOutcome::Failed { .. }) | Ok(TaskRunOutcome::LeaseLost { .. }) => {
            PassOutcome::Failed
        }
        // `AlreadyRunning` never reaches an instrumented pass.
        Ok(TaskRunOutcome::AlreadyRunning { .. }) => PassOutcome::Completed,
        Err(_) => PassOutcome::Failed,
    };
    crate::telemetry::record_pass_outcome(span, outcome);
}

/// Sync checkpoint seam backed by an unbounded writer channel: `item_done`
/// never blocks the pass, and the runner awaits the writer before finishing.
struct RunnerCheckpoint {
    task_id: String,
    cap: usize,
    /// Summaries already spent by previous attempts (the resume budget base).
    base_summaries: u64,
    state: Arc<Mutex<TaskCheckpoint>>,
    span_stats: Arc<Mutex<crate::telemetry::PassStats>>,
    tx: mpsc::UnboundedSender<TaskCheckpoint>,
    lease_lost: Arc<AtomicBool>,
    shutdown: CancellationToken,
}

impl RunnerCheckpoint {
    fn snapshot(&self) -> TaskCheckpoint {
        self.state.lock().unwrap().clone()
    }

    fn send(&self) {
        let _ = self.tx.send(self.snapshot());
    }
}

impl PassCheckpoint for RunnerCheckpoint {
    fn should_continue(&self) -> bool {
        !self.lease_lost.load(Ordering::SeqCst) && !self.shutdown.is_cancelled()
    }

    fn item_done(&self, item_id: &str, outcome: &str, items_total: usize) {
        {
            let mut stats = self.span_stats.lock().unwrap();
            stats.items = stats.items.saturating_add(1);
        }
        {
            let mut state = self.state.lock().unwrap();
            state.last_item_id = Some(item_id.to_string());
            state.items_done = state.items_done.saturating_add(1);
            state.items_total = (items_total as u64).max(state.items_done);
        }
        self.send();
        tracing::debug!(
            target: "task_checkpoint",
            task = %self.task_id,
            item = %item_id,
            outcome,
            "task_checkpoint"
        );
    }

    fn items_total(&self, total: usize) {
        {
            let mut state = self.state.lock().unwrap();
            state.items_total = (total as u64).max(state.items_done);
        }
        self.send();
    }

    fn scope_done(&self, report: &ibis_optimization::ObservationPassReport) {
        {
            // `observations_pass` hands the same cumulative report to every
            // scope: assign, never add (addition would triangle the totals).
            let mut stats = self.span_stats.lock().unwrap();
            stats.summaries = report.summarized as u64;
            stats.deferred = report.deferred as u64;
            stats.fallbacks = report.fallbacks as u64;
            stats.failed = report.failed as u64;
        }
        {
            let mut state = self.state.lock().unwrap();
            state.summaries_used = self.base_summaries.saturating_add(report.summarized as u64);
            state.scopes_done = state.scopes_done.saturating_add(1);
            state.scopes_total = (report.scopes as u64).max(state.scopes_done);
        }
        self.send();
    }

    fn remaining_summaries(&self) -> Option<usize> {
        let used = self.state.lock().unwrap().summaries_used as usize;
        Some(remaining_summaries(self.cap, used))
    }
}

fn bounded_error(error: &str) -> String {
    let bytes = error.as_bytes();
    if bytes.len() <= 2048 {
        return error.to_string();
    }
    String::from_utf8_lossy(&bytes[..2048]).into_owned()
}

/// F17: stats-only observer for the degraded/no-store passes. It fills the
/// same [`crate::telemetry::PassStats`] the leased runner reports without
/// changing pass semantics: `should_continue` is always true and
/// `remaining_summaries` stays `None`, so the pass keeps its per-config cap
/// exactly as with no checkpoint at all.
pub(crate) struct StatsCheckpoint {
    stats: Mutex<crate::telemetry::PassStats>,
}

impl StatsCheckpoint {
    pub(crate) fn new() -> Self {
        StatsCheckpoint {
            stats: Mutex::new(crate::telemetry::PassStats::default()),
        }
    }

    pub(crate) fn snapshot(&self) -> crate::telemetry::PassStats {
        *self.stats.lock().unwrap()
    }
}

impl PassCheckpoint for StatsCheckpoint {
    fn should_continue(&self) -> bool {
        true
    }

    fn item_done(&self, _item_id: &str, _outcome: &str, _items_total: usize) {
        let mut stats = self.stats.lock().unwrap();
        stats.items = stats.items.saturating_add(1);
    }

    fn scope_done(&self, report: &ibis_optimization::ObservationPassReport) {
        // Same cumulative-report rule as `RunnerCheckpoint::scope_done`:
        // assign, never add.
        let mut stats = self.stats.lock().unwrap();
        stats.summaries = report.summarized as u64;
        stats.deferred = report.deferred as u64;
        stats.fallbacks = report.fallbacks as u64;
        stats.failed = report.failed as u64;
    }

    fn remaining_summaries(&self) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use surrealdb::types::RecordIdKey;

    use ibis_optimization::ObservationPassReport;

    fn task_row(id: &str, status: TaskStatus, active_key: Option<&str>) -> TaskRow {
        let now = Datetime::from(chrono::Utc::now());
        TaskRow {
            id: RecordId {
                table: "task".into(),
                key: RecordIdKey::String(id.to_string()),
            },
            kind: TASK_KIND_MEMORY_CONSOLIDATE.into(),
            status: status.as_str().into(),
            project: Some("proj".into()),
            params: Some(TaskParams {
                max_items: 20,
                max_llm_summaries: 10,
                mode: TASK_KIND_MEMORY_CONSOLIDATE.into(),
            }),
            checkpoint: Some(TaskCheckpoint::default()),
            active_key: active_key.map(str::to_string),
            lease_owner: None,
            lease_epoch: 0,
            lease_expires_at: None,
            claims: 0,
            recoveries: 0,
            error: None,
            requested_by: Some("test".into()),
            created_at: now,
            updated_at: now,
            claimed_at: None,
            finished_at: None,
        }
    }

    #[derive(Default)]
    struct FakeTaskStore {
        rows: Mutex<HashMap<String, TaskRow>>,
        calls: Mutex<Vec<String>>,
        fail: AtomicBool,
    }

    impl FakeTaskStore {
        fn insert(&self, row: TaskRow) {
            let key = record_id_to_plain(&row.id);
            self.rows.lock().unwrap().insert(key, row);
        }

        fn find_active(&self, key: &str) -> Option<TaskRow> {
            self.rows
                .lock()
                .unwrap()
                .values()
                .find(|row| row.active_key.as_deref() == Some(key))
                .cloned()
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: &str) {
            self.calls.lock().unwrap().push(call.to_string());
        }
    }

    #[async_trait]
    impl TaskStore for FakeTaskStore {
        async fn create_or_get(&self, task: NewTask) -> Result<CreateOutcome, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            if let Some(row) = self.find_active(&task.active_key) {
                self.record("create_existing");
                return Ok(CreateOutcome::Existing(row));
            }
            let id = new_task_id();
            let mut row = task_row(&id, TaskStatus::Queued, Some(&task.active_key));
            row.project = Some(task.project.clone());
            row.params = Some(task.params.clone());
            row.requested_by = Some(task.actor.clone());
            self.insert(row.clone());
            self.record("create");
            Ok(CreateOutcome::Created(row))
        }

        async fn claim(
            &self,
            task_id: &str,
            owner: &str,
            policy: &TaskPolicy,
        ) -> Result<Option<TaskRow>, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            let now = chrono::Utc::now();
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(task_id) else {
                return Ok(None);
            };
            let live = row.status == "running"
                && row
                    .lease_expires_at
                    .map(|expires| expires.into_inner() > now)
                    .unwrap_or(false);
            if live {
                return Ok(None);
            }
            row.status = "running".into();
            row.lease_owner = Some(owner.to_string());
            row.lease_epoch += 1;
            row.lease_expires_at = Some(Datetime::from(
                now + chrono::Duration::seconds(policy.lease_secs() as i64),
            ));
            row.claims += 1;
            let claimed = row.clone();
            drop(rows);
            self.record("claim");
            Ok(Some(claimed))
        }

        async fn heartbeat(
            &self,
            task_id: &str,
            owner: &str,
            policy: &TaskPolicy,
            epoch: i64,
        ) -> Result<bool, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(task_id) else {
                return Ok(false);
            };
            if row.lease_owner.as_deref() != Some(owner)
                || row.lease_epoch != epoch
                || row.status != "running"
            {
                return Ok(false);
            }
            row.lease_expires_at = Some(Datetime::from(
                chrono::Utc::now() + chrono::Duration::seconds(policy.lease_secs() as i64),
            ));
            drop(rows);
            self.record("heartbeat");
            Ok(true)
        }

        async fn checkpoint(
            &self,
            task_id: &str,
            owner: &str,
            epoch: i64,
            checkpoint: &TaskCheckpoint,
        ) -> Result<bool, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(task_id) else {
                return Ok(false);
            };
            if row.lease_owner.as_deref() != Some(owner)
                || row.lease_epoch != epoch
                || row.status != "running"
            {
                return Ok(false);
            }
            row.checkpoint = Some(checkpoint.clone());
            drop(rows);
            self.record("checkpoint");
            Ok(true)
        }

        async fn finish(
            &self,
            task_id: &str,
            owner: &str,
            epoch: i64,
            terminal: TaskStatus,
            error: Option<&str>,
            checkpoint: &TaskCheckpoint,
        ) -> Result<bool, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(task_id) else {
                return Ok(false);
            };
            if row.lease_owner.as_deref() != Some(owner)
                || row.lease_epoch != epoch
                || row.status != "running"
            {
                return Ok(false);
            }
            row.status = terminal.as_str().into();
            row.active_key = None;
            row.error = error.map(str::to_string);
            row.checkpoint = Some(checkpoint.clone());
            row.finished_at = Some(Datetime::from(chrono::Utc::now()));
            drop(rows);
            self.record("finish");
            Ok(true)
        }

        async fn requeue(&self, task_id: &str, owner: &str, epoch: i64) -> Result<bool, String> {
            if self.fail.load(Ordering::SeqCst) {
                return Err("store down".into());
            }
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.get_mut(task_id) else {
                return Ok(false);
            };
            if row.lease_owner.as_deref() != Some(owner)
                || row.lease_epoch != epoch
                || row.status != "running"
            {
                return Ok(false);
            }
            row.status = "queued".into();
            row.lease_owner = None;
            row.lease_expires_at = None;
            drop(rows);
            self.record("requeue");
            Ok(true)
        }

        async fn recover_expired(&self, _max_recoveries: u32) -> Result<RecoveryReport, String> {
            Ok(RecoveryReport::default())
        }

        async fn prune(&self, _retention_days: u64) -> Result<usize, String> {
            Ok(0)
        }

        async fn get(&self, task_id: &str) -> Result<Option<TaskRow>, String> {
            Ok(self.rows.lock().unwrap().get(task_id).cloned())
        }

        async fn active(&self) -> Result<Vec<TaskRow>, String> {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .values()
                .filter(|row| row.status == "queued" || row.status == "running")
                .cloned()
                .collect())
        }
    }

    struct FakePass {
        result: Result<(), String>,
        calls: AtomicUsize,
        remaining: AtomicUsize,
    }

    #[derive(Default)]
    struct CountingEvents(AtomicUsize);

    impl TaskEvents for CountingEvents {
        fn task_changed(&self, _task_id: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl FakePass {
        fn ok() -> Self {
            FakePass {
                result: Ok(()),
                calls: AtomicUsize::new(0),
                remaining: AtomicUsize::new(usize::MAX),
            }
        }

        fn err(message: &str) -> Self {
            FakePass {
                result: Err(message.to_string()),
                calls: AtomicUsize::new(0),
                remaining: AtomicUsize::new(usize::MAX),
            }
        }
    }

    impl TaskPass for FakePass {
        fn run<'a>(
            &'a self,
            params: &'a TaskParams,
            checkpoint: Option<&'a dyn PassCheckpoint>,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = params.max_items;
                if let Some(cp) = checkpoint {
                    cp.items_total(2);
                    cp.item_done("item-1", "ok", 2);
                    cp.item_done("item-2", "noise", 2);
                    cp.scope_done(&ObservationPassReport {
                        scopes: 1,
                        summarized: 1,
                        ..Default::default()
                    });
                    self.remaining.store(
                        cp.remaining_summaries().unwrap_or(usize::MAX),
                        Ordering::SeqCst,
                    );
                }
                self.result.clone()
            })
        }
    }

    fn runner(store: Arc<FakeTaskStore>, shutdown: CancellationToken) -> TaskRunner {
        TaskRunner::new(store, TaskPolicy::new(30, 10, 3, 30), shutdown)
    }

    #[tokio::test]
    async fn creates_and_claims_one_task() {
        let store = Arc::new(FakeTaskStore::default());
        let runner = runner(store.clone(), CancellationToken::new());
        let pass = FakePass::ok();
        let outcome = runner
            .run_task(
                NewTask::memory_consolidate("Proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskRunOutcome::Completed { .. }));
        let calls = store.calls();
        assert!(calls.iter().any(|call| call == "create"));
        assert!(calls.iter().any(|call| call == "claim"));
        assert!(calls.iter().any(|call| call == "finish"));
        assert_eq!(
            calls.iter().filter(|call| *call == "checkpoint").count(),
            4,
            "items_total + two items + one scope checkpoint"
        );
    }

    #[tokio::test]
    async fn second_call_returns_already_running() {
        let store = Arc::new(FakeTaskStore::default());
        let mut row = task_row(
            "0123456789abcdef0123456789abcdef",
            TaskStatus::Running,
            Some("memory_consolidate:proj"),
        );
        row.lease_owner = Some("other".into());
        row.lease_epoch = 1;
        row.lease_expires_at = Some(Datetime::from(
            chrono::Utc::now() + chrono::Duration::seconds(60),
        ));
        store.insert(row);
        let runner = runner(store, CancellationToken::new());
        let pass = FakePass::ok();
        let outcome = runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskRunOutcome::AlreadyRunning { .. }));
        assert_eq!(pass.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pass_failure_marks_failed_with_bounded_error() {
        let store = Arc::new(FakeTaskStore::default());
        let runner = runner(store.clone(), CancellationToken::new());
        let pass = FakePass::err(&"x".repeat(3000));
        let outcome = runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        let TaskRunOutcome::Failed { error, .. } = outcome else {
            panic!("expected failed outcome");
        };
        assert!(error.len() <= 2048);
        let row = store
            .rows
            .lock()
            .unwrap()
            .values()
            .next()
            .cloned()
            .expect("row");
        assert_eq!(row.status, "failed");
        assert_eq!(row.active_key, None);
    }

    #[tokio::test]
    async fn shutdown_requeues_without_counting_a_recovery() {
        let store = Arc::new(FakeTaskStore::default());
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let runner = runner(store.clone(), shutdown);
        let pass = FakePass::ok();
        let outcome = runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskRunOutcome::Requeued { .. }));
        let row = store
            .rows
            .lock()
            .unwrap()
            .values()
            .next()
            .cloned()
            .expect("row");
        assert_eq!(row.status, "queued");
        assert_eq!(row.recoveries, 0);
        assert_eq!(row.active_key.as_deref(), Some("memory_consolidate:proj"));
    }

    #[tokio::test]
    async fn remaining_summaries_uses_the_persisted_budget() {
        let store = Arc::new(FakeTaskStore::default());
        let mut row = task_row(
            "0123456789abcdef0123456789abcdef",
            TaskStatus::Queued,
            Some("memory_consolidate:proj"),
        );
        row.checkpoint = Some(TaskCheckpoint {
            summaries_used: 10,
            ..Default::default()
        });
        store.insert(row);
        let runner = runner(store, CancellationToken::new());
        let pass = FakePass::ok();
        runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert_eq!(pass.remaining.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn store_error_is_fail_open() {
        let store = Arc::new(FakeTaskStore::default());
        store.fail.store(true, Ordering::SeqCst);
        let runner = runner(store, CancellationToken::new());
        let pass = FakePass::ok();
        let outcome = runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskRunOutcome::Degraded { .. }));
        assert_eq!(pass.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn events_fire_on_create_claim_and_finish() {
        let store = Arc::new(FakeTaskStore::default());
        let runner = runner(store, CancellationToken::new());
        let events = Arc::new(CountingEvents::default());
        runner.set_events(Some(events.clone()));
        let pass = FakePass::ok();
        runner
            .run_task(
                NewTask::memory_consolidate("proj", 20, 10, "test").unwrap(),
                &pass,
            )
            .await
            .unwrap();
        assert!(
            events.0.load(Ordering::SeqCst) >= 3,
            "create + claim + finish notify"
        );
    }

    #[test]
    fn lease_lost_stops_the_checkpoint() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let checkpoint = RunnerCheckpoint {
            task_id: "task".into(),
            cap: 10,
            base_summaries: 0,
            state: Arc::new(Mutex::new(TaskCheckpoint::default())),
            span_stats: Arc::new(Mutex::new(crate::telemetry::PassStats::default())),
            tx,
            lease_lost: Arc::new(AtomicBool::new(true)),
            shutdown: CancellationToken::new(),
        };
        assert!(!checkpoint.should_continue());
    }

    /// `observations_pass` reports cumulative totals to every scope; the
    /// span stats must mirror the latest report, never sum it per scope.
    #[test]
    fn scope_done_assigns_cumulative_report() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let stats = Arc::new(Mutex::new(crate::telemetry::PassStats::default()));
        let checkpoint = RunnerCheckpoint {
            task_id: "task".into(),
            cap: 10,
            base_summaries: 0,
            state: Arc::new(Mutex::new(TaskCheckpoint::default())),
            span_stats: stats.clone(),
            tx,
            lease_lost: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
        };
        checkpoint.scope_done(&ObservationPassReport {
            scopes: 2,
            summarized: 1,
            deferred: 2,
            fallbacks: 3,
            failed: 4,
            ..Default::default()
        });
        checkpoint.scope_done(&ObservationPassReport {
            scopes: 2,
            summarized: 5,
            deferred: 6,
            fallbacks: 7,
            failed: 8,
            ..Default::default()
        });
        let stats = *stats.lock().unwrap();
        assert_eq!(stats.summaries, 5);
        assert_eq!(stats.deferred, 6);
        assert_eq!(stats.fallbacks, 7);
        assert_eq!(stats.failed, 8);
    }

    /// F17: the stats-only checkpoint collected on the degraded/no-store
    /// passes must mirror the leased runner's numbers without changing pass
    /// semantics (always continue, no budget override).
    #[test]
    fn stats_checkpoint_collects_without_changing_semantics() {
        let checkpoint = StatsCheckpoint::new();
        assert!(checkpoint.should_continue());
        assert_eq!(checkpoint.remaining_summaries(), None);

        checkpoint.items_total(5);
        checkpoint.item_done("item-1", "ok", 5);
        checkpoint.item_done("item-2", "error", 5);
        checkpoint.scope_done(&ObservationPassReport {
            scopes: 2,
            summarized: 3,
            deferred: 1,
            fallbacks: 4,
            failed: 1,
            ..Default::default()
        });
        // Cumulative report: assigned, not summed.
        checkpoint.scope_done(&ObservationPassReport {
            scopes: 2,
            summarized: 3,
            deferred: 2,
            fallbacks: 4,
            failed: 1,
            ..Default::default()
        });

        let stats = checkpoint.snapshot();
        assert_eq!(stats.items, 2);
        assert_eq!(stats.summaries, 3);
        assert_eq!(stats.deferred, 2);
        assert_eq!(stats.fallbacks, 4);
        assert_eq!(stats.failed, 1);
    }
}
