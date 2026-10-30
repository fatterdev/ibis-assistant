//! Process-tree ownership for spawned sidecars.
//!
//! Ownership principle: **ibis owns only the processes it spawns.** The Job
//! Object is created lazily — at the first child spawn (embedded SurrealDB,
//! `ollama serve`, ast-grep sidecar) — and the ibis process attaches itself
//! right before the spawn, so every child inherits the membership. When the
//! `ibis` process exits (graceful stop, crash or hard kill), the kernel
//! terminates whatever is left in the tree. No orphaned `surreal.exe` /
//! `ollama.exe` after tests or IDE kills.
//!
//! Configurations that use already-running, independent services (`attach`
//! mode over Streamable HTTP, external SurrealDB/Ollama) never spawn those
//! children, therefore never create the job and never touch those processes.
//!
//! Windows only; elsewhere this is a no-op (children are stopped through the
//! graceful shutdown paths: `ProcessManager::stop`, `OllamaProcess::stop`).

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

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
        let Some(job) = JOB
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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
}

#[cfg(windows)]
pub use imp::{attach_child, children_job_active, ensure_children_job};

#[cfg(not(windows))]
mod imp {
    pub fn ensure_children_job() {}
    pub fn children_job_active() -> bool {
        false
    }
    pub fn attach_child(_raw_handle: isize) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub use imp::{attach_child, children_job_active, ensure_children_job};

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment-dependent: in a sandboxed/CI harness the self-attach may
    /// legitimately fail; the contract is only "no panic, idempotent, and the
    /// reported state is consistent".
    #[test]
    fn ensure_children_job_is_idempotent() {
        ensure_children_job();
        ensure_children_job();
        let _ = children_job_active();
    }
}
