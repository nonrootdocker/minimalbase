//! # Container Init (PID 1)
//!
//! A minimal init system designed to run as PID 1 inside lightweight, non-shell
//! container environments.
//!
//! ## Core Responsibilities:
//! * **Configuration Loading**: Parses a read-only JSON specification to determine the payload.
//! * **Process Management**: Spawns and tracks the primary application process.
//! * **Signal Forwarding**: Forwards lifecycle signals (like `SIGTERM` or `SIGINT`) to the payload.
//! * **Orphan Reaping**: Automatically adopts and cleans up zombie subprocesses to prevent PID leaks.
//! * **Filesystem-Level Security**: Relies on read-only Unix permissions of the config file

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use std::path::Path;
use std::process::{Command, exit};

use serde::Deserialize;

/// The path to the immutable application configuration (ABI contract).
/// This file is expected to be owned by root and read-only inside the container.
const MAIN_ABI: &str = "/app/main";

/// Represents the structure of the JSON contract.
#[derive(Debug, Deserialize)]
struct Abi {
    process: Process,
}

/// Represents the execution target and its command line arguments.
#[derive(Debug, Deserialize)]
struct Process {
    exec: String,
    args: Option<Vec<String>>,
}

/// Reads and parses the ABI JSON configuration from the filesystem.
///
/// Because this file is loaded at container boot, any parsing failure
/// will halt PID 1 and cleanly terminate the container immediately.
fn load_abi() -> Result<(String, Vec<String>), String> {
    let content = std::fs::read_to_string(MAIN_ABI)
        .map_err(|e| format!("failed to read {MAIN_ABI}: {e}"))?;

    let abi: Abi = serde_json::from_str(&content)
        .map_err(|e| format!("invalid ABI JSON: {e}"))?;

    let args = abi.process.args.unwrap_or_default();

    Ok((abi.process.exec, args))
}

/// Resolves the executable path and verifies its viability.
///
/// This function supports:
/// * **Absolute/Relative Paths** (containing slashes): Verified physically on disk before spawning.
/// * **System Utilities** (names only): Automatically resolved by looking up the container's `$PATH`.
///
/// Security is maintained by ensuring path arguments are passed as discrete strings
/// directly to the `execve` system call, preventing shell-injection vectors.
fn resolve_exec(exec: &str, args: Vec<String>) -> Result<Vec<String>, String> {
    if exec.is_empty() {
        return Err("empty executable path".into());
    }

    // Null bytes are invalid in Unix path strings and can cause truncation issues.
    if exec.contains('\0') {
        return Err("invalid executable path: contains null byte".into());
    }

    // If the path contains a slash, verify its existence physically on disk.
    // If it is a bare name (e.g., "ls"), we skip this check and let the OS resolve it via $PATH.
    if exec.contains('/') {
        if !Path::new(exec).exists() {
            return Err(format!("executable not found at path: {exec}"));
        }
    }

    let mut cmd = vec![exec.to_string()];
    cmd.extend(args);
    Ok(cmd)
}

/// Safely forwards a signal to a specific process ID.
fn forward_signal(pid: Pid, sig: Signal) {
    let _ = signal::kill(pid, sig);
}

/// Reaps any orphaned or zombie processes in the container.
///
/// In Unix systems, when a process dies, it remains in the process table as a "zombie"
/// until its parent reads its exit status. If the parent dies first, the process is
/// adopted by PID 1.
///
/// This function cleans up all outstanding zombie processes using `waitpid` with `WNOHANG`
/// so that the call is non-blocking and does not stall the main thread.
fn reap_children() {
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            // No more processes have changed state; stop reaping for now.
            Ok(WaitStatus::StillAlive) => break,
            // A child was successfully reaped; continue the loop to check for others.
            Ok(_) => continue,
            // No child processes left, or an interrupt occurred; exit the reaping loop.
            Err(_) => break,
        }
    }
}

fn main() {
    // Load the ABI configuration.
    let (exec, args) = match load_abi() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[init] ABI load failed: {e}");
            exit(1);
        }
    };

    // Resolve the target binary path.
    let cmd = match resolve_exec(&exec, args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[init] resolve failed: {e}");
            exit(1);
        }
    };

    // Spawn the primary child application.
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("[init] failed to start process: {e}");
            exit(1);
        });

    let child_pid = Pid::from_raw(child.id() as i32);

    // Initialize the signal-handling pipeline.
    // We register for SIGTERM/SIGINT (for graceful stops) and SIGCHLD (child status updates).
    let mut signals = Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGCHLD,
    ]).expect("signal setup failed");

    let mut shutting_down = false;

    // The Main Event Monitoring Loop.
    for sig in signals.forever() {
        match sig {
            // Runtime requested a termination/interruption.
            signal_hook::consts::SIGTERM | signal_hook::consts::SIGINT => {
                eprintln!("[init] shutdown signal received");
                shutting_down = true;
                
                // Forward the termination request to the primary child process.
                forward_signal(child_pid, Signal::SIGTERM);
            }
            
            // A child changed state (e.g., terminated or spawned a subprocess).
            signal_hook::consts::SIGCHLD => {
                reap_children();
            }
            _ => {}
        }

        // If we are shutting down, check if our primary child process has exited yet.
        if shutting_down {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                // Child is still terminating; continue waiting for events.
                Ok(WaitStatus::StillAlive) => {}
                // Child exited or vanished; break loop and start final cleanup.
                _ => break,
            }
        }
    }

    // Graceful Cleanup.
    // Ensure the primary child has been signaled to stop, then perform one final reap.
    forward_signal(child_pid, Signal::SIGTERM);
    reap_children();
    
    // Explicitly wait on the primary child to release its exit code.
    let _ = child.wait();

    eprintln!("[init] exit complete");
}
