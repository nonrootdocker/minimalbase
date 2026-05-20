use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use std::path::Path;
use std::process::{Command, exit};

const INIT: &str = "/app/init";
const MAIN: &str = "/app/main";

/// -------------------------
/// optional init hook
/// -------------------------
fn run_init() -> Result<(), String> {
    if !Path::new(INIT).exists() {
        return Ok(());
    }

    let status = Command::new(INIT)
        .status()
        .map_err(|e| format!("failed to run init: {e}"))?;

    if !status.success() {
        return Err("init failed".into());
    }

    Ok(())
}

/// -------------------------
/// forward signal
/// -------------------------
fn forward_signal(pid: Pid, sig: Signal) {
    let _ = signal::kill(pid, sig);
}

/// -------------------------
/// reap any zombies (safe PID1 hygiene)
/// -------------------------
fn reap_children() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// -------------------------
/// MAIN PID1
/// -------------------------
fn main() {
    // init (optional)
    if let Err(e) = run_init() {
        eprintln!("[init] init failed: {e}");
        exit(1);
    }

    // main is REQUIRED
    if !Path::new(MAIN).exists() {
        eprintln!("[init] /app/main is required but missing");
        exit(1);
    }

    let mut child = Command::new(MAIN)
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("[init] failed to start main: {e}");
            exit(1);
        });

    let child_pid = Pid::from_raw(child.id() as i32);

    // signal handling
    let mut signals = Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGCHLD,
    ]).expect("signal setup failed");

    let mut shutting_down = false;

    // PID1 loop
    for sig in signals.forever() {
        match sig {
            signal_hook::consts::SIGTERM | signal_hook::consts::SIGINT => {
                eprintln!("[init] shutdown signal received");
                shutting_down = true;

                forward_signal(child_pid, Signal::SIGTERM);
            }

            signal_hook::consts::SIGCHLD => {
                reap_children();
            }

            _ => {}
        }

        // exit when main is gone
        if shutting_down {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {}
                _ => break,
            }
        }
    }

    // cleanup
    forward_signal(child_pid, Signal::SIGTERM);
    reap_children();

    let _ = child.wait();

    eprintln!("[init] exit complete");
}
