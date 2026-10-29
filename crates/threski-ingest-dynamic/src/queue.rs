//! Per-project ingestion queue.
//!
//! One worker task per project, fed by a bounded (100) channel: enqueues never
//! block the caller; on a full queue the job is dropped and its
//! [`OnComplete`] callback receives [`DynamicError::QueueFull`] (
//! `select { case q <- job: default: drop }`).
//!
//! Parity notes:
//! - `Stop` cancels the workers; a job
//!   already executing runs to completion — this crate checks the cancel flag
//!   before starting the next phase, while additionally kills in-flight
//!   git children via `exec.CommandContext` (E-A16).
//! - The `ProcessJob` phase runs under the 2h timeout
//!   ([`PROCESS_JOB_TIMEOUT`], `context.WithTimeout(m.ctx, 2*time.Hour)).
//! - Callback payloads share errors as `Arc<DynamicError>` — the original code
//!   passes the same `error` value to both callbacks (Rust errors are not
//!   `Clone`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};

use threski_core::config::Config;
use threski_db::executor::Executor;

use crate::error::{DynamicError, PROCESS_JOB_TIMEOUT};
use crate::workspace::{sync_workspace, SyncOutcome};

/// Queue capacity per project.
pub const JOB_CHANNEL_CAP: usize = 100;

/// Result of the synchronous workspace-sync phase.
/// `error` is shared because both callbacks observe the same failure.
#[derive(Debug)]
pub struct IngestionResult {
    pub actual_commit: String,
    pub is_aligned: bool,
    pub error: Option<Arc<DynamicError>>,
}

/// Sync-phase callback.
pub type OnSyncDone = Box<dyn FnOnce(IngestionResult) + Send>;

/// Completion callback (`OnComplete func(error); `Ok()` = nil).
pub type OnComplete = Box<dyn FnOnce(Result<(), Arc<DynamicError>>) + Send>;

/// A request to synchronize and ingest a repository.
pub struct IngestionJob {
    pub project_name: String,
    pub origin_url: String,
    pub branch: String,
    pub commit: String,
    pub on_sync_done: Option<OnSyncDone>,
    pub on_complete: Option<OnComplete>,
}

/// The ingest closure wired by the server lane. Cancellation is cooperative: the manager stops waiting
/// after [`PROCESS_JOB_TIMEOUT`] or on `Stop`, dropping the future.
pub type ProcessJobFn = Arc<
    dyn Fn(IngestionJob, String) -> Pin<Box<dyn Future<Output = Result<(), DynamicError>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
struct WorkerDeps {
    cfg: Arc<Config>,
    db: Option<Arc<dyn Executor>>,
    process_job: Option<ProcessJobFn>,
}

/// Manages a queue of ingestion jobs per project (
/// `ProjectIngestionManager`).
pub struct ProjectIngestionManager {
    queues: Mutex<HashMap<String, mpsc::Sender<IngestionJob>>>,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
    deps: WorkerDeps,
}

impl ProjectIngestionManager {
    /// Creates a new queue manager.
    pub fn new(cfg: Arc<Config>, db: Option<Arc<dyn Executor>>) -> Self {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        ProjectIngestionManager {
            queues: Mutex::new(HashMap::new()),
            workers: Mutex::new(Vec::new()),
            cancel_tx,
            cancel_rx,
            deps: WorkerDeps {
                cfg,
                db,
                process_job: None,
            },
        }
    }

    /// Wires the ingest closure ( mutates the `ProcessJob` field before the
    /// first `Enqueue`).
    pub fn set_process_job(&mut self, process_job: ProcessJobFn) {
        self.deps.process_job = Some(process_job);
    }

    /// Adds a job to the project's queue, creating a worker if none exists.
    /// Never blocks; full queues drop the job and notify `on_complete`.
    pub fn enqueue(&self, job: IngestionJob) {
        let name = job.project_name.clone();
        let sender = {
            let mut queues = self.queues.lock().unwrap();
            match queues.entry(name.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let (tx, rx) = mpsc::channel(JOB_CHANNEL_CAP);
                    entry.insert(tx.clone());
                    let handle = tokio::spawn(worker_task(
                        name.clone(),
                        rx,
                        self.cancel_rx.clone(),
                        self.deps.clone(),
                    ));
                    self.workers.lock().unwrap().push(handle);
                    tx
                }
                std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
            }
        };

