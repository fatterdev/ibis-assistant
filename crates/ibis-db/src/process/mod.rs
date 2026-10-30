//! Embedded SurrealDB process lifecycle.
//!
//! Contract:
//! - `surreal start --user <u> --pass <p> --bind 127.0.0.1:<port> surrealkv:<absDataPath>`
//!   with stdout/stderr appended to `surreal.log` beside the ibis executable
//!   (args are never logged — they contain the password).
//! - Readiness: TCP dial every 500ms up to 30s; on timeout the process is killed.
//! - Spawn/stop policy is the shared one in `ibis_core::process_tree`:
//!   `configure_spawn` owns the child tree per OS (kill-on-close job + own
//!   process group on Windows; own process group + PDEATHSIG on Linux; own
//!   process group + parent-death watchdog on macOS) and `graceful_stop` asks
//!   first (console ctrl event / SIGINT to the group) and kills after the
//!   2s grace. On pipe-spawned children without a shared console the ctrl
//!   event cannot be delivered and the kill-after-grace path applies.

pub mod install;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::process::Child;

use crate::error::ProcessError;
use crate::process::install::ensure_surreal_db;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const DIAL_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Handles the lifecycle of the embedded SurrealDB process.
pub struct ProcessManager {
    child: Child,
    port: u16,
}

impl ProcessManager {
    /// Launches the surreal process in the background.
    pub async fn start_embedded(
        user: &str,
        password: &str,
        data_path: &str,
        port: u16,
        auto_update: bool,
    ) -> Result<ProcessManager, ProcessError> {
        // Ensure binary exists and is up to date
        let bin_path = ensure_surreal_db(auto_update)
            .await
            .map_err(|e| ProcessError::Other(format!("failed to ensure database binary: {e}")))?;
        ProcessManager::start_with_bin(&bin_path, user, password, data_path, port).await
    }

    /// Launches a specific surreal binary (install check skipped) — the seam
    /// used by integration tests and by callers managing the binary themselves.
    pub async fn start_with_bin(
        bin_path: &Path,
        user: &str,
        password: &str,
        data_path: &str,
        port: u16,
    ) -> Result<ProcessManager, ProcessError> {
        let abs_bin_path = absolute(bin_path).map_err(|e| {
            ProcessError::Other(format!(
                "failed to get absolute path for database binary: {e}"
            ))
        })?;
        let abs_data_path = absolute(Path::new(data_path)).map_err(|e| {
            ProcessError::Other(format!("failed to get absolute path for data: {e}"))
        })?;
        let bind_addr = format!("127.0.0.1:{port}");
        let file_arg = format!("surrealkv:{}", abs_data_path.display());

        let mut cmd = tokio::process::Command::new(&abs_bin_path);
        cmd.args([
            "start", "--user", user, "--pass", password, "--bind", &bind_addr,
        ])
        .arg(file_arg);
        apply_log_redirection(&mut cmd);

        // Do not log args: they include --pass <password>.
        tracing::info!(
            "Starting embedded database: {} (bind={bind_addr}, data={})",
            abs_bin_path.display(),
            abs_data_path.display()
        );
        // Own the child tree: per-OS spawn configuration (kill-on-close job +
        // process group on Windows; process group + PDEATHSIG on Linux;
        // process group + parent-death watchdog on macOS).
        ibis_core::process_tree::configure_spawn(&mut cmd)
            .map_err(|e| ProcessError::Spawn(e.to_string()))?;
        let mut child = cmd
            .spawn()
            .map_err(|e| ProcessError::Spawn(e.to_string()))?;

        #[cfg(windows)]
        if !ibis_core::process_tree::children_job_active() {
            // Self-attach unavailable: assign this child explicitly.
            if let Some(handle) = child.raw_handle() {
                let _ = ibis_core::process_tree::attach_child(handle as isize);
            }
        }

        #[cfg(target_os = "macos")]
        if let Some(pid) = child.id() {
            ibis_core::process_tree::arm_parent_death_watchdog(pid)
                .map_err(|e| ProcessError::Spawn(e.to_string()))?;
        }

        if let Err(err) = wait_for_port(port, READY_TIMEOUT).await {
            // If timeout, kill and return error
            let _ = child.kill().await;
            tracing::warn!("database start failed: {err}");
            return Err(ProcessError::NotReady {
                port,
                timeout: READY_TIMEOUT,
            });
        }

        Ok(ProcessManager { child, port })
    }

    /// The port the embedded instance was started on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Graceful shutdown via the shared process-tree policy: console ctrl
    /// event / SIGINT to the child's process group first (flush of the
    /// surrealkv journal), then kill after the 2s grace.
    pub async fn stop(&mut self) -> Result<(), ProcessError> {
        tracing::info!("Stopping embedded database...");
        ibis_core::process_tree::graceful_stop(&mut self.child, STOP_GRACE)
            .await
            .map_err(ProcessError::Io)
    }
}

/// Appends the process output to `surreal.log` beside the ibis executable,
/// falling back to inherited stdio when the log file cannot be opened
///
fn apply_log_redirection(cmd: &mut tokio::process::Command) {
    let log_path = match std::env::current_exe() {
        Ok(exe) => exe.parent().unwrap_or(Path::new(".")).join("surreal.log"),
        Err(_) => PathBuf::from("surreal.log"),
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(log_file) => {
            let stderr = log_file.try_clone();
            cmd.stdout(std::process::Stdio::from(log_file));
            if let Ok(stderr) = stderr {
                cmd.stderr(std::process::Stdio::from(stderr));
            }
        }
        Err(_) => {
            // Fallback to stdio if log file fails
            cmd.stdout(std::process::Stdio::inherit());
            cmd.stderr(std::process::Stdio::inherit());
        }
    }
}

/// Dials this crate every 500ms until it accepts or the deadline passes
///
pub async fn wait_for_port(port: u16, timeout: Duration) -> Result<(), ProcessError> {
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(conn)) => {
                drop(conn);
                return Ok(());
            }
            _ => tokio::time::sleep(DIAL_TIMEOUT).await,
        }
    }
    Err(ProcessError::NotReady { port, timeout })
}

/// Normalizes a path to its absolute form.
fn absolute(path: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_port_times_out_on_closed_port() {
        // Grab a free port by binding then releasing it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let result = wait_for_port(port, Duration::from_millis(1200)).await;
        assert!(result.is_err(), "expected timeout on closed port {port}");
    }

    #[tokio::test]
    async fn wait_for_port_succeeds_on_open_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            // Keep the listener alive while the dial runs.
            for _ in 0..10 {
                if listener.accept().is_ok() {
                    // one probe is enough
                }
            }
        });
        let result = wait_for_port(port, Duration::from_secs(5)).await;
        assert!(
            result.is_ok(),
            "expected dial to succeed on open port {port}"
        );
    }
}
