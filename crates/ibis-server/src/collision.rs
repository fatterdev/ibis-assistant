//! Collision warnings plus the
//! PID-file helpers from /
//! that `warnPluginCollisions` consumed.
//!
//! Semantics/messages preserved verbatim; the process-liveness probe is the
//! one platform-shaped deviation ( called `GetProcessVersion` via
//! `syscall.NewLazyDLL`; Rust probes `tasklist` on Windows and `/proc/<pid>`
//! on Unix — best-effort, only used for the startup warning).

use std::net::IpAddr;

/// The daemon PID file name.
pub const PID_FILE_NAME: &str = "ibis.pid";

/// `PortInUse: reports whether something is already listening on
/// host:port (200ms dial timeout; port <= 0 → false).
pub fn port_in_use(host: &str, port: i64) -> bool {
    if port <= 0 {
        return false;
    }
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::new(ip, port as u16),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
}

/// `CollisionWarning: describes a personal daemon / HTTP listener that
/// the plugin should not attach to. Empty string when there is no collision.
pub fn collision_warning(
    pid_running: bool,
    pid: i64,
    default_port_in_use: bool,
    port: i64,
) -> String {
    match (pid_running, default_port_in_use) {
        (true, true) => format!(
            "warning: personal Ibis Assistant appears to be running (pid {pid}) and port {port} is in use; plugin continues on stdio with its own database and will not attach to that HTTP listener"
        ),
        (true, false) => format!(
            "warning: personal Ibis Assistant appears to be running (pid {pid}); plugin continues on stdio with its own database and will not attach to that process"
        ),
        (false, true) => format!(
            "warning: default HTTP port {port} is in use (likely a personal daemon); plugin continues on stdio with its own database and will not attach to that listener"
        ),
        (false, false) => String::new(),
    }
}

/// `getPidFilePath : beside the executable,
/// falling back to the bare name when the exe path is unavailable.
pub fn pid_file_path() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join(PID_FILE_NAME).display().to_string();
        }
    }
    PID_FILE_NAME.to_string()
}

/// `isProcessRunning ( / ):
/// best-effort liveness probe.
pub fn is_process_running(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Scenario: used GetProcessVersion(pid) != 0. tasklist is the portable CLI
        // equivalent; failures read as "not running" (best-effort warning).
        let filter = format!("/FI \"PID eq {pid}\"");
        std::process::Command::new("tasklist")
            .args(["/NH", "/FO", "CSV", &filter])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
            .map(|out| {
                let text = String::from_utf8_lossy(&out.stdout);
                text.contains(&format!("\"{pid}\""))
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// Reads the recorded daemon PID, returning `(pid, running)` (
/// `warnPluginCollisions` prologue).
pub fn daemon_pid_status() -> (i64, bool) {
    let Ok(data) = std::fs::read_to_string(pid_file_path()) else {
        return (0, false);
    };
    match data.trim().parse::<i64>() {
        Ok(pid) => (pid, is_process_running(pid)),
        Err(_) => (0, false),
    }
}

/// `warnPluginCollisions : logs and surfaces the
/// collision message for plugin launches.
pub fn warn_plugin_collisions(cfg: &ibis_core::config::Config) {
    let (pid, pid_running) = daemon_pid_status();
    let port_busy = port_in_use("127.0.0.1", cfg.port);
    let msg = collision_warning(pid_running, pid, port_busy, cfg.port);
    if msg.is_empty() {
        return;
    }
    tracing::warn!("{msg}");
    // `if cfg != nil && cfg.Mode != "stdio" && !cfg.IsPlugin` — inside
    // this function that combination is unreachable (audit finding N-A10);
    // the Rust port keeps the log only.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: TestCollisionWarning.
    #[test]
    fn collision_warning_matrix() {
        assert_eq!(collision_warning(false, 0, false, 3030), "");
        let msg = collision_warning(true, 42, true, 3030);
        assert!(!msg.is_empty());
        assert!(msg.contains("pid 42"));
        assert!(msg.contains("port 3030"));
        let only_pid = collision_warning(true, 7, false, 3030);
        assert!(only_pid.contains("pid 7"));
        assert!(!only_pid.contains("in use"));
        let only_port = collision_warning(false, 0, true, 3000);
        assert!(only_port.contains("port 3000"));
        assert!(!only_port.contains("pid"));
    }

    /// Scenario: TestPortInUse: a bound listener reports in-use; port 1
    /// does not.
    #[test]
    fn port_in_use_probes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(port_in_use("127.0.0.1", port as i64));
        drop(listener);
        assert!(!port_in_use("127.0.0.1", 1));
        assert!(!port_in_use("127.0.0.1", 0));
        assert!(!port_in_use("127.0.0.1", -5));
    }

    #[test]
    fn pid_helpers_parse() {
        assert!(!is_process_running(0));
        assert!(!is_process_running(-1));
    }
}
