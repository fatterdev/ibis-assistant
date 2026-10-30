//! Process-tree ownership for spawned sidecars (portable: Windows / Linux /
//! macOS).
//!
//! Ownership principle: **ibis owns only the processes it spawns.** On Windows
//! the Job Object is created lazily — at the first child spawn (embedded
//! SurrealDB, `ollama serve`, ast-grep sidecar) — and the ibis process attaches
//! itself right before the spawn, so every child inherits the membership. On
//! unix every spawned sidecar gets its own process group and a parent-death
//! mechanism (kernel PDEATHSIG on Linux, a watchdog child on macOS). When the
//! `ibis` process exits (graceful stop, crash or hard kill), whatever is left
//! of the tree is terminated. No orphaned `surreal`/`ollama` after tests or
//! IDE kills.
//!
//! Configurations that use already-running, independent services (`attach`
//! mode over Streamable HTTP, external SurrealDB/Ollama) never spawn those
//! children, therefore never create the job, never arm a watchdog and never
//! touch those processes.
//!
//! Per-OS guarantees (full study: `docs/planning/sidecar-lifecycle-plan.md`):
//!
//! - **Windows** — kill-on-close Job Object (kernel): the kernel terminates
//!   every member when the ibis process exits for any reason; grandchildren
//!   are covered by job inheritance. Fallback when a parent harness job
//!   disallows self-attachment: per-child `AssignProcessToJobObject`.
//! - **Linux** — `pre_exec` registers `prctl(PR_SET_PDEATHSIG, SIGKILL)`: the
//!   kernel delivers SIGKILL to the child when the parent dies. The
//!   fork→prctl race is closed by re-checking `getppid()` right after `prctl`
//!   (if the parent is already gone no signal would ever arrive — exit now).
//!   Caveat (man PR_SET_PDEATHSIG): the signal is bound to the *creating
//!   thread* — spawn sidecars only from long-lived threads (the main runtime
//!   workers), never from `spawn_blocking`. PDEATHSIG covers the direct child
//!   only; grandchildren are covered by the graceful `killpg` and by the
//!   post-crash reconciliation follow-up.
//! - **macOS** — no kernel parent-death primitive (no job object, no
//!   PDEATHSIG). The child gets its own process group (`setpgid(0,0)`) and a
//!   **watchdog child** is armed right after the spawn:
//!   `ibis -sidecar-watchdog <pgid>` holds the read end of a pipe whose write
//!   end this process keeps open (leaked) for its lifetime. When ibis dies the
//!   kernel closes the write end, the watchdog sees EOF and SIGKILLs the
//!   group. Hard-kill of ibis is covered; the only residual is the watchdog
//!   itself dying in the same instant (mitigated by the reconciliation
//!   follow-up).
//!
//! `graceful_stop` is the single stop policy for all spawned sidecars: ask
//! first (CTRL_BREAK on Windows when a console is shared, SIGINT to the
//! process group on unix so grandchildren receive it), then kill after the
//! grace window.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::process::Child;
    use windows_sys::Win32::System::Console::{
        GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, GetCurrentProcess,
    };

    /// Open job handle for the process lifetime, as isize for Send storage.
    /// Never closed manually: the kernel closing it at process exit is
    /// exactly the kill trigger.
    static JOB: Mutex<Option<isize>> = Mutex::new(None);
    static CREATED: AtomicBool = AtomicBool::new(false);
    static SELF_ATTACHED: AtomicBool = AtomicBool::new(false);

    /// Lazily create the kill-on-close job and attach this process, so
    /// children spawned afterwards inherit the membership. Idempotent; safe
    /// to call immediately before every child spawn.
    pub fn ensure_children_job() {
        if CREATED.load(Ordering::Relaxed) {
            return;
        }
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                let err = std::io::Error::last_os_error();
                eprintln!("process_tree: CreateJobObjectW failed: {err}");
                CREATED.store(true, Ordering::Relaxed); // do not retry spam
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                eprintln!("process_tree: SetInformationJobObject failed: {err}");
                windows_sys::Win32::Foundation::CloseHandle(job);
                CREATED.store(true, Ordering::Relaxed); // do not retry spam
                return;
            }
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                // Already inside a parent job (IDE/CI harness) that disallows
                // nesting: fall back to per-child assignment at spawn sites.
                let err = std::io::Error::last_os_error();
                eprintln!(
                    "process_tree: self-attach unavailable ({err}), falling back to per-child assignment"
                );
                *JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(job as isize);
                CREATED.store(true, Ordering::Relaxed);
                return;
            }
            *JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(job as isize);
            SELF_ATTACHED.store(true, Ordering::Relaxed);
            CREATED.store(true, Ordering::Relaxed);
            tracing::info!("process_tree: ibis owns its child tree (kill-on-close job)");
        }
    }

    /// True when this process is a job member with inheritance: spawned
    /// children join the job automatically. False means the per-child
    /// fallback must be used at spawn sites.
    pub fn children_job_active() -> bool {
        SELF_ATTACHED.load(Ordering::Relaxed)
    }

    /// Fallback when self-attach is unavailable: put a freshly spawned child
    /// into the held job so it still dies with this process. `raw_handle` is
    /// the child process HANDLE as isize.
    pub fn attach_child(raw_handle: isize) -> bool {
        let Some(job) = *JOB
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        else {
            return false;
        };
        unsafe {
            AssignProcessToJobObject(
                job as *mut core::ffi::c_void,
                raw_handle as *mut core::ffi::c_void,
            ) != 0
        }
    }

    /// Per-OS spawn configuration for sidecar children.
    ///
    /// Windows: lazily create/self-attach the kill-on-close job (so the spawn
    /// inherits the membership) and put the child in its own process group so
    /// the CTRL_BREAK graceful stop is deliverable when a console is shared.
    pub fn configure_spawn(cmd: &mut tokio::process::Command) -> std::io::Result<()> {
        ensure_children_job();
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        Ok(())
    }

    /// Shared stop policy: ask first (CTRL_BREAK to the child's process
    /// group — needs a shared console; on pipe-spawned children the event
    /// fails and the kill-after-grace path applies), then kill after the
    /// grace window.
    pub async fn graceful_stop(child: &mut Child, grace: Duration) -> std::io::Result<()> {
        if let Some(pid) = child.id() {
            let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            if sent != 0 {
                tracing::info!("process_tree: CTRL_BREAK_EVENT sent to the child process group (pid {pid})");
            }
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(status) => {
                status?;
                Ok(())
            }
            Err(_) => {
                tracing::info!("process_tree: grace expired, forcing kill");
                child.kill().await?;
                child.wait().await?;
                Ok(())
            }
        }
    }
}

