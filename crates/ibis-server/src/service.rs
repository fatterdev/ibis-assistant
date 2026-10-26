//! Windows SCM service program and the daemon/service dispatch seam
//!
//!
//! Arg surface (exact spellings): `install`/`/install`/
//! `-install`/`--install`, the four `uninstall` spellings, `/run` (SCM run
//! mode), `start`/`stop` (PID-file daemon, [`crate::daemon`]). `run`/
//! `-run`/`--run` are deliberately NOT claimed — they are the binary's
//! interactive server entry (N-core).
//!
//! F-011 one-pass elevation: `install`/`uninstall` first probe the process
//! token (`IsUserAnAdmin`). Unelevated, the verb is relaunched on the current
//! executable via `ShellExecuteExW` with the `runas` verb (single UAC prompt),
//! the parent waits on the elevated child and relays its exit code. A declined
//! or failed relaunch falls back to the unelevated SCM attempt, which surfaces
//! the SCM access error exactly like the original port. The FFI is declared directly
//! against shell32/ole32/advapi32/kernel32 (no extra crates).
//!
//! SCM run contract (`/run`): `service_dispatcher::start` blocks the caller
//! while the SCM drives `service_main` on its own thread; the runner seam
//! ([`ServerRunner`]) is invoked on a dedicated thread after reporting
//! RUNNING. Stop/Shutdown controls report SERVICE_STOPPED and the dispatcher
//! returns — the process exit reaps the server thread. cancelled a context
//! that made `runServer` perform its graceful sequence; the binary's server
//! entry owns shutdown internally and exposes no external cancel, so the SCM
//! path cannot trigger the graceful sequence (audit N-A02, documented
//! deviation).
//!
//! Service registration mirrors the reference service library (
//! ): name `ibis-assistant`, display name
//! `Ibis Assistant`, description set via `ChangeServiceConfig2W`,
//! `OWN_PROCESS`, AutoStart, ErrorControl Ignore (the reference service library left the field
//! zero), LocalSystem account, launch arguments `["/run"]`, no dependencies.
//! Windows EventLog source registration is deliberately omitted (audit
//! N-A03: only its own logger consumed it).

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use windows_service::define_windows_service;
#[cfg(windows)]
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
#[cfg(windows)]
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
#[cfg(windows)]
use windows_service::service_dispatcher;
#[cfg(windows)]
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::daemon;

pub const SERVICE_NAME: &str = "ibis-assistant";
pub const SERVICE_DISPLAY_NAME: &str = "Ibis Assistant";
pub const SERVICE_DESCRIPTION: &str = "MCP Ibis Assistant for Codebase and Git Analysis";
pub const SERVICE_LAUNCH_ARGUMENT: &str = "/run";

/// The server-run seam`). `main.rs` wires its server entry here when dispatching;
/// the SCM run handler calls it on a dedicated thread with the arguments left
/// after the verb ( stripped `/run` the same way).
///
/// The runner exits the process itself (the `runServer` returned into a
/// process that only exited on service stop), so the pointer diverges.
pub type ServerRunner = fn(&[String]) -> !;

/// A verb recognized on the first argv element (the dispatcher seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceVerb {
    Install,
    Uninstall,
    ServiceRun,
    DaemonStart,
    DaemonStop,
}

impl ServiceVerb {
    /// F-011 verbs that need an elevated SCM context.
    pub fn requires_elevation(self) -> bool {
        matches!(self, ServiceVerb::Install | ServiceVerb::Uninstall)
    }
}

/// Pure routing of the raw argv slice; `None` means "not a service/daemon
/// argument" (the seam stays transparent to the CLI/wizard passthrough).
pub fn classify_command(args: &[String]) -> Option<ServiceVerb> {
    let cmd = args.first()?;
    Some(match cmd.as_str() {
        "/run" => ServiceVerb::ServiceRun,
        "/install" | "install" | "-install" | "--install" => ServiceVerb::Install,
        "/uninstall" | "uninstall" | "-uninstall" | "--uninstall" => ServiceVerb::Uninstall,
        "start" => ServiceVerb::DaemonStart,
        "stop" => ServiceVerb::DaemonStop,
        _ => return None,
    })
}

