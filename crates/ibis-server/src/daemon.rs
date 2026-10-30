//! PID-file background daemon.
//!
//! Port shape:
//! - `start`/`stop` own the exact argv spellings and exit-code contract,
//!   with every user-facing message translated to English (N-A15).
//! - `start` spawns the own executable detached (`exe run`) with stdout/stderr
//!   redirected to `server.log` next to the binary, then writes the PID file.
//! - `stop` reads the PID, probes liveness, then applies the per-OS stop
//!   semantics mandated by D10: Windows CTRL_BREAK → 5s grace →
//!   TerminateProcess; Unix SIGTERM → 5s grace → SIGKILL.
//! - PID-file liveness is keyed on the bare PID identically (audit
//!   N-A04/N-A05: no process-identity check — parity kept, improvement
//!   backlogged). Unix probes guard against `kill(-1, 0)` broadcasts by
//!   rejecting pids that do not fit a positive `i32` (audit N-A08).
//!
//! The spawn detaches with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on
//! Windows and `process_group(0)` (setpgid) on Unix — used `Setsid`; a new
//! process group is the load-bearing part for the stop semantics (D15 note).

use std::io;
use std::path::{Path, PathBuf};

pub const PID_FILE_NAME: &str = "ibis.pid";
pub const LOG_FILE_NAME: &str = "server.log";