#[cfg(windows)]
pub use imp::{
    attach_child, children_job_active, configure_spawn, ensure_children_job, graceful_stop,
};

#[cfg(unix)]
mod imp {
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "macos")]
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::process::Child;

    /// Flag that turns an ibis process into the parent-death watchdog for a
    /// sidecar already running in its own process group (macOS arm path).
    /// Parsed from the raw argv before any other startup work.
    pub const WATCHDOG_FLAG: &str = "-sidecar-watchdog";

    /// Per-OS spawn configuration for sidecar children.
    ///
    /// The child gets its own process group (`setpgid(0,0)`) so a graceful
    /// stop can signal the whole group. On Linux the child also registers
    /// `prctl(PR_SET_PDEATHSIG, SIGKILL)`: the kernel kills it when the
    /// creating parent dies, on any exit path. The fork→prctl race is closed
    /// by re-checking the parent right after `prctl` (man PR_SET_PDEATHSIG:
    /// if the parent is already dead at that point, no signal ever arrives).
    pub fn configure_spawn(cmd: &mut tokio::process::Command) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        let parent_pid = std::process::id() as libc::pid_t;
        // pre_exec is builder-style; closure errors surface at spawn().
        unsafe {
            cmd.as_std_mut().pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Close the fork→prctl race: if the parent died inside the
                    // window, no PDEATHSIG will ever be delivered — exit now.
                    if libc::getppid() != parent_pid {
                        libc::_exit(1);
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Shared stop policy: SIGINT to the child's own process group
    /// (`setpgid` at spawn — grandchildren such as ollama runners receive it
    /// too; falls back to the pid when the group is gone), then SIGKILL after
    /// the grace window.
    pub async fn graceful_stop(child: &mut Child, grace: Duration) -> std::io::Result<()> {
        if let Some(pid) = child.id() {
            let pgid = pid as i32;
            unsafe {
                if libc::killpg(pgid, libc::SIGINT) != 0 {
                    let _ = libc::kill(pgid, libc::SIGINT);
                }
            }
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(status) => {
                status?;
                Ok(())
            }
            Err(_) => {
                tracing::info!("process_tree: grace expired, forcing kill");
                if let Some(pid) = child.id() {
                    unsafe {
                        let _ = libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
                child.kill().await?;
                child.wait().await?;
                Ok(())
            }
        }
    }

    /// Write ends of all watchdog pipes, held open (leaked) for the process
    /// lifetime — the kernel closing them at exit is exactly the EOF trigger.
    #[cfg(target_os = "macos")]
    static WATCHDOG_WRITE_ENDS: Mutex<Vec<std::fs::File>> = Mutex::new(Vec::new());

    /// macOS parent-death watchdog: no kernel primitive exists, so the
    /// watchdog is a child ibis process holding the read end of a pipe whose
    /// write end this process keeps open for its lifetime. When ibis exits
    /// for any reason the kernel closes the write end; the watchdog sees EOF
    /// and SIGKILLs the sidecar process group. Must be called right after the
    /// spawn, from a tokio runtime context (the watchdog is spawned via tokio
    /// so the runtime reaps it when it exits).
    #[cfg(target_os = "macos")]
    pub fn arm_parent_death_watchdog(child_pid: u32) -> std::io::Result<()> {
        use std::os::unix::io::FromRawFd;

        // CLOEXEC keeps the write end out of every exec'd child: only ibis
        // holds it, so its death is the only way the pipe can close. (macOS
        // has no pipe2: create then set FD_CLOEXEC on both ends.)
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let exe = std::env::current_exe()?;
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args([WATCHDOG_FLAG, &child_pid.to_string()])
            .stdin(std::process::Stdio::from(unsafe {
                std::fs::File::from_raw_fd(fds[0])
            }))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.spawn()?; // Child dropped on purpose: tokio keeps reaping orphans.
        WATCHDOG_WRITE_ENDS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(unsafe { std::fs::File::from_raw_fd(fds[1]) });
        tracing::debug!("process_tree: parent-death watchdog armed (pgid {child_pid})");
        Ok(())
    }

    /// Watchdog body — `ibis -sidecar-watchdog <pgid>`: poll stdin (the pipe
    /// read end) every 500ms; EOF means ibis died → SIGKILL the group. A
    /// vanished group (sidecar stopped by other means) ends the loop quietly.
    pub fn run_parent_death_watchdog(pgid: i32) -> i32 {
        loop {
            let mut fds = [libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            }];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 500) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    return 0;
                }
                continue;
            }
            if n > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let mut buf = [0u8; 64];
                let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if r <= 0 {
                    // EOF: ibis is gone (any exit path) — terminate the tree.
                    unsafe {
                        let _ = libc::killpg(pgid, libc::SIGKILL);
                    }
                    return 0;
                }
            }
            unsafe {
                if libc::killpg(pgid, 0) == -1 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ESRCH) {
                        return 0;
                    }
                }
            }
        }
    }

    /// Parses `ibis -sidecar-watchdog <pgid>` out of the raw argv. `Some(code)`
    /// means the process must exit with that code after running the watchdog
    /// body; `None` means this is a normal ibis start.
    pub fn watchdog_exit_code(argv: &[String]) -> Option<i32> {
        let mut iter = argv.iter();
        while let Some(arg) = iter.next() {
            if arg == WATCHDOG_FLAG || arg == "--sidecar-watchdog" {
                let pgid = match iter.next().and_then(|v| v.parse::<i32>().ok()) {
                    Some(pgid) => pgid,
                    None => {
                        eprintln!("process_tree: {WATCHDOG_FLAG} requires a pgid argument");
                        return Some(2);
                    }
                };
                return Some(run_parent_death_watchdog(pgid));
            }
        }
        None
    }
}