/// F-011 decision, isolated for tests: only install/uninstall on Windows,
/// and only when the process is not already elevated.
pub fn should_relaunch_elevated(
    already_elevated: bool,
    verb: ServiceVerb,
    platform_is_windows: bool,
) -> bool {
    verb.requires_elevation() && platform_is_windows && !already_elevated
}

/// The single dispatch seam called from `main.rs`. The raw arguments are
/// passed verbatim; `Some(code)` is the process exit code, `None` means the
/// arguments are not a service/daemon verb.
pub fn dispatch_service_command(args: &[String], runner: ServerRunner) -> Option<i32> {
    let verb = classify_command(args)?;
    Some(match verb {
        ServiceVerb::Install => service_install(args),
        ServiceVerb::Uninstall => service_uninstall(args),
        ServiceVerb::ServiceRun => run_service_program(runner),
        ServiceVerb::DaemonStart => daemon::start_background_server(),
        ServiceVerb::DaemonStop => daemon::stop_background_server(),
    })
}

// message texts (audit N-A01: failures now exit 1).
#[cfg(windows)]
const MSG_INSTALL_SUCCESS: &str = "Service installed successfully.";
#[cfg(windows)]
const MSG_INSTALL_FAILURE: &str = "Failed to install service";
#[cfg(windows)]
const MSG_UNINSTALL_SUCCESS: &str = "Service uninstalled successfully.";
#[cfg(windows)]
const MSG_UNINSTALL_FAILURE: &str = "Failed to uninstall service";
#[cfg(windows)]
const MSG_RUN_FAILURE: &str = "Failed to run service";
#[cfg(not(windows))]
const MSG_WINDOWS_ONLY: &str =
    "Service management is only supported on Windows; use 'start'/'stop' for the background daemon on Unix.";

#[cfg(windows)]
fn service_install(args: &[String]) -> i32 {
    if should_relaunch_elevated(is_current_process_elevated(), ServiceVerb::Install, true) {
        println!(
            "Requesting administrator privileges to install the service (single UAC prompt)..."
        );
        if let Some(code) = relaunch_elevated_and_wait(args) {
            return code;
        }
        // UAC declined or relaunch failed: the unelevated attempt below
        // remains the fallback and surfaces the SCM access error.
    }
    match install_service() {
        Ok(()) => {
            println!("{MSG_INSTALL_SUCCESS}");
            0
        }
        Err(err) => {
            eprintln!("{MSG_INSTALL_FAILURE}: {err}");
            1
        }
    }
}

#[cfg(windows)]
fn service_uninstall(args: &[String]) -> i32 {
    if should_relaunch_elevated(is_current_process_elevated(), ServiceVerb::Uninstall, true) {
        println!(
            "Requesting administrator privileges to uninstall the service (single UAC prompt)..."
        );
        if let Some(code) = relaunch_elevated_and_wait(args) {
            return code;
        }
    }
    match uninstall_service() {
        Ok(()) => {
            println!("{MSG_UNINSTALL_SUCCESS}");
            0
        }
        Err(err) => {
            eprintln!("{MSG_UNINSTALL_FAILURE}: {err}");
            1
        }
    }
}

#[cfg(not(windows))]
fn service_install(_args: &[String]) -> i32 {
    eprintln!("{MSG_WINDOWS_ONLY}");
    1
}

#[cfg(not(windows))]
fn service_uninstall(_args: &[String]) -> i32 {
    eprintln!("{MSG_WINDOWS_ONLY}");
    1
}

#[cfg(not(windows))]
fn run_service_program(_runner: ServerRunner) -> i32 {
    eprintln!("{MSG_WINDOWS_ONLY}");
    1
}

/// Parity: "service already exists"
/// beats CreateService's raw error 1073.
#[cfg(windows)]
fn install_service() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|e| e.to_string())?;
    if manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .is_ok()
    {
        return Err(format!("service {SERVICE_NAME} already exists"));
    }
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Ignore,
        executable_path: exe,
        launch_arguments: vec![OsString::from(SERVICE_LAUNCH_ARGUMENT)],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::QUERY_STATUS)
        .map_err(|e| e.to_string())?;
    drop(service);
    if let Err(err) = set_service_description(SERVICE_NAME, SERVICE_DESCRIPTION) {
        let _ = uninstall_service();
        return Err(err.to_string());
    }
    Ok(())
}