/// Console-stdout/stderr report for one daemon action, with the process exit
/// code applied via `os.Exit`. Emitted through [`emit`]; assertions in the
/// tests pin the English wording (N-A15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Report {
    fn success(stdout: impl Into<String>) -> Report {
        Report {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn failure(stderr: impl Into<String>) -> Report {
        Report {
            code: 1,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Prints a [`Report`] the way the mix of `fmt.Printf`/`fmt.Fprintf` did.
pub fn emit(report: &Report) {
    for line in report.stdout.lines() {
        println!("{line}");
    }
    if !report.stderr.is_empty() {
        eprintln!("{}", report.stderr);
    }
}

/// Failure of the detached spawn step, split so the exact messages survive
/// ( log-file vs `:50` spawn errors).
#[derive(Debug)]
pub enum StartError {
    LogFile { path: String, source: io::Error },
    Spawn(io::Error),
}

/// ` startBackgroundServer` against injectable exe and
/// spawn seams (tests pass a mock; production uses [`spawn_detached_with_log`]).
pub fn start_background_server_with(
    exe: &Result<PathBuf, io::Error>,
    spawn: impl Fn(&Path) -> Result<u32, StartError>,
    pid_path: &Path,
) -> Report {
    if let Ok(bytes) = std::fs::read(pid_path) {
        if let Some(pid) = parse_pid(&String::from_utf8_lossy(&bytes)) {
            if is_process_running(pid) {
                return Report::success(format!("The server is already running with PID {pid}."));
            }
        }
    }

    let exe_path = match exe {
        Ok(path) => path,
        Err(err) => return Report::failure(format!("Error retrieving executable: {err}")),
    };

    let pid = match spawn(exe_path) {
        Ok(pid) => pid,
        Err(err) => {
            return Report::failure(match err {
                StartError::LogFile { path, source } => {
                    format!("Error opening log file {path}: {source}")
                }
                StartError::Spawn(source) => {
                    format!("Error starting the server in the background: {source}")
                }
            });
        }
    };

    if let Err(err) = std::fs::write(pid_path, pid.to_string()) {
        return Report::failure(format!("Error writing PID file: {err}"));
    }
    Report::success(format!(
        "Server started in background with PID {pid}.\nLogs are redirected to {LOG_FILE_NAME}."
    ))
}

/// ` stopBackgroundServer` against an injectable PID-file
/// path (tests use a tempdir; production uses [`get_pid_file_path`]).
pub fn stop_background_server_with(pid_path: &Path) -> Report {
    let Ok(bytes) = std::fs::read(pid_path) else {
        return Report::success("The server is not running (PID file not found).");
    };

    let Some(pid) = parse_pid(&String::from_utf8_lossy(&bytes)) else {
        let _ = std::fs::remove_file(pid_path);
        return Report::success(format!("Corrupt PID file. Removing file {PID_FILE_NAME}."));
    };

    if !is_process_running(pid) {
        let _ = std::fs::remove_file(pid_path);
        return Report::success(format!(
            "The process with PID {pid} is not running. Removing file {PID_FILE_NAME}."
        ));
    }

    match stop_process(pid) {
        Ok(()) => {
            let _ = std::fs::remove_file(pid_path);
            Report::success(format!(
                "Stopping the server (PID {pid})...\nServer stopped successfully."
            ))
        }
        Err(err) => Report {
            code: 1,
            stdout: format!("Stopping the server (PID {pid})..."),
            stderr: format!("Error while stopping the process: {err}"),
        },
    }
}

/// Production `start` entry ( exited via `os.Exit`; here the code is
/// returned to the dispatcher seam).
pub fn start_background_server() -> i32 {
    let exe = std::env::current_exe();
    let pid_path = pid_file_path_from_exe(&exe);
    let report = start_background_server_with(&exe, spawn_detached_with_log, &pid_path);
    emit(&report);
    report.code
}

/// Production `stop` entry.
pub fn stop_background_server() -> i32 {
    let report = stop_background_server_with(&get_pid_file_path());
    emit(&report);
    report.code
}

/// ` getPidFilePath`: exe directory, falling back to a
/// CWD-relative bare name when the executable path is unknown.
pub fn pid_file_path_from_exe(exe: &Result<PathBuf, io::Error>) -> PathBuf {
    match exe {
        Ok(path) => path
            .parent()
            .map(|dir| dir.join(PID_FILE_NAME))
            .unwrap_or_else(|| PathBuf::from(PID_FILE_NAME)),
        Err(_) => PathBuf::from(PID_FILE_NAME),
    }
}

pub fn get_pid_file_path() -> PathBuf {
    pid_file_path_from_exe(&std::env::current_exe())
}

/// ` configExists`: CWD first, then the exe directory.
pub fn config_exists_in(cwd: &Path, exe_dir: Option<&Path>) -> bool {
    if cwd.join("config.json").exists() {
        return true;
    }
    exe_dir.is_some_and(|dir| dir.join("config.json").exists())
}

pub fn config_exists() -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    config_exists_in(&cwd, exe_dir.as_deref())
}

/// ` peekFlag` — raw pre-flag scan: `-name value`
/// (value must not start with `-`) or `-name=value`, first match wins.
pub fn peek_flag(args: &[String], names: &[&str]) -> String {
    for (i, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                return args[i + 1].clone();
            }
            if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
                return value.to_string();
            }
        }
    }
    String::new()
}

/// ` isPluginLaunch` with the environment value made
/// explicit for tests.
pub fn is_plugin_launch_from(env_runtime_mode: &str, args: &[String]) -> bool {
    if ibis_core::config::is_plugin_runtime(env_runtime_mode) {
        return true;
    }
    ibis_core::config::is_plugin_runtime(&peek_flag(args, &["-runtime-mode", "--runtime-mode"]))
}

pub fn is_plugin_launch(args: &[String]) -> bool {
    is_plugin_launch_from(&std::env::var("RUNTIME_MODE").unwrap_or_default(), args)
}

/// `strconv.Atoi(strings.TrimSpace(...))` restricted to plausible positive
/// pids. accepted negative Atoi results and then treated them as
/// not-running via the probe; the corrupt-file branch here is the equivalent
/// observable for a CLI tool (S3, negligible).
pub fn parse_pid(data: &str) -> Option<u32> {
    data.trim().parse::<u32>().ok()
}

