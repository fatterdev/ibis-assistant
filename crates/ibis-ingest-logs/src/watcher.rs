//! Recursive log-folder watcher: fsnotify replaced by the
//! `notify` crate (D4/D12). The watcher owns one tailer task per watchable
//! file, keyed by canonical path; each task runs against its own shutdown
//! `watch` channel and is joined on stop. spawned detached goroutines and
//! never waited for them — Rust tasks join on stop
//! (finding G-A09).
//!
//! Watch semantics preserved: `.log`/`.txt` files not starting with `report_`
//! are watchable; created directories are walked and added; the initial walk
//! seeds tailers for existing files. The root is watched recursively (native
//! on Windows; the directory-create re-walk covers backends that do not
//! auto-add new subtrees).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::tailer::Tailer;

/// Grace period for tailer tasks to flush and exit on stop.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared registry between the watcher and its event loop.
struct WatcherState {
    tailers: Mutex<HashMap<String, TailerHandle>>,
    cfg: Option<Arc<ibis_core::config::Config>>,
    db: Option<Arc<dyn ibis_db::executor::Executor>>,
    ai: Option<Arc<ibis_ai::Client>>,
}

struct TailerHandle {
    shutdown_tx: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

pub struct Watcher {
    pub cfg: Option<Arc<ibis_core::config::Config>>,
    pub db: Option<Arc<dyn ibis_db::executor::Executor>>,
    pub ai: Option<Arc<ibis_ai::Client>>,
    state: Arc<WatcherState>,
    event_task: Option<tokio::task::JoinHandle<()>>,
    /// Keeps the notify watcher alive (watching stops when dropped).
    _notifier: Option<notify::RecommendedWatcher>,
}

/// True for `.log`/`.txt` files that are not generated reports
///
pub fn is_watchable_log(name: &str) -> bool {
    if name.starts_with("report_") {
        return false;
    }
    name.ends_with(".txt") || name.ends_with(".log")
}

impl Watcher {
    pub fn new(
        cfg: Option<Arc<ibis_core::config::Config>>,
        db: Option<Arc<dyn ibis_db::executor::Executor>>,
        ai: Option<Arc<ibis_ai::Client>>,
    ) -> Self {
        Watcher {
            cfg: cfg.clone(),
            db: db.clone(),
            ai: ai.clone(),
            state: Arc::new(WatcherState {
                tailers: Mutex::new(HashMap::new()),
                cfg,
                db,
                ai,
            }),
            event_task: None,
            _notifier: None,
        }
    }

    /// Starts recursive monitoring of `logs_root`.
    pub fn start(&mut self) -> Result<(), crate::error::LogsError> {
        let logs_root = self
            .cfg
            .as_ref()
            .map(|c| c.logs_root.clone())
            .unwrap_or_else(|| "./logs".to_string());
        std::fs::create_dir_all(&logs_root)?;

        // Event pump: notify's callback thread pushes into an unbounded tokio
        // channel (`send` is sync-safe).
        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = event_tx.send(res);
        })
        .map_err(|e| crate::error::LogsError::other(format!("notify watcher init failed: {e}")))?;

        notify::Watcher::watch(
            &mut watcher,
            Path::new(&logs_root),
            notify::RecursiveMode::Recursive,
        )
        .map_err(|e| crate::error::LogsError::other(format!("watch {logs_root} failed: {e}")))?;

        // Initial walk: add tailers for existing watchable files.
        let state = self.state.clone();
        walk_and_watch(Path::new(&logs_root), &state);

        let event_state = self.state.clone();
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let Ok(event) = event else {
                    tracing::error!("Watcher error: {:?}", event.unwrap_err());
                    continue;
                };
                match event.kind {
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {}
                    _ => continue,
                }
                for path in event.paths {
                    let Ok(meta) = tokio::fs::metadata(&path).await else {
                        continue;
                    };
                    if meta.is_dir() {
                        if matches!(event.kind, notify::EventKind::Create(_)) {
                            walk_and_watch(&path, &event_state);
                        }
                        continue;
                    }
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if is_watchable_log(&name) {
                        add_tailer(&event_state, &path.display().to_string());
                    }
                }
            }
        });

        self.event_task = Some(event_task);
        self._notifier = Some(watcher);

        tracing::info!("Live monitoring log folder (recursive): {logs_root}");
        Ok(())
    }

    /// Cancels the event loop and every tailer task, joining with a timeout
    ///
    pub async fn stop(&mut self) {
        let handles: Vec<TailerHandle> = {
            let mut map = self.state.tailers.lock().unwrap();
            map.drain().map(|(_, h)| h).collect()
        };
        for handle in handles {
            let _ = handle.shutdown_tx.send(true);
            let _ = tokio::time::timeout(STOP_JOIN_TIMEOUT, handle.join).await;
        }
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
        self._notifier = None;
    }
}