/// Parity: a missing service reads
/// "service ibis-assistant is not installed". The EventLog source removal is
/// omitted (N-A03).
#[cfg(windows)]
fn uninstall_service() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| e.to_string())?;
    let service = match manager.open_service(SERVICE_NAME, ServiceAccess::DELETE) {
        Ok(service) => service,
        Err(_) => return Err(format!("service {SERVICE_NAME} is not installed")),
    };
    service.delete().map_err(|e| e.to_string())
}

#[cfg(windows)]
fn set_service_description(name: &str, description: &str) -> std::io::Result<()> {
    const SC_MANAGER_CONNECT: u32 = 0x0002;
    const SERVICE_CHANGE_CONFIG: u32 = 0x0002;
    const SERVICE_CONFIG_DESCRIPTION: u32 = 1;
    unsafe {
        let scm = ffi::OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            let name_w = to_wide_os(std::ffi::OsStr::new(name));
            let service = ffi::OpenServiceW(scm, name_w.as_ptr(), SERVICE_CHANGE_CONFIG);
            if service == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let changed = (|| {
                let mut description_w = to_wide_os(std::ffi::OsStr::new(description));
                let desc = ffi::SERVICE_DESCRIPTIONW {
                    lp_description: description_w.as_mut_ptr(),
                };
                if ffi::ChangeServiceConfig2W(
                    service,
                    SERVICE_CONFIG_DESCRIPTION,
                    &desc as *const _ as *const core::ffi::c_void,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })();
            ffi::CloseServiceHandle(service);
            changed
        })();
        ffi::CloseServiceHandle(scm);
        result
    }
}

#[cfg(windows)]
fn is_current_process_elevated() -> bool {
    unsafe { ffi::IsUserAnAdmin() != 0 }
}

/// F-011 relaunch: `ShellExecuteExW("runas")` on the current executable with
/// the original arguments, waiting for the elevated child and relaying its
/// exit code. `None` = declined/failed (caller falls back to the unelevated
/// attempt).
#[cfg(windows)]
fn relaunch_elevated_and_wait(args: &[String]) -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    let file = to_wide_os(exe.as_os_str());
    let params = to_wide_os(std::ffi::OsStr::new(&join_command_line(args)));
    let verb = to_wide_os(std::ffi::OsStr::new("runas"));
    let mut sei = ffi::ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ffi::ShellExecuteInfoW>() as u32,
        f_mask: ffi::SEE_MASK_NOCLOSEPROCESS | ffi::SEE_MASK_NOASYNC,
        hwnd: 0,
        lp_verb: verb.as_ptr(),
        lp_file: file.as_ptr(),
        lp_parameters: params.as_ptr(),
        lp_directory: std::ptr::null(),
        n_show: ffi::SW_SHOWNORMAL,
        h_inst_app: 0,
        lp_id_list: std::ptr::null_mut(),
        lp_class: std::ptr::null(),
        hkey_class: 0,
        dw_hot_key: 0,
        h_icon: 0,
        h_process: 0,
    };
    unsafe {
        // ShellExecuteEx requires an initialized COM apartment.
        let com = ffi::CoInitializeEx(std::ptr::null_mut(), ffi::COINIT_APARTMENTTHREADED);
        let result = if ffi::ShellExecuteExW(&mut sei) != 0 && sei.h_process != 0 {
            ffi::WaitForSingleObject(sei.h_process, ffi::INFINITE);
            let mut code: u32 = 0;
            let exited =
                ffi::GetExitCodeProcess(sei.h_process, &mut code) != 0 && code != ffi::STILL_ACTIVE;
            ffi::CloseHandle(sei.h_process);
            if exited {
                Some(code as i32)
            } else {
                None
            }
        } else {
            None
        };
        if com == 0 {
            // S_OK: we own the apartment; S_FALSE / RPC_E_CHANGED_MODE do not.
            ffi::CoUninitialize();
        }
        result
    }
}

