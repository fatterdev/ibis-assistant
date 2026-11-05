//! Sidecar ledger + startup reconciliation — Layer 3 of the sidecar
//! lifecycle plan (`docs/planning/sidecar-lifecycle-plan.md`), decision T4 of
//! the F4 team-server design.
//!
//! Layers 1–2 cover parent death at spawn time (Windows job object, Linux
//! PDEATHSIG, macOS watchdog pipe-EOF). The residual edges — a watchdog
//! killed in the same instant as its parent, grandchildren on Linux, any
//! machine without kernel-kill — leave orphans that only a *later* run can
//! clean: this module makes the cleanup *eventually complete*.
//!
//! Contract:
//! - Every sidecar spawn records a [`SidecarEntry`] in a JSON ledger inside
//!   the directory that binds the sidecar (the parent of the SurrealDB data
//!   file): per-sidecar `pid` + process **start-time** + `port`, plus the
//!   owner identity (`owner_pid` + `owner_start_time`).
//! - Before a new sidecar starts, [`reconcile`] drops dead entries, keeps
//!   live entries whose owner is still alive (attach tolerance — a sibling
//!   runtime or daemon may legitimately own them) and **terminates** live
//!   entries whose owner is gone: those are orphans, by definition.
//! - The start-time comparison excludes PID reuse on both sides (sidecar and
//!   owner): Windows compares `GetProcessTimes` creation FILETIME, Linux
//!   compares `/proc/<pid>/stat` field 22 (starttime); macOS has no /proc,
//!   so it degrades to `kill(pid, 0)` with the residual documented below.
//!
//! Residuals (declared):
//! - macOS: no start-time source in this layer — a reused PID that still
//!   answers `kill(pid, 0)` reads as alive, and a reused owner PID could
//!   read as a live owner, postponing the cleanup to the next restart.
//!   Never a wrong kill: a reused PID never *creates* staleness.
//! - The ledger file is written without cross-process locking: two runtimes
//!   racing on the same data dir may lose an entry, which only postpones the
//!   cleanup (the stale sidecar is caught on the next restart). One runtime
//!   per data dir is the supported topology (the F4 daemon makes it
//!   structural).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ledger file name, beside the sidecar data file.
pub const LEDGER_FILE: &str = "sidecar_ledger.json";

/// One registered sidecar process and its owner.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SidecarEntry {
    /// Sidecar kind (for example `surreal`).
    pub name: String,
    /// Sidecar pid.
    pub pid: u32,
    /// Sidecar process group (unix `setpgid(0,0)` makes it the pid; used to
    /// terminate grandchildren together with the sidecar).
    pub pgid: u32,
    /// Sidecar process start-time (platform clock: Windows FILETIME, Linux
    /// `/proc` starttime ticks; 0 where unavailable — macOS).
    pub start_time: u64,
    /// Port the sidecar binds (informational binding record).
    pub port: u16,
    /// Absolute path of the bound data file (the SurrealDB data file).
    pub data_file: String,
    /// Owner ibis pid at spawn time.
    pub owner_pid: u32,
    /// Owner ibis process start-time (same clock as [`Self::start_time`]).
    pub owner_start_time: u64,
}

impl SidecarEntry {
    /// Builds the ledger entry for a freshly spawned child. The child start
    /// time is read from the OS (`raw_handle` on Windows, `/proc` on Linux);
    /// a failed read degrades to 0 exactly like the macOS case.
    pub fn for_child(
        name: &str,
        pid: u32,
        port: u16,
        data_file: &Path,
        raw_handle: Option<isize>,
    ) -> SidecarEntry {
        SidecarEntry {
            name: name.to_string(),
            pid,
            pgid: process_group(pid).unwrap_or(pid),
            start_time: child_start_time(pid, raw_handle).unwrap_or(0),
            port,
            data_file: data_file.display().to_string(),
            owner_pid: std::process::id(),
            owner_start_time: self_start_time(),
        }
    }
}

/// Ledger path inside the sidecar data directory.
pub fn ledger_path(dir: &Path) -> std::path::PathBuf {
    dir.join(LEDGER_FILE)
}

/// Reads the ledger. A missing or corrupt file reads as empty — the ledger
/// is an optimization over reconciliation, never a gate.
pub fn load(dir: &Path) -> Vec<SidecarEntry> {
    let Ok(bytes) = std::fs::read(ledger_path(dir)) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<SidecarEntry>>(&bytes) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!("sidecar ledger: corrupt {}, ignoring ({err})", LEDGER_FILE);
            Vec::new()
        }
    }
}

