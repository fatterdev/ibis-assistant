// Probe: canonical windows-sys definitions for the Job Object APIs.
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

fn main() {
    println!(
        "extended size={}",
        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()
    );
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        println!("job={job:?}");
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let r = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        println!("class9 r={r} err={:?}", std::io::Error::last_os_error());
        if r != 0 {
            // Full round-trip: attach this probe process and spawn a child
            // that must die when the probe hard-exits (job handle closes).
            let a = windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job,
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
            );
            println!("assign r={a}");
            let child = std::process::Command::new("cmd")
                .args(["/c", "ping -n 31 127.0.0.1 > nul"])
                .spawn()
                .expect("spawn child");
            println!("child pid={}", child.id());
            std::process::exit(9); // hard exit: job handle closes -> tree killed
        }
    }
}
