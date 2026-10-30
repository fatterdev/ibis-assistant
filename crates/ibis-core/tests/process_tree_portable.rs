//! e2e for the PDEATHSIG parent-death guarantee (`process_tree::configure_spawn`,
//! point 5 of docs/planning/sidecar-lifecycle-plan.md). Linux-only: runs on the
//! CI ubuntu matrix. The test re-executes its own binary in "child role"; the
//! child role spawns a sleeper with the portable configure_spawn and
//! hard-exits — the kernel must then SIGKILL the sleeper (PDEATHSIG).

#![cfg(target_os = "linux")]

use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

/// Marker env that switches this test binary into child role.
const CHILD_ROLE_ENV: &str = "IBIS_PDEATHSIG_TEST_CHILD_ROLE";

#[test]
fn pdeathsig_child_dies_when_parent_exits() {
    if std::env::var(CHILD_ROLE_ENV).as_deref() == Ok("1") {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let spawned = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("/bin/sleep");
            cmd.arg("30");
            ibis_core::process_tree::configure_spawn(&mut cmd).expect("configure_spawn");
            let child = cmd.spawn().expect("spawn sleeper");
            child.id()
        });
        let pid = spawned.expect("child pid");
        println!("IBIS_PDEATHSIG_SPAWNED={pid}");
        std::process::exit(0); // hard exit → PDEATHSIG must kill the sleeper
    }

    let exe = std::env::current_exe().expect("test binary");
    let output = StdCommand::new(&exe)
        .args(["--exact", "pdeathsig_child_dies_when_parent_exits", "--nocapture"])
        .env(CHILD_ROLE_ENV, "1")
        .output()
        .expect("spawn child role");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pid = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("IBIS_PDEATHSIG_SPAWNED=")
                .map(|value| value.trim().parse::<u32>())
        })
        .transpose()
        .expect("parse marker")
        .expect("child role must print the spawned sleeper pid");

    // The child role has exited (output() waited for it); the sleeper must be
    // killed by the kernel (PDEATHSIG), not survive as an orphan.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && process_alive(pid) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !process_alive(pid),
        "sleeper {pid} survived its parent (PDEATHSIG not delivered)"
    );
}

/// A zombie still owns a /proc entry until reaped — treat it as dead (the
/// signal was delivered; reaping belongs to init).
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match stat.rsplit_once(')') {
            Some((_, rest)) => {
                let state = rest.split_whitespace().next().unwrap_or("?");
                state != "Z"
            }
            None => true,
        },
        Err(_) => false,
    }
}