#[cfg(windows)]
fn join_command_line(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if !arg.is_empty() && arg.chars().all(|c| !c.is_whitespace() && c != '"') {
                arg.clone()
            } else {
                format!("\"{}\"", arg.replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
fn to_wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    let mut wide: Vec<u16> = value.encode_wide().collect();
    wide.push(0);
    wide
}

#[cfg(windows)]
mod ffi {
    use std::os::raw::c_void;

    pub type Handle = isize;

    pub const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    pub const SEE_MASK_NOASYNC: u32 = 0x0000_1000;
    pub const SW_SHOWNORMAL: i32 = 1;
    pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
    pub const INFINITE: u32 = 0xFFFF_FFFF;
    pub const STILL_ACTIVE: u32 = 259;

    #[repr(C)]
    pub struct ShellExecuteInfoW {
        pub cb_size: u32,
        pub f_mask: u32,
        pub hwnd: Handle,
        pub lp_verb: *const u16,
        pub lp_file: *const u16,
        pub lp_parameters: *const u16,
        pub lp_directory: *const u16,
        pub n_show: i32,
        pub h_inst_app: Handle,
        pub lp_id_list: *mut c_void,
        pub lp_class: *const u16,
        pub hkey_class: Handle,
        pub dw_hot_key: u32,
        pub h_icon: Handle,
        pub h_process: Handle,
    }

    #[repr(C)]
    pub struct SERVICE_DESCRIPTIONW {
        pub lp_description: *mut u16,
    }

    #[link(name = "shell32")]
    extern "system" {
        pub fn ShellExecuteExW(exec_info: *mut ShellExecuteInfoW) -> i32;
        pub fn IsUserAnAdmin() -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        pub fn CoInitializeEx(reserved: *mut c_void, co_init: u32) -> i32;
        pub fn CoUninitialize();
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        pub fn GetExitCodeProcess(handle: Handle, exit_code: *mut u32) -> i32;
        pub fn CloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        pub fn OpenSCManagerW(machine: *const u16, database: *const u16, access: u32) -> Handle;
        pub fn OpenServiceW(manager: Handle, name: *const u16, access: u32) -> Handle;
        pub fn ChangeServiceConfig2W(
            service: Handle,
            info_level: u32,
            buffer: *const c_void,
        ) -> i32;
        pub fn CloseServiceHandle(handle: Handle) -> i32;
    }
}

#[cfg(windows)]
static RUNNER: OnceLock<ServerRunner> = OnceLock::new();

#[cfg(windows)]
define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
fn service_main(arguments: Vec<OsString>) {
    if let Err(err) = run_service(arguments) {
        eprintln!("{MSG_RUN_FAILURE}: {err}");
    }
}

#[cfg(windows)]
fn run_service(arguments: Vec<OsString>) -> windows_service::Result<()> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            // the reference service library accepted Stop and Shutdown and both cancelled the run
            // context ( handler); mirror them here.
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    })?;

    let runner = match RUNNER.get() {
        Some(runner) => *runner,
        None => {
            eprintln!("{MSG_RUN_FAILURE}: server runner not installed");
            return report_stopped(&status_handle);
        }
    };

    // — the runner owns the server loop
    // (and its internal shutdown) until the process exits.
    let runner_flags: Vec<String> = arguments
        .into_iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::thread::spawn(move || {
        runner(&runner_flags);
    });

    let _ = stop_rx.recv();
    // `program.Stop cancelled the run context so runServer performed its
    // graceful sequence (audit N-A02). The runner seam owns shutdown
    // internally; report STOPPED and let the dispatcher return — the process
    // exit reaps the server thread.
    report_stopped(&status_handle)
}

#[cfg(windows)]
fn report_stopped(status_handle: &ServiceStatusHandle) -> windows_service::Result<()> {
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })
}

