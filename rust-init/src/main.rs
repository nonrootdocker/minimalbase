//! # Container Init (PID 1)
//!
//! A minimal init system designed to run as PID 1 inside lightweight, non-shell
//! container environments.
//!
//! ## Core Responsibilities:
//! * **Configuration Loading**: Parses a read-only JSON specification to determine the payload.
//! * **Runtime Arguments**: Appends any arguments given to init itself (the container's
//!   `command:` / `CMD`) to the ABI-declared argument list.
//! * **Process Management**: Spawns and tracks the primary application process.
//! * **Signal Forwarding**: Forwards lifecycle signals (like `SIGTERM` or `SIGINT`) to the payload.
//! * **Orphan Reaping**: Automatically adopts and cleans up zombie subprocesses to prevent PID leaks.
//! * **Filesystem-Level Security**: Relies on read-only Unix permissions of the config file

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;
use std::time::{Instant, Duration};

use std::path::Path;
use std::process::{Command, exit};

use serde::Deserialize;

/// The path to the immutable application configuration (ABI contract).
/// This file is expected to be owned by root and read-only inside the container.
const MAIN_ABI: &str = "/app/main";

/// Target minimum runtime to prevent rapid crash loops.
const MINIMUM_LIFETIME_SECS: u64 = 120;

/// Represents the structure of the JSON contract.
#[derive(Debug, Deserialize)]
struct Abi {
    process: Process,
}

/// Represents the execution target and its command line arguments.
#[derive(Debug, Deserialize)]
struct Process {
    exec: String,
    /// Baseline arguments baked into the image. Arguments supplied at runtime
    /// (see [`runtime_args`]) are appended to these.
    args: Option<Vec<String>>,
    /// Optional working directory to `chdir` into before exec. Useful for
    /// applications that read/write configuration relative to the cwd.
    cwd: Option<String>,
    /// Optional directories to create (`mkdir -p`) before exec. Useful for
    /// applications that do not create their own data dirs on a mounted volume.
    dirs: Option<Vec<String>>,
}

/// Reads and parses the ABI JSON configuration from the filesystem.
///
/// Because this file is loaded at container boot, any parsing failure
/// will halt PID 1 and cleanly terminate the container immediately.
fn load_abi() -> Result<Process, String> {
    let content = std::fs::read_to_string(MAIN_ABI)
        .map_err(|e| format!("failed to read {MAIN_ABI}: {e}"))?;

    let abi: Abi = serde_json::from_str(&content)
        .map_err(|e| format!("invalid ABI JSON: {e}"))?;

    Ok(abi.process)
}

/// Collects the arguments passed to init itself, skipping `argv[0]`.
///
/// Because init runs as the container's entrypoint, these are exactly the words
/// of a Compose `command:` (or a `docker run` trailing command / image `CMD`).
/// Surfacing them lets a deployment extend the payload's command line without
/// rebuilding the image, while `exec` stays pinned by the read-only ABI.
fn runtime_args() -> Vec<String> {
    std::env::args().skip(1).collect()
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
fn reap_children(primary_pid: Pid) -> bool {
    let mut primary_exited = false;
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            // No more processes have changed state; stop reaping.
            Ok(WaitStatus::StillAlive) => break,
            
            // A process has exited; check if it was our primary child.
            Ok(WaitStatus::Exited(pid, _)) => {
                if pid == primary_pid {
                    primary_exited = true;
                }
            }
            Ok(WaitStatus::Signaled(pid, _, _)) => {
                if pid == primary_pid {
                    primary_exited = true;
                }
            }
            
            // Another status change (stopped, continued); ignore and continue reaping.
            Ok(_) => continue,
            
            // No child processes left, or error; exit the reaping loop.
            Err(_) => break,
        }
    }
    primary_exited
}

/// Computes the elapsed time since startup and sleeps for the remainder
/// if the elapsed time is less than the defined threshold.
fn enforce_minimum_runtime(start_time: Instant) {
    let elapsed = start_time.elapsed();
    let threshold = Duration::from_secs(MINIMUM_LIFETIME_SECS);

    if elapsed < threshold {
        let remaining = threshold - elapsed;
        eprintln!(
            "[init] container lifetime was only {:.2?}. Rate-limiting restart; sleeping for remaining {:.2?}...",
            elapsed, remaining
        );
        std::thread::sleep(remaining);
    }
}

fn main() {
    // Record the start time of the init container system.
    let start_time = Instant::now();

    // Load the ABI configuration.
    let process = match load_abi() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[init] ABI load failed: {e}");
            enforce_minimum_runtime(start_time);
            exit(1);
        }
    };

    // Create any requested directories before launching the payload. This lets
    // applications that do not create their own data dirs operate on a mounted
    // volume (created relative to the running user's permissions).
    if let Some(dirs) = &process.dirs {
        for dir in dirs {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("[init] failed to create directory {dir}: {e}");
                enforce_minimum_runtime(start_time);
                exit(1);
            }
        }
    }

    // Build the payload's argument list: the ABI baseline first, then whatever
    // the runtime handed us. Appending (rather than replacing) means an image
    // that declares no `args` yields full control to the deployment, while an
    // image that does declare them keeps its required flags intact.
    let mut args = process.args.clone().unwrap_or_default();
    args.extend(runtime_args());

    // Resolve the target binary path.
    let cmd = match resolve_exec(&process.exec, args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[init] resolve failed: {e}");
            enforce_minimum_runtime(start_time);
            exit(1);
        }
    };

    // Spawn the primary child application, optionally in a specified working dir.
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    if let Some(cwd) = &process.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("[init] failed to start process: {e}");
            enforce_minimum_runtime(start_time);
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
                // If the primary process exited, break out of the event loop immediately.
                if reap_children(child_pid) {
                    break;
                }                
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
    reap_children(child_pid);
    
    // Explicitly wait on the primary child to release its exit code.
    let _ = child.wait();
    eprintln!("[init] process exited");

    // Apply the rate-limiting delay if the total execution was too brief.
    enforce_minimum_runtime(start_time);

    eprintln!("[init] exit complete");
}