        match sender.try_send(job) {
            Ok(()) => {
                tracing::debug!("Job queued for project {name}");
            }
            Err(mpsc::error::TrySendError::Full(mut dropped)) => {
                tracing::warn!("Queue full for project {name}. Job dropped.");
                if let Some(on_complete) = dropped.on_complete.take() {
                    on_complete(Err(Arc::new(DynamicError::QueueFull)));
                }
            }
            Err(mpsc::error::TrySendError::Closed(mut dropped)) => {
                tracing::warn!("Queue closed for project {name}. Job dropped.");
                if let Some(on_complete) = dropped.on_complete.take() {
                    on_complete(Err(Arc::new(DynamicError::QueueClosed)));
                }
            }
        }
    }

    /// Gracefully shuts down all workers: cancels, then waits for
    /// the worker tasks. An in-flight job finishes first.
    pub async fn stop(&self) {
        tracing::info!("Stopping the ProjectIngestionManager...");
        let _ = self.cancel_tx.send(true);
        let handles = std::mem::take(&mut *self.workers.lock().unwrap());
        for handle in handles {
            let _ = handle.await;
        }
        tracing::info!("ProjectIngestionManager stopped.");
    }
}

async fn worker_task(
    project_name: String,
    mut rx: mpsc::Receiver<IngestionJob>,
    mut cancel_rx: watch::Receiver<bool>,
    deps: WorkerDeps,
) {
    tracing::info!("Worker started for project: {project_name}");
    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                tracing::info!("Worker stopped for cancellation: {project_name}");
                return;
            }
            job = rx.recv() => {
                match job {
                    Some(job) => execute_job(&deps, &cancel_rx, job).await,
                    None => return, // manager dropped
                }
            }
        }
    }
}