/// Shared pid probe entry. Pids are rejected before they can reach a
/// platform syscall that treats negative/wrapped values specially (Unix
/// `kill(-1, 0)` signals every process — audit N-A08).
pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    is_process_running_raw(pid)
}

/// D10 stop semantics entry with the same guard as [`is_process_running`].
pub fn stop_process(pid: u32) -> io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"));
    }
    stop_process_raw(pid)
}

#[cfg(windows)]
mod ffi {
    // isize keeps the declarations identical to the ones in `service.rs`.
    pub type Handle = isize;

    //
    pub const PROCESS_TERMINATE: u32 = 0x0001;
    pub const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
    pub const STILL_ACTIVE: u32 = 259;
    pub const CTRL_BREAK_EVENT: u32 = 1;
    //
    pub const DETACHED_PROCESS: u32 = 0x0000_0008;
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        pub fn TerminateProcess(process: Handle, exit_code: u32) -> i32;
        pub fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        pub fn CloseHandle(object: Handle) -> i32;
        pub fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
        pub fn GetProcessVersion(process_id: u32) -> u32;
    }
}

/// ` isProcessRunning` (GetProcessVersion != 0).
/// Parity note (N-A04): processes the caller cannot query read as not-running.
#[cfg(windows)]
fn is_process_running_raw(pid: u32) -> bool {
    unsafe { ffi::GetProcessVersion(pid) != 0 }
}

/// ` isProcessRunning` (signal 0; EPERM reads as
/// not-running identically — N-A08 parity).
#[cfg(unix)]
fn is_process_running_raw(pid: u32) -> bool {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(any(windows, unix)))]
fn is_process_running_raw(_pid: u32) -> bool {
    false
}