/// Writes the ledger (best effort: failures are logged, never fatal).
pub fn save(dir: &Path, entries: &[SidecarEntry]) {
    if let Err(err) = std::fs::create_dir_all(dir) {
        tracing::warn!("sidecar ledger: cannot create {}: {err}", dir.display());
        return;
    }
    match serde_json::to_vec_pretty(entries) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(ledger_path(dir), bytes) {
                tracing::warn!("sidecar ledger: write failed: {err}");
            }
        }
        Err(err) => tracing::warn!("sidecar ledger: serialize failed: {err}"),
    }
}

/// Appends an entry, preserving unrelated and foreign-live entries (attach
/// tolerance: a sibling runtime's sidecar must never be dropped from the
/// ledger by this process).
pub fn register(dir: &Path, entry: &SidecarEntry) {
    let mut entries = load(dir);
    entries.push(entry.clone());
    save(dir, &entries);
    tracing::info!(
        "sidecar ledger: registered {} pid {} (port {}) in {}",
        entry.name,
        entry.pid,
        entry.port,
        dir.display()
    );
}

/// Removes the entry for a sidecar this process stopped gracefully.
pub fn unregister(dir: &Path, name: &str, pid: u32) {
    let mut entries = load(dir);
    let before = entries.len();
    entries.retain(|e| !(e.name == name && e.pid == pid));
    if entries.len() != before {
        save(dir, &entries);
    }
}

/// True when `pid` is running **and** was created at `start_time` (0 skips
/// the start-time check — the macOS degradation). PID reuse is excluded by
/// the comparison, never by pid alone.
pub fn process_alive(pid: u32, start_time: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    process_alive_raw(pid, start_time)
}

/// Liveness of a recorded sidecar.
pub fn entry_alive(entry: &SidecarEntry) -> bool {
    process_alive(entry.pid, entry.start_time)
}

/// Liveness of the recorded owner process.
pub fn owner_alive(entry: &SidecarEntry) -> bool {
    process_alive(entry.owner_pid, entry.owner_start_time)
}

/// Start time of this ibis process (the owner identity written at spawn).
pub fn self_start_time() -> u64 {
    process_start_time(std::process::id()).unwrap_or(0)
}