/// Walks a directory tree adding tailers for watchable files
/// walk errors are logged and skipped (parity).
fn walk_and_watch(dir: &Path, state: &Arc<WatcherState>) {
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_file() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if is_watchable_log(&name) {
                        add_tailer(state, &entry.path().display().to_string());
                    }
                }
            }
            Err(err) => {
                tracing::warn!("Log watch walk error: {err}");
            }
        }
    }
}

/// Registers a tailer task for `path` unless one already exists
///
fn add_tailer(state: &Arc<WatcherState>, path: &str) {
    let abs = std::fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string());
    if state.tailers.lock().unwrap().contains_key(&abs) {
        return;
    }

    let state = state.clone();
    let path_owned = path.to_string();
    tokio::spawn(async move {
        // Re-check after the async constructor window (two events may race).
        {
            let abs = std::fs::canonicalize(&path_owned)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path_owned.clone());
            if state.tailers.lock().unwrap().contains_key(&abs) {
                return;
            }
        }
        let tailer = Tailer::new(
            &path_owned,
            "",
            state.cfg.clone(),
            state.db.clone(),
            state.ai.clone(),
        )
        .await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let abs = std::fs::canonicalize(&path_owned)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path_owned.clone());
        let mut tailers = state.tailers.lock().unwrap();
        if tailers.contains_key(&abs) {
            return; // Lost the race; the loser drops out silently.
        }
        let tailer = Arc::new(tailer);
        let join = tokio::spawn(async move { tailer.run(shutdown_rx).await });
        tailers.insert(abs, TailerHandle { shutdown_tx, join });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_watchable_log_pins_report_exclusion() {
        assert!(is_watchable_log("app.log"));
        assert!(is_watchable_log("trace.txt"));
        assert!(!is_watchable_log("report_app_20260918120000.md"));
        assert!(!is_watchable_log("notes.md"));
        assert!(!is_watchable_log("data.bin"));
    }

    #[tokio::test]
    async fn watcher_start_stop_joins_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let logs_root = tmp.path().join("logs");
        std::fs::create_dir_all(&logs_root).unwrap();
        std::fs::write(logs_root.join("seed.log"), "ERROR seed\n").unwrap();

        let cfg = Arc::new(ibis_core::config::Config {
            logs_root: logs_root.display().to_string(),
            ..Default::default()
        });

        let mut watcher = Watcher::new(Some(cfg), None, None);
        watcher.start().expect("watcher start");

        // Allow the initial walk to register the tailer.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !watcher.state.tailers.lock().unwrap().is_empty(),
            "existing watchable files must get tailers"
        );

        watcher.stop().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Registry drained by stop.
        assert!(watcher.state.tailers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn watcher_picks_up_new_log_files() {
        let tmp = tempfile::tempdir().unwrap();
        let logs_root = tmp.path().join("logs");
        std::fs::create_dir_all(&logs_root).unwrap();

        let cfg = Arc::new(ibis_core::config::Config {
            logs_root: logs_root.display().to_string(),
            ..Default::default()
        });

        let mut watcher = Watcher::new(Some(cfg), None, None);
        watcher.start().expect("watcher start");

        // Create a new log file after start.
        std::fs::write(logs_root.join("new.log"), "ERROR x\n").unwrap();

        let mut registered = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let map = watcher.state.tailers.lock().unwrap();
            if map
                .keys()
                .any(|k| k.replace('\\', "/").ends_with("new.log"))
            {
                registered = true;
                break;
            }
        }
        watcher.stop().await;
        assert!(registered, "new log file must be picked up by the watcher");
    }
}