#[cfg(unix)]
pub use imp::{
    configure_spawn, graceful_stop, run_parent_death_watchdog, watchdog_exit_code,
    WATCHDOG_FLAG,
};

/// Only macOS arms the watchdog (Linux is kernel-covered by PDEATHSIG).
#[cfg(target_os = "macos")]
pub use imp::arm_parent_death_watchdog;

#[cfg(not(any(windows, unix)))]
mod imp {
    use std::time::Duration;

    pub fn configure_spawn(_cmd: &mut tokio::process::Command) -> std::io::Result<()> {
        Ok(())
    }

    pub async fn graceful_stop(child: &mut tokio::process::Child, _grace: Duration) -> std::io::Result<()> {
        child.kill().await?;
        child.wait().await?;
        Ok(())
    }
}

#[cfg(not(any(windows, unix)))]
pub use imp::{configure_spawn, graceful_stop};

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment-dependent: in a sandboxed/CI harness the self-attach may
    /// legitimately fail; the contract is only "no panic, idempotent, and the
    /// reported state is consistent".
    #[cfg(windows)]
    #[test]
    fn ensure_children_job_is_idempotent() {
        ensure_children_job();
        ensure_children_job();
        let _ = children_job_active();
    }

    /// Unix round-trip: a child spawned with `configure_spawn` (own process
    /// group, PDEATHSIG on Linux) is terminated by `graceful_stop` within the
    /// grace window — `/bin/sleep` dies on SIGINT, so the graceful path is
    /// exercised, not the kill fallback.
    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_stop_terminates_group_child() {
        use std::time::Duration;

        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("30");
        configure_spawn(&mut cmd).expect("configure_spawn");
        let mut child = cmd.spawn().expect("spawn sleep");
        assert!(child.id().is_some());
        let start = std::time::Instant::now();
        graceful_stop(&mut child, Duration::from_millis(500))
            .await
            .expect("graceful stop");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "graceful stop took too long: {:?}",
            start.elapsed()
        );
        assert!(child.id().is_none(), "child must be reaped");
    }
}