/// Layer 3 reconciliation pass: prune dead entries, keep live entries with a
/// live owner (foreign sidecars — attach tolerance), **terminate** live
/// entries whose owner is gone (orphans from hard kills on the residual
/// edges layers 1–2 cannot cover). Returns the survivors.
pub async fn reconcile(dir: &Path, grace: Duration) -> Vec<SidecarEntry> {
    let mut survivors = Vec::new();
    for entry in load(dir) {
        if !entry_alive(&entry) {
            tracing::debug!(
                "sidecar ledger: pruning dead {} pid {}",
                entry.name,
                entry.pid
            );
            continue;
        }
        if owner_alive(&entry) {
            tracing::debug!(
                "sidecar ledger: {} pid {} still owned by live ibis pid {} — kept",
                entry.name,
                entry.pid,
                entry.owner_pid
            );
            survivors.push(entry);
            continue;
        }
        tracing::info!(
            "sidecar reconciliation: stale {} pid {} (owner ibis {} gone) — terminating before a new one starts",
            entry.name,
            entry.pid,
            entry.owner_pid
        );
        terminate_impl(&entry, grace).await;
        if entry_alive(&entry) {
            tracing::warn!(
                "sidecar reconciliation: {} pid {} survived termination — keeping the entry for the next restart",
                entry.name,
                entry.pid
            );
            survivors.push(entry);
        }
    }
    save(dir, &survivors);
    survivors
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, TerminateProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    /// Not exported by windows-sys 0.60 under this name (daemon.rs keeps the
    /// same literal).
    const STILL_ACTIVE: u32 = 259;

    fn filetime_to_u64(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    fn creation_time(handle: *mut core::ffi::c_void) -> Option<u64> {
        unsafe {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
                return None;
            }
            Some(filetime_to_u64(creation))
        }
    }

    /// Creation FILETIME of a pid (`OpenProcess` + `GetProcessTimes`); the
    /// terminated-but-not-reaped Windows edge still reports the original
    /// creation time, which keeps the start-time match honest.
    pub fn process_start_time(pid: u32) -> Option<u64> {
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let creation = creation_time(handle);
            CloseHandle(handle);
            creation
        }
    }

    pub fn pid_exists(pid: u32) -> bool {
        process_start_time(pid).is_some()
    }

    /// Windows liveness: the process object can outlive termination while
    /// handles are open (the tokio `Child` of the previous owner, for
    /// example), and such a corpse still answers `OpenProcess` with its
    /// original creation time — so the exit code (`STILL_ACTIVE` check)
    /// decides first, then the creation-time comparison excludes PID reuse.
    /// An un-openable pid reads as dead (safe direction: no kill).
    pub fn process_alive_raw(pid: u32, start_time: u64) -> bool {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            ) as isize;
            if handle == 0 {
                return false;
            }
            let mut exit_code: u32 = 0;
            GetExitCodeProcess(handle as *mut core::ffi::c_void, &mut exit_code);
            if exit_code != STILL_ACTIVE {
                CloseHandle(handle as *mut core::ffi::c_void);
                return false; // terminated, however many handles remain
            }
            let creation = creation_time(handle as *mut core::ffi::c_void);
            CloseHandle(handle as *mut core::ffi::c_void);
            match creation {
                Some(actual) => actual == start_time || start_time == 0,
                None => true, // cannot compare → assume alive, never wrong-kill
            }
        }
    }

    /// Start time of a freshly spawned child through its handle when
    /// available (`tokio::process::Child::raw_handle`), by pid otherwise.
    pub fn child_start_time(pid: u32, raw_handle: Option<isize>) -> Option<u64> {
        match raw_handle {
            Some(handle) => creation_time(handle as *mut core::ffi::c_void),
            None => process_start_time(pid),
        }
    }

    pub fn process_group(pid: u32) -> Option<u32> {
        // Windows has no readable process-group notion here; the recorded
        // pgid stays equal to the pid.
        Some(pid)
    }

    /// CTRL_BREAK first (delivered when a console happens to be shared),
    /// poll the exit code for the grace window, then TerminateProcess. The
    /// handle must carry a query right too: without it `GetExitCodeProcess`
    /// fails, the stale exit-code 0 reads as "already exited" and the
    /// termination silently never happens. The raw handle is kept as `isize`
    /// (Send) because it lives across the grace-loop await points.
    pub async fn terminate_impl(entry: &super::SidecarEntry, grace: Duration) {
        let pid = entry.pid;
        unsafe {
            let handle = OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            ) as isize;
            if handle == 0 {
                return; // gone between the liveness check and now
            }
            let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let mut exit_code: u32 = 0;
                GetExitCodeProcess(handle as *mut core::ffi::c_void, &mut exit_code);
                if exit_code != STILL_ACTIVE {
                    CloseHandle(handle as *mut core::ffi::c_void);
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            TerminateProcess(handle as *mut core::ffi::c_void, 1);
            CloseHandle(handle as *mut core::ffi::c_void);
        }
    }
}

#[cfg(windows)]
pub use imp::{
    child_start_time, pid_exists, process_alive_raw, process_group, process_start_time,
    terminate_impl,
};

#[cfg(unix)]
mod imp {
    use std::time::Duration;

    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    const EPERM: i32 = 1;