#[cfg(windows)]
fn run_service_program(runner: ServerRunner) -> i32 {
    let _ = RUNNER.set(runner);
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{MSG_RUN_FAILURE}: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No live SCM tests: install/uninstall/start/stop of a real Windows
    /// service mutate the machine's service database and require an elevated
    /// process, so they cannot run in CI. The matrices below pin the arg
    /// surface, the elevation decision (F-011) and the English message texts;
    /// the SCM body follows the windows-service crate's documented control
    /// flow and was verified against its 0.8.1 source.

    #[test]
    fn classify_matrix() {
        let verb = |first: &str| classify_command(&[first.to_string()]);
        for spelling in ["/install", "install", "-install", "--install"] {
            assert_eq!(verb(spelling), Some(ServiceVerb::Install), "{spelling}");
        }
        for spelling in ["/uninstall", "uninstall", "-uninstall", "--uninstall"] {
            assert_eq!(verb(spelling), Some(ServiceVerb::Uninstall), "{spelling}");
        }
        assert_eq!(verb("/run"), Some(ServiceVerb::ServiceRun));
        assert_eq!(verb("start"), Some(ServiceVerb::DaemonStart));
        assert_eq!(verb("stop"), Some(ServiceVerb::DaemonStop));

        assert_eq!(classify_command(&[]), None);
        for not_a_service_verb in [
            "run", "-run", "--run", "config", "version", "help", "Install", "INSTALL", "Stop", "",
        ] {
            assert_eq!(
                verb(not_a_service_verb),
                None,
                "unexpectedly claimed {not_a_service_verb:?}"
            );
        }
        let extras = vec!["start".to_string(), "--flag".to_string()];
        assert_eq!(classify_command(&extras), Some(ServiceVerb::DaemonStart));
    }

    #[test]
    fn requires_elevation_matrix() {
        assert!(ServiceVerb::Install.requires_elevation());
        assert!(ServiceVerb::Uninstall.requires_elevation());
        assert!(!ServiceVerb::ServiceRun.requires_elevation());
        assert!(!ServiceVerb::DaemonStart.requires_elevation());
        assert!(!ServiceVerb::DaemonStop.requires_elevation());
    }

    #[test]
    fn should_relaunch_elevated_matrix() {
        assert!(should_relaunch_elevated(false, ServiceVerb::Install, true));
        assert!(should_relaunch_elevated(
            false,
            ServiceVerb::Uninstall,
            true
        ));
        assert!(!should_relaunch_elevated(true, ServiceVerb::Install, true));
        assert!(!should_relaunch_elevated(
            true,
            ServiceVerb::Uninstall,
            true
        ));
        assert!(!should_relaunch_elevated(
            false,
            ServiceVerb::ServiceRun,
            true
        ));
        assert!(!should_relaunch_elevated(
            false,
            ServiceVerb::DaemonStart,
            true
        ));
        assert!(!should_relaunch_elevated(
            false,
            ServiceVerb::DaemonStop,
            true
        ));
        assert!(!should_relaunch_elevated(
            false,
            ServiceVerb::Install,
            false
        ));
        assert!(!should_relaunch_elevated(
            true,
            ServiceVerb::Uninstall,
            false
        ));
    }

    #[cfg(windows)]
    #[test]
    fn english_message_assertions() {
        assert_eq!(MSG_INSTALL_SUCCESS, "Service installed successfully.");
        assert_eq!(MSG_INSTALL_FAILURE, "Failed to install service");
        assert_eq!(MSG_UNINSTALL_SUCCESS, "Service uninstalled successfully.");
        assert_eq!(MSG_UNINSTALL_FAILURE, "Failed to uninstall service");
        assert_eq!(MSG_RUN_FAILURE, "Failed to run service");
    }

    #[cfg(windows)]
    #[test]
    fn service_surface_constants() {
        assert_eq!(SERVICE_NAME, "ibis-assistant");
        assert_eq!(SERVICE_DISPLAY_NAME, "Ibis Assistant");
        assert_eq!(
            SERVICE_DESCRIPTION,
            "MCP Ibis Assistant for Codebase and Git Analysis"
        );
        assert_eq!(SERVICE_LAUNCH_ARGUMENT, "/run");
    }

    #[cfg(windows)]
    #[test]
    fn join_command_line_quoting() {
        assert_eq!(join_command_line(&["install".to_string()]), "install");
        assert_eq!(
            join_command_line(&["--flag".to_string(), "a b".to_string()]),
            "--flag \"a b\""
        );
        assert_eq!(join_command_line(&["a\"b".to_string()]), "\"a\\\"b\"");
        assert_eq!(join_command_line(&[]), "");
    }
}