/// ` stopProcess`: CTRL_BREAK to the process group
/// with a 5s/200ms exit-code grace, then TerminateProcess on the atomic
/// OpenProcess handle. Detached (console-less) daemon children never receive
/// the event, so the grace window is skipped — exactly the behavior (N-A07).
#[cfg(windows)]
fn stop_process_raw(pid: u32) -> io::Result<()> {
    use std::time::{Duration, Instant};
    unsafe {
        let handle = ffi::OpenProcess(ffi::PROCESS_TERMINATE | ffi::PROCESS_SYNCHRONIZE, 0, pid);
        if handle == 0 {
            return Ok(());
        }
        let delivered = ffi::GenerateConsoleCtrlEvent(ffi::CTRL_BREAK_EVENT, pid) != 0;
        if delivered {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let mut exit_code: u32 = 0;
                ffi::GetExitCodeProcess(handle, &mut exit_code);
                if exit_code != ffi::STILL_ACTIVE {
                    ffi::CloseHandle(handle);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let terminated = ffi::TerminateProcess(handle, 1);
        ffi::CloseHandle(handle);
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// ` stopProcess`: SIGTERM, 50×100ms grace probing
/// liveness, then SIGKILL (including the SIGTERM-refused fallback).
#[cfg(unix)]
fn stop_process_raw(pid: u32) -> io::Result<()> {
    use std::time::Duration;
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    unsafe {
        if kill(pid as i32, SIGTERM) != 0 {
            return if kill(pid as i32, SIGKILL) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            };
        }
        for _ in 0..50 {
            if !is_process_running(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if kill(pid as i32, SIGKILL) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(not(any(windows, unix)))]
fn stop_process_raw(_pid: u32) -> io::Result<()> {
    Err(io::Error::other("stop unsupported on this platform"))
}

/// ` startBackgroundProcess` — detached child with
/// stdout/stderr appended to the log file.
#[cfg(windows)]
fn spawn_detached(exe: &Path, args: &[String], log: &std::fs::File) -> io::Result<u32> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let child = Command::new(exe)
        .args(args)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .creation_flags(ffi::DETACHED_PROCESS | ffi::CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    Ok(child.id())
}

/// Unix `startBackgroundProcess` — `Setsid` implemented as
/// `process_group(0)` (new process group; the session split is the only
/// observable difference and does not affect the stop semantics, D15 note).
#[cfg(unix)]
fn spawn_detached(exe: &Path, args: &[String], log: &std::fs::File) -> io::Result<u32> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let child = Command::new(exe)
        .args(args)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .process_group(0)
        .spawn()?;
    Ok(child.id())
}

#[cfg(not(any(windows, unix)))]
fn spawn_detached(_exe: &Path, _args: &[String], _log: &std::fs::File) -> io::Result<u32> {
    Err(io::Error::other(
        "detached spawn unsupported on this platform",
    ))
}

/// Production spawn seam: append-mode `server.log` next to the binary, then
/// `exe run` detached.
#[cfg(any(windows, unix))]
fn spawn_detached_with_log(exe: &Path) -> Result<u32, StartError> {
    let log_path = exe
        .parent()
        .map(|dir| dir.join(LOG_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(LOG_FILE_NAME));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| StartError::LogFile {
            path: log_path.display().to_string(),
            source,
        })?;
    let args = ["run".to_string()];
    spawn_detached(exe, &args, &log).map_err(StartError::Spawn)
}

#[cfg(not(any(windows, unix)))]
fn spawn_detached_with_log(_exe: &Path) -> Result<u32, StartError> {
    Err(StartError::Spawn(io::Error::other(
        "detached spawn unsupported on this platform",
    )))
}

#[cfg(test)]
mod tests {
    //! No live SCM/elevation tests live in this suite (install/uninstall/run
    //! mutate the machine's service database and require an elevated process).
    //! The daemon lifecycle below is fully exercised against real child
    //! processes and tempdir PID files instead.

    use super::*;
    use tempfile::tempdir;

    fn temp_pid_path(dir: &Path) -> PathBuf {
        dir.join(PID_FILE_NAME)
    }

    #[test]
    fn peek_flag_matrix() {
        let space = vec!["-runtime-mode".to_string(), "plugin".to_string()];
        assert_eq!(peek_flag(&space, &["-runtime-mode"]), "plugin");

        let equals = vec!["--runtime-mode=plugin".to_string()];
        assert_eq!(
            peek_flag(&equals, &["-runtime-mode", "--runtime-mode"]),
            "plugin"
        );

        let flag_value = vec!["-runtime-mode".to_string(), "-x".to_string()];
        assert_eq!(peek_flag(&flag_value, &["-runtime-mode"]), "");

        let dangling = vec!["-runtime-mode".to_string()];
        assert_eq!(peek_flag(&dangling, &["-runtime-mode"]), "");

        let unrelated = vec!["run".to_string(), "--port".to_string(), "1".to_string()];
        assert_eq!(
            peek_flag(&unrelated, &["-runtime-mode", "--runtime-mode"]),
            ""
        );

        let first_wins = vec![
            "-runtime-mode".to_string(),
            "plugin".to_string(),
            "-runtime-mode".to_string(),
            "server".to_string(),
        ];
        assert_eq!(peek_flag(&first_wins, &["-runtime-mode"]), "plugin");

        let name_order = vec!["--a".to_string(), "v".to_string()];
        assert_eq!(peek_flag(&name_order, &["-a", "--a"]), "v");
    }

    #[test]
    fn is_plugin_launch_matrix() {
        assert!(is_plugin_launch_from("plugin", &[]));
        assert!(is_plugin_launch_from(
            "",
            &[
                "run".to_string(),
                "-runtime-mode".to_string(),
                "plugin".to_string()
            ]
        ));
        assert!(is_plugin_launch_from(
            "",
            &["--runtime-mode=plugin".to_string()]
        ));
        assert!(!is_plugin_launch_from("personal", &[]));
        assert!(!is_plugin_launch_from("", &["run".to_string()]));
        assert!(!is_plugin_launch_from(
            "",
            &["-runtime-mode".to_string(), "-plugin".to_string()]
        ));
    }

    #[test]
    fn config_exists_matrix() {
        let cwd = tempdir().unwrap();
        let exe_dir = tempdir().unwrap();
        assert!(!config_exists_in(cwd.path(), Some(exe_dir.path())));
        std::fs::write(cwd.path().join("config.json"), "{}").unwrap();
        assert!(config_exists_in(cwd.path(), Some(exe_dir.path())));
        std::fs::remove_file(cwd.path().join("config.json")).unwrap();
        std::fs::write(exe_dir.path().join("config.json"), "{}").unwrap();
        assert!(config_exists_in(cwd.path(), Some(exe_dir.path())));
        assert!(!config_exists_in(cwd.path(), None));
    }

    #[test]
    fn parse_pid_matrix() {
        assert_eq!(parse_pid("1234"), Some(1234));
        assert_eq!(parse_pid("  42\n"), Some(42));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("not-a-pid"), None);
        assert_eq!(parse_pid("-1"), None);
        assert_eq!(parse_pid("+7"), Some(7));
    }

    #[test]
    fn pid_file_path_matrix() {
        let exe = Ok(PathBuf::from("C:/tools/ibis.exe"));
        assert_eq!(
            pid_file_path_from_exe(&exe),
            PathBuf::from("C:/tools/ibis.pid")
        );
        let failed = Err(io::Error::other("no exe"));
        assert_eq!(
            pid_file_path_from_exe(&failed),
            PathBuf::from(PID_FILE_NAME)
        );
    }

    #[test]
    fn start_reports_already_running() {
        let dir = tempdir().unwrap();
        let pid_path = temp_pid_path(dir.path());
        let self_pid = std::process::id();
        std::fs::write(&pid_path, self_pid.to_string()).unwrap();

        let spawned = std::cell::Cell::new(false);
        let spawn = |_: &Path| {
            spawned.set(true);
            Ok(u32::MAX)
        };
        let exe = Ok(dir.path().join("ibis.exe"));
        let report = start_background_server_with(&exe, spawn, &pid_path);
        assert_eq!(report.code, 0);
        assert_eq!(
            report.stdout,
            format!("The server is already running with PID {self_pid}.")
        );
        assert!(report.stderr.is_empty());
        assert!(!spawned.get());
    }

    #[test]
    fn start_writes_pid_file_and_reports() {
        let dir = tempdir().unwrap();
        let pid_path = temp_pid_path(dir.path());
        let spawn = |_: &Path| Ok(12345);
        let exe = Ok(dir.path().join("ibis.exe"));
        let report = start_background_server_with(&exe, spawn, &pid_path);
        assert_eq!(report.code, 0);
        assert_eq!(
            report.stdout,
            "Server started in background with PID 12345.\nLogs are redirected to server.log."
        );
        assert_eq!(std::fs::read_to_string(&pid_path).unwrap(), "12345");
    }

    #[test]
    fn start_spawn_failure_report() {
        let dir = tempdir().unwrap();
        let spawn = |_: &Path| -> Result<u32, StartError> {
            Err(StartError::Spawn(io::Error::other("boom")))
        };
        let exe = Ok(dir.path().join("ibis.exe"));
        let report = start_background_server_with(&exe, spawn, &temp_pid_path(dir.path()));
        assert_eq!(report.code, 1);
        assert_eq!(
            report.stderr,
            "Error starting the server in the background: boom"
        );
    }

    #[test]
    fn start_log_failure_report() {
        let dir = tempdir().unwrap();
        let spawn = |_: &Path| -> Result<u32, StartError> {
            Err(StartError::LogFile {
                path: "C:/x/server.log".to_string(),
                source: io::Error::other("denied"),
            })
        };
        let exe = Ok(dir.path().join("ibis.exe"));
        let report = start_background_server_with(&exe, spawn, &temp_pid_path(dir.path()));
        assert_eq!(report.code, 1);
        assert_eq!(
            report.stderr,
            "Error opening log file C:/x/server.log: denied"
        );
    }

    #[test]
    fn start_exe_error_report() {
        let dir = tempdir().unwrap();
        let spawn = |_: &Path| Ok(1);
        let exe = Err(io::Error::other("gone"));
        let report = start_background_server_with(&exe, spawn, &temp_pid_path(dir.path()));
        assert_eq!(report.code, 1);
        assert_eq!(report.stderr, "Error retrieving executable: gone");
    }

    #[test]
    fn start_pid_write_failure_report() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("missing-dir").join(PID_FILE_NAME);
        let spawn = |_: &Path| Ok(7);
        let exe = Ok(dir.path().join("ibis.exe"));
        let report = start_background_server_with(&exe, spawn, &pid_path);
        assert_eq!(report.code, 1);
        assert!(report.stderr.starts_with("Error writing PID file: "));
    }

    #[test]
    fn stop_reports_missing_pid_file() {
        let dir = tempdir().unwrap();
        let report = stop_background_server_with(&temp_pid_path(dir.path()));
        assert_eq!(
            report,
            Report::success("The server is not running (PID file not found).")
        );
    }

    #[test]
    fn stop_removes_corrupt_pid_file() {
        let dir = tempdir().unwrap();
        let pid_path = temp_pid_path(dir.path());
        std::fs::write(&pid_path, "not-a-pid").unwrap();
        let report = stop_background_server_with(&pid_path);
        assert_eq!(
            report,
            Report::success(format!("Corrupt PID file. Removing file {PID_FILE_NAME}."))
        );
        assert!(!pid_path.exists());
    }

    #[test]
    fn stop_removes_stale_pid_file() {
        let dir = tempdir().unwrap();
        let pid_path = temp_pid_path(dir.path());
        std::fs::write(&pid_path, "4294967295").unwrap();
        let report = stop_background_server_with(&pid_path);
        assert_eq!(
            report,
            Report::success(format!(
                "The process with PID 4294967295 is not running. Removing file {PID_FILE_NAME}."
            ))
        );
        assert!(!pid_path.exists());
    }

    #[test]
    fn is_process_running_self_and_bogus() {
        assert!(is_process_running(std::process::id()));
        assert!(!is_process_running(0));
        assert!(!is_process_running(4294967295));
    }

    #[cfg(any(windows, unix))]
    fn spawn_sleeper() -> std::process::Child {
        use std::process::{Command, Stdio};
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // Own console-ctrl group so the CTRL_BREAK grace path is exercised.
            Command::new("cmd")
                .args(["/C", "ping", "-n", "60", "127.0.0.1"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x0000_0200)
                .spawn()
                .expect("spawn cmd sleeper")
        }
        #[cfg(unix)]
        {
            Command::new("sleep")
                .arg("60")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleep")
        }
    }

    #[cfg(any(windows, unix))]
    fn reap(child: &mut std::process::Child) {
        let _ = child.wait();
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn stop_process_kills_spawned_process() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        assert!(is_process_running(pid));
        stop_process(pid).expect("stop_process");
        reap(&mut child);
        assert!(!is_process_running(pid));
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn stop_background_server_stops_running_server() {
        let dir = tempdir().unwrap();
        let mut child = spawn_sleeper();
        let pid = child.id();
        let pid_path = temp_pid_path(dir.path());
        std::fs::write(&pid_path, pid.to_string()).unwrap();

        let report = stop_background_server_with(&pid_path);
        reap(&mut child);

        assert_eq!(report.code, 0);
        assert_eq!(
            report.stdout,
            format!("Stopping the server (PID {pid})...\nServer stopped successfully.")
        );
        assert!(report.stderr.is_empty());
        assert!(!pid_path.exists());
        assert!(!is_process_running(pid));
    }
}