    /// `/proc/<pid>/stat` after the comm field (fields 3+): pgrp is index 2,
    /// starttime is index 19 (field 22). Linux only — macOS has no /proc.
    #[cfg(target_os = "linux")]
    fn read_stat_fields(pid: u32) -> Option<(u32, u64)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = text.rsplit_once(')')?.1;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        let pgrp = fields.get(2)?.parse::<u32>().ok()?;
        let start = fields.get(19)?.parse::<u64>().ok()?;
        Some((pgrp, start))
    }

    #[cfg(target_os = "linux")]
    pub fn process_start_time(pid: u32) -> Option<u64> {
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        read_stat_fields(pid).map(|(_, start)| start)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn process_start_time(_pid: u32) -> Option<u64> {
        // macOS: no start-time source in this layer (residual documented in
        // the module header).
        None
    }

    pub fn pid_exists(pid: u32) -> bool {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // kill(pid, 0): 0 = alive; EPERM also implies alive (an unprobeable
        // process must not be killed — the start-time check is skipped by
        // the caller on this path anyway).
        let errno = unsafe { kill(pid as i32, 0) };
        errno == 0 || std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }

    pub fn child_start_time(pid: u32, _raw_handle: Option<isize>) -> Option<u64> {
        process_start_time(pid)
    }

    /// Unix liveness: the `/proc` start-time match when the source exists
    /// (Linux), existence-only otherwise (macOS degradation).
    pub fn process_alive_raw(pid: u32, start_time: u64) -> bool {
        match process_start_time(pid) {
            Some(actual) => actual == start_time || start_time == 0,
            None => pid_exists(pid),
        }
    }

    pub fn process_group(pid: u32) -> Option<u32> {
        #[cfg(target_os = "linux")]
        if pid != 0 && pid <= i32::MAX as u32 {
            if let Some((pgrp, _)) = read_stat_fields(pid) {
                return Some(pgrp);
            }
        }
        // Outside Linux (or the stat raced away): the spawn contract
        // (`setpgid(0,0)`) makes the group equal to the pid.
        Some(pid)
    }

    fn alive(pid: u32) -> bool {
        pid_exists(pid)
    }

    /// SIGTERM to the process group (grandchildren included), poll for the
    /// grace window, then SIGKILL the group (and the pid, should the group
    /// have vanished around the sidecar).
    pub async fn terminate_impl(entry: &super::SidecarEntry, grace: Duration) {
        let pid = entry.pid;
        let pgid = if entry.pgid == 0 { pid } else { entry.pgid };
        unsafe {
            let _ = kill(pgid as i32, SIGTERM);
            let deadline = tokio::time::Instant::now() + grace;
            while alive(pid) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if alive(pid) {
                let _ = kill(pgid as i32, SIGKILL);
                let _ = kill(pid as i32, SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
pub use imp::{
    child_start_time, pid_exists, process_alive_raw, process_group, process_start_time,
    terminate_impl,
};

#[cfg(not(any(windows, unix)))]
mod imp {
    pub fn process_start_time(_pid: u32) -> Option<u64> {
        None
    }

    pub fn pid_exists(_pid: u32) -> bool {
        false
    }

    pub fn child_start_time(_pid: u32, _raw_handle: Option<isize>) -> Option<u64> {
        None
    }

    pub fn process_group(_pid: u32) -> Option<u32> {
        None
    }

    pub fn process_alive_raw(_pid: u32, _start_time: u64) -> bool {
        false
    }

    pub async fn terminate_impl(_entry: &super::SidecarEntry, _grace: std::time::Duration) {}
}

#[cfg(not(any(windows, unix)))]
pub use imp::{
    child_start_time, pid_exists, process_alive_raw, process_group, process_start_time,
    terminate_impl,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tempdir_ledger() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn sample_entry(pid: u32, port: u16) -> SidecarEntry {
        SidecarEntry {
            name: "surreal".to_string(),
            pid,
            pgid: pid,
            start_time: 42,
            port,
            data_file: "/tmp/db".to_string(),
            owner_pid: 777,
            owner_start_time: 43,
        }
    }

    #[test]
    fn ledger_round_trips_and_tolerates_corruption() {
        let (_guard, path) = tempdir_ledger();
        assert!(load(&path).is_empty(), "missing file reads empty");

        let entries = vec![sample_entry(11, 8000), sample_entry(12, 8001)];
        save(&path, &entries);
        assert_eq!(load(&path), entries);

        std::fs::write(ledger_path(&path), b"{not json").expect("corrupt");
        assert!(load(&path).is_empty(), "corrupt file reads empty");
    }

    #[test]
    fn register_appends_and_unregister_removes_only_the_target() {
        let (_guard, path) = tempdir_ledger();
        register(&path, &sample_entry(11, 8000));
        register(&path, &sample_entry(12, 8000));
        assert_eq!(load(&path).len(), 2, "append keeps foreign entries");

        unregister(&path, "surreal", 11);
        let left = load(&path);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pid, 12);

        // Removing an absent entry is a no-op that does not rewrite the file.
        unregister(&path, "surreal", 11);
        assert_eq!(load(&path).len(), 1);
    }

    #[test]
    fn liveness_excludes_pid_reuse_by_start_time() {
        let self_pid = std::process::id();
        let own_start = process_start_time(self_pid).expect("self start time readable");
        assert!(own_start > 0 || cfg!(not(any(windows, target_os = "linux"))));

        // Same pid, matching start time → alive.
        assert!(process_alive(self_pid, own_start));
        // Same pid, mismatched start time → a different process now sits on
        // the pid (reuse simulation) → not alive.
        assert!(!process_alive(self_pid, own_start.wrapping_add(12345)));
        // Bogus pid → not alive whatever the recorded start time.
        assert!(!process_alive(4294967295, 0));
        assert!(!process_alive(0, 0));
    }

    #[test]
    fn owner_and_entry_alive_wire_the_recorded_identity() {
        let self_pid = std::process::id();
        let own_start = process_start_time(self_pid).expect("self start time readable");
        let mut entry = sample_entry(self_pid, 8000);
        entry.start_time = own_start;
        entry.owner_pid = self_pid;
        entry.owner_start_time = own_start;
        assert!(entry_alive(&entry));
        assert!(owner_alive(&entry));

        entry.owner_start_time = own_start.wrapping_add(1);
        assert!(
            !owner_alive(&entry),
            "mismatched owner start time reads dead"
        );
    }

    /// e2e of the stale path with real processes: a recorded sidecar whose
    /// owner is gone is terminated by [`reconcile`], dead entries are pruned,
    /// and the ledger is rewritten with the survivors only.
    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn reconcile_terminates_stale_and_rewrites_the_ledger() {
        let sleeper = spawn_sleeper();
        let stale_pid = sleeper.id().expect("sleeper pid");
        let stale_start = process_start_time(stale_pid).expect("sleeper start time");

        let dead_owner_pid = spawn_exiting_process();

        let (_guard, path) = tempdir_ledger();
        let mut stale = sample_entry(stale_pid, 8000);
        stale.start_time = stale_start;
        stale.owner_pid = dead_owner_pid;
        stale.owner_start_time = 1; // any value: the owner no longer exists
        let mut dead = sample_entry(4242, 8001);
        dead.name = "ollama".to_string(); // a recorded pid that no longer exists
        save(&path, &[stale.clone(), dead]);

        let survivors = reconcile(&path, Duration::from_secs(2)).await;
        assert!(
            survivors.is_empty(),
            "the stale entry was terminated and the dead one pruned: {survivors:?}"
        );
        assert!(load(&path).is_empty(), "ledger rewritten with survivors");

        // The stale sidecar is really gone.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while entry_alive(&stale) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!entry_alive(&stale), "stale sidecar must be terminated");
    }

    /// Foreign-live preservation: an entry whose owner is this process
    /// survives reconcile untouched (attach tolerance for sibling runtimes),
    /// and reconcile is idempotent on the same state.
    #[cfg(any(windows, unix))]
    #[tokio::test]
    async fn reconcile_keeps_entries_owned_by_a_live_ibis() {
        let mut sleeper = spawn_sleeper();
        let pid = sleeper.id().expect("sleeper pid");
        let start = process_start_time(pid).expect("sleeper start time");

        let (_guard, path) = tempdir_ledger();
        let mut entry = sample_entry(pid, 8000);
        entry.start_time = start;
        entry.owner_pid = std::process::id();
        entry.owner_start_time = self_start_time();
        save(&path, &[entry.clone()]);

        let survivors = reconcile(&path, Duration::from_secs(2)).await;
        assert_eq!(
            survivors,
            vec![entry.clone()],
            "foreign-live entry preserved"
        );
        assert!(entry_alive(&entry), "the kept sidecar must stay alive");

        let again = reconcile(&path, Duration::from_secs(2)).await;
        assert_eq!(again, vec![entry.clone()], "reconcile is idempotent");

        // Cleanup: stop the sleeper through the same terminate path.
        terminate_impl(&entry, Duration::from_secs(2)).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while entry_alive(&entry) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!entry_alive(&entry), "cleanup terminated the sleeper");
        sleeper.wait().await.expect("reap the cleaned-up sleeper");
    }

    /// Dead entries are pruned without any termination attempt.
    #[tokio::test]
    async fn reconcile_prunes_dead_entries() {
        let (_guard, path) = tempdir_ledger();
        let mut entry = sample_entry(4294967295, 8000);
        entry.start_time = 7;
        save(&path, &[entry.clone()]);

        let survivors = reconcile(&path, Duration::from_millis(200)).await;
        assert!(survivors.is_empty(), "dead entry pruned");
        assert!(load(&path).is_empty());
    }

    #[cfg(any(windows, unix))]
    fn spawn_sleeper() -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" });
        if cfg!(windows) {
            cmd.args(["/C", "ping", "-n", "30", "127.0.0.1"]);
        } else {
            cmd.arg("30");
        }
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::process_tree::configure_spawn(&mut cmd).expect("configure spawn");
        cmd.spawn().expect("spawn sleeper")
    }

    /// Spawns a process that exits immediately and returns its (now dead)
    /// pid, reaped — the "owner gone" fixture.
    #[cfg(any(windows, unix))]
    fn spawn_exiting_process() -> u32 {
        let mut cmd = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" });
        if cfg!(windows) {
            cmd.args(["/C", "exit", "0"]);
        }
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn exiting process");
        let pid = child.id();
        let _ = child.wait(); // reap: the pid is dead when this returns
        pid
    }
}