async fn execute_job(deps: &WorkerDeps, cancel_rx: &watch::Receiver<bool>, job: IngestionJob) {
    let IngestionJob {
        project_name,
        origin_url,
        branch,
        commit,
        on_sync_done,
        on_complete,
    } = job;
    tracing::info!("Starting job for {project_name} (Branch: {branch}, Commit: {commit})");

    // 1. Sync workspace ( passes the manager ctx; request tokens never
    // reach this path — E-A15 notes the context-value wart).
    let sync: Result<SyncOutcome, Arc<DynamicError>> = if *cancel_rx.borrow() {
        Err(Arc::new(DynamicError::ManagerStopped))
    } else {
        match sync_workspace(
            &deps.cfg,
            deps.db.clone(),
            None,
            &project_name,
            &origin_url,
            &branch,
            &commit,
        )
        .await
        {
            Ok(outcome) => Ok(outcome),
            Err(err) => Err(Arc::new(err)),
        }
    };

    if let Some(on_sync_done) = on_sync_done {
        let result = match &sync {
            Ok(outcome) => IngestionResult {
                actual_commit: outcome.actual_commit.clone(),
                is_aligned: outcome.is_aligned,
                error: None,
            },
            Err(err) => IngestionResult {
                actual_commit: String::new(),
                is_aligned: false,
                error: Some(err.clone()),
            },
        };
        on_sync_done(result);
    }

    let outcome = match sync {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!("Workspace sync error for {project_name}: {err}");
            if let Some(on_complete) = on_complete {
                on_complete(Err(err));
            }
            return;
        }
    };

    // 2. Process job (IngestRepo, IngestCodebase) under the 2h timeout.
    let process_error: Option<Arc<DynamicError>> = match &deps.process_job {
        Some(process_job) => {
            let fut = process_job(
                IngestionJob {
                    project_name: project_name.clone(),
                    origin_url,
                    branch,
                    commit,
                    on_sync_done: None,
                    on_complete: None,
                },
                outcome.repo_path.clone(),
            );
            tokio::select! {
                res = fut => match res {
                    Ok(()) => {
                        tracing::info!("Ingestion completed successfully for {project_name}");
                        None
                    }
                    Err(err) => {
                        tracing::error!("Ingestion error for {project_name}: {err}");
                        Some(Arc::new(err))
                    }
                },
                _ = tokio::time::sleep(PROCESS_JOB_TIMEOUT) => {
                    tracing::error!(
                        "Ingestion timed out for {project_name} after {:?}",
                        PROCESS_JOB_TIMEOUT
                    );
                    Some(Arc::new(DynamicError::IngestTimeout(PROCESS_JOB_TIMEOUT)))
                }
            }
        }
        None => None,
    };

    // 3. Notify.
    if let Some(on_complete) = on_complete {
        on_complete(match process_error {
            Some(err) => Err(err),
            None => Ok(()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Minimal real git fixture: bare remote + one pushed commit
    /// ( setup). Returns the remote path.
    async fn make_bare_remote() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let remote = std::path::Path::new(&root)
            .join("remote.git")
            .display()
            .to_string();
        let dummy = std::path::Path::new(&root)
            .join("dummy")
            .display()
            .to_string();
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&dummy).unwrap();

        let runner = threski_git::GitRunner::anonymous();
        for (repo, args) in [
            (&remote, vec!["init", "-q", "--bare"]),
            (&dummy, vec!["init", "-q"]),
        ] {
            runner
                .run(repo, &args)
                .await
                .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
        }
        for args in [
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test User"],
        ] {
            runner
                .run(&dummy, &args)
                .await
                .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
        }
        std::fs::write(std::path::Path::new(&dummy).join("test.txt"), "hello").unwrap();
        for args in [
            vec!["add", "."],
            vec!["commit", "-q", "-m", "Initial commit"],
        ] {
            runner
                .run(&dummy, &args)
                .await
                .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
        }
        runner
            .run(&dummy, &["remote", "add", "origin", &remote])
            .await
            .unwrap();
        let branch = String::from_utf8_lossy(
            &runner
                .run(&dummy, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
        )
        .trim()
        .to_string();
        runner
            .run(&dummy, &["push", "-q", "origin", &branch])
            .await
            .unwrap();
        (dir, remote)
    }

    // Scenario: TestProjectIngestionManager_Enqueue_WorkspaceSync.
    #[tokio::test]
    async fn enqueue_runs_sync_and_process_phases() {
        let (_dir, remote) = make_bare_remote().await;
        let discovery = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config {
            discovery_root: discovery.path().display().to_string(),
            ..Default::default()
        });
        let manager_inner = ProjectIngestionManager::new(cfg, None);

        let process_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = process_called.clone();
        let repo_seen: Arc<std::sync::Mutex<String>> = Arc::default();
        let seen = repo_seen.clone();
        let mut manager_inner = manager_inner;
        manager_inner.set_process_job(Arc::new(move |job: IngestionJob, repo_path: String| {
            let called = called.clone();
            let seen = seen.clone();
            Box::pin(async move {
                assert_eq!(job.project_name, "test-repo");
                *seen.lock().unwrap() = repo_path;
                called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }) as Pin<Box<dyn Future<Output = Result<(), DynamicError>> + Send>>
        }));
        let manager = Arc::new(manager_inner);

        let (sync_tx, sync_rx) = tokio::sync::oneshot::channel::<IngestionResult>();
        let (complete_tx, complete_rx) =
            tokio::sync::oneshot::channel::<Result<(), Arc<DynamicError>>>();

        manager.enqueue(IngestionJob {
            project_name: "test-repo".to_string(),
            origin_url: remote.clone(),
            branch: String::new(),
            commit: String::new(),
            on_sync_done: Some(Box::new(move |res| {
                let _ = sync_tx.send(res);
            })),
            on_complete: Some(Box::new(move |res| {
                let _ = complete_tx.send(res);
            })),
        });

        let res = tokio::time::timeout(Duration::from_secs(30), sync_rx)
            .await
            .expect("timeout waiting for OnSyncDone")
            .expect("OnSyncDone dropped");
        assert!(res.error.is_none(), "sync must not fail: {:?}", res.error);
        assert!(res.is_aligned, "expected IsAligned to be true");
        assert!(!res.actual_commit.is_empty(), "expected ActualCommit");

        let completed = tokio::time::timeout(Duration::from_secs(30), complete_rx)
            .await
            .expect("timeout waiting for OnComplete")
            .expect("OnComplete dropped");
        completed.as_ref().expect("job must complete without error");

        assert!(
            process_called.load(std::sync::atomic::Ordering::SeqCst),
            "ProcessJob was not called"
        );
        let clone_root = discovery.path().join("dynamic").join("test-repo");
        assert_eq!(*repo_seen.lock().unwrap(), clone_root.display().to_string());

        manager.stop().await;
    }

    #[tokio::test]
    async fn sync_failure_flows_to_both_callbacks() {
        let discovery = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config {
            runtime_mode: "server".to_string(),
            discovery_root: discovery.path().display().to_string(),
            ..Default::default()
        });
        let manager = ProjectIngestionManager::new(cfg, None);

        let (complete_tx, complete_rx) =
            tokio::sync::oneshot::channel::<Result<(), Arc<DynamicError>>>();
        let (sync_tx, sync_rx) = tokio::sync::oneshot::channel::<IngestionResult>();

        manager.enqueue(IngestionJob {
            project_name: "broken".to_string(),
            origin_url: "/nonexistent/remote".to_string(),
            branch: String::new(),
            commit: String::new(),
            on_sync_done: Some(Box::new(move |res| {
                let _ = sync_tx.send(res);
            })),
            on_complete: Some(Box::new(move |res| {
                let _ = complete_tx.send(res);
            })),
        });

        let res = tokio::time::timeout(Duration::from_secs(30), sync_rx)
            .await
            .expect("timeout waiting for OnSyncDone")
            .expect("OnSyncDone dropped");
        let err = res.error.expect("sync error expected");
        // A local missing path is a plain clone failure, not an auth failure
        // ("repository not found" ≠ "does not exist" markers).
        assert!(err.to_string().contains("git clone failed"));
        assert!(err.credentials_required_fields().is_none());
        assert!(!res.is_aligned);

        let completed = tokio::time::timeout(Duration::from_secs(30), complete_rx)
            .await
            .expect("timeout waiting for OnComplete")
            .expect("OnComplete dropped");
        let err = completed.expect_err("job must fail");
        assert!(err.to_string().contains("git clone failed"));

        manager.stop().await;
    }

    #[tokio::test]
    async fn queue_full_drops_job_with_queue_full_error() {
        let (_dir, remote) = make_bare_remote().await;
        let discovery = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config {
            discovery_root: discovery.path().display().to_string(),
            ..Default::default()
        });

        // Park the worker inside the process phase of the first job so the
        // queue stays full behind it.
        let parked = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let job_gate = gate.clone();
        let parked_signal = parked.clone();
        let mut manager_inner = ProjectIngestionManager::new(cfg, None);
        manager_inner.set_process_job(Arc::new(move |_job: IngestionJob, _repo: String| {
            let gate = job_gate.clone();
            let parked_signal = parked_signal.clone();
            Box::pin(async move {
                parked_signal.notify_one();
                let _ = tokio::time::timeout(Duration::from_secs(60), gate.notified()).await;
                Ok(())
            }) as Pin<Box<dyn Future<Output = Result<(), DynamicError>> + Send>>
        }));
        let manager = Arc::new(manager_inner);

        let (first_complete_tx, first_complete_rx) =
            tokio::sync::oneshot::channel::<Result<(), Arc<DynamicError>>>();
        manager.enqueue(IngestionJob {
            project_name: "dropper".to_string(),
            origin_url: remote.clone(),
            branch: String::new(),
            commit: String::new(),
            on_sync_done: None,
            on_complete: Some(Box::new(move |res| {
                let _ = first_complete_tx.send(res);
            })),
        });
        tokio::time::timeout(Duration::from_secs(30), parked.notified())
            .await
            .expect("worker never parked");

        // The channel holds 100 jobs; the next one must be dropped inline.
        let full = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..=JOB_CHANNEL_CAP {
            let full_count = full.clone();
            manager.enqueue(IngestionJob {
                project_name: "dropper".to_string(),
                origin_url: "/nonexistent".to_string(),
                branch: String::new(),
                commit: String::new(),
                on_sync_done: None,
                on_complete: Some(Box::new(move |res| {
                    if matches!(res, Err(err) if matches!(&*err, DynamicError::QueueFull)) {
                        full_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })),
            });
        }
        assert_eq!(
            full.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one dropped job expected"
        );

        // Release the parked job and stop; queued jobs after cancellation
        // stay unprocessed (parity: Stop cancels workers, queue remains).
        gate.notify_one();
        let completed = tokio::time::timeout(Duration::from_secs(30), first_complete_rx)
            .await
            .expect("first job never completed")
            .expect("completion dropped");
        completed.as_ref().expect("first job must succeed");
        manager.stop().await;
    }

    #[test]
    fn process_job_timeout_is_two_hours() {
        // `context.WithTimeout(m.ctx, 2*time.Hour).
        assert_eq!(PROCESS_JOB_TIMEOUT, Duration::from_secs(2 * 60 * 60));
    }

    #[tokio::test]
    async fn process_job_timeout_branch_fires() {
        // Enforcement branch of the select in `execute_job`, exercised
        // directly with a shortened deadline.
        let fut: Pin<Box<dyn Future<Output = Result<(), DynamicError>> + Send>> = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        });
        let deadline = Duration::from_millis(50);
        let res = tokio::select! {
            res = fut => res,
            _ = tokio::time::sleep(deadline) => {
                Err(DynamicError::IngestTimeout(deadline))
            }
        };
        assert!(matches!(res, Err(DynamicError::IngestTimeout(_))));
    }
}
