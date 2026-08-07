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
//! * **Run Modes**: Runs the payload once and exits (`oneshot`), or stays resident and
//!   idle, running it once per `SIGUSR1` (`triggered`).
//! * **Signal Forwarding**: Forwards lifecycle signals (like `SIGTERM` or `SIGINT`) to the payload.
//! * **Orphan Reaping**: Automatically adopts and cleans up zombie subprocesses to prevent PID leaks.
//! * **Filesystem-Level Security**: Relies on read-only Unix permissions of the config file

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::consts::{SIGCHLD, SIGINT, SIGTERM, SIGUSR1};
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

/// How init runs the payload over the container's lifetime.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    /// Run the payload once at boot; init exits when the payload does.
    #[default]
    Oneshot,
    /// Stay resident and idle, running the payload once per `SIGUSR1`. Keeps
    /// transient work in a long-lived container an external scheduler drives.
    Triggered,
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
    /// How the payload is run. Defaults to [`Mode::Oneshot`].
    #[serde(default)]
    mode: Mode,
}

/// Reads and parses the ABI JSON configuration from the filesystem.
/// Any parsing failure halts PID 1 and cleanly terminates the container.
fn load_abi() -> Result<Process, String> {
    let content = std::fs::read_to_string(MAIN_ABI)
        .map_err(|e| format!("failed to read {MAIN_ABI}: {e}"))?;

    let abi: Abi = serde_json::from_str(&content)
        .map_err(|e| format!("invalid ABI JSON: {e}"))?;

    Ok(abi.process)
}

/// Reports a fatal startup error, applies the crash-loop delay, and exits.
fn fail(start_time: Instant, msg: String) -> ! {
    eprintln!("[init] {msg}");
    enforce_minimum_runtime(start_time);
    exit(1);
}

/// Collects the arguments passed to init itself, skipping `argv[0]`. As the entrypoint,
/// these are exactly a Compose `command:` / `docker run` trailing command / image `CMD`.
fn runtime_args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// Resolves the executable path and verifies its viability. Paths containing a slash are
/// checked on disk; bare names are left to `$PATH`. Args reach `execve` discretely, no shell.
fn resolve_exec(exec: &str, args: Vec<String>) -> Result<Vec<String>, String> {
    if exec.is_empty() {
        return Err("empty executable path".into());
    }

    // Null bytes are invalid in Unix path strings and can cause truncation issues.
    if exec.contains('\0') {
        return Err("invalid executable path: contains null byte".into());
    }

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

/// Reaps every outstanding zombie with a non-blocking `waitpid`, returning the terminal
/// status of `primary_pid` if it was among them. `None` means no payload is running.
fn reap_children(primary_pid: Option<Pid>) -> Option<WaitStatus> {
    let mut primary_status = None;
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            // No more processes have changed state; stop reaping.
            Ok(WaitStatus::StillAlive) => break,

            // A process has terminated; check if it was our primary child.
            Ok(status @ (WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _))) => {
                if Some(pid) == primary_pid {
                    primary_status = Some(status);
                }
            }

            // Another status change (stopped, continued); ignore and continue reaping.
            Ok(_) => continue,

            // No child processes left, or error; exit the reaping loop.
            Err(_) => break,
        }
    }
    primary_status
}

/// Renders a payload's terminal status for the log.
fn describe_status(status: WaitStatus) -> String {
    match status {
        WaitStatus::Exited(_, code) => format!("exit code {code}"),
        WaitStatus::Signaled(_, sig, _) => format!("killed by {}", sig.as_str()),
        other => format!("{other:?}"),
    }
}

/// Spawns the payload, optionally in a specified working directory. The `Child` handle
/// is dropped: as PID 1 we reap every child through [`reap_children`] anyway.
fn spawn_payload(cmd: &[String], cwd: Option<&str>) -> std::io::Result<Pid> {
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let child = command.spawn()?;
    Ok(Pid::from_raw(child.id() as i32))
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

/// Runs the payload once and returns when it has exited.
fn run_oneshot(cmd: &[String], cwd: Option<&str>, signals: &mut Signals, start_time: Instant) {
    let child_pid = spawn_payload(cmd, cwd)
        .unwrap_or_else(|e| fail(start_time, format!("failed to start process: {e}")));

    let mut shutting_down = false;

    for sig in signals.forever() {
        match sig {
            // Runtime requested a termination/interruption.
            SIGTERM | SIGINT => {
                eprintln!("[init] shutdown signal received");
                shutting_down = true;
                forward_signal(child_pid, Signal::SIGTERM);
            }

            // A child changed state (e.g., terminated or spawned a subprocess).
            SIGCHLD => {
                if let Some(status) = reap_children(Some(child_pid)) {
                    eprintln!("[init] payload exited ({})", describe_status(status));
                    return;
                }
            }

            SIGUSR1 => eprintln!("[init] SIGUSR1 ignored: mode is oneshot"),
            _ => {}
        }

        // If we are shutting down, check if our primary child process has exited yet.
        if shutting_down {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                // Child is still terminating; continue waiting for events.
                Ok(WaitStatus::StillAlive) => {}
                // Child exited or vanished; start final cleanup.
                _ => return,
            }
        }
    }
}

/// Idles until `SIGUSR1`, runs the payload, then idles again. Returns on `SIGTERM`/`SIGINT`
/// once any in-flight run has finished.
fn run_triggered(cmd: &[String], cwd: Option<&str>, signals: &mut Signals) {
    let mut current: Option<Pid> = None;
    let mut shutting_down = false;

    eprintln!("[init] idle; waiting for SIGUSR1 to run {}", cmd[0]);

    for sig in signals.forever() {
        match sig {
            // An external scheduler asked for a run.
            SIGUSR1 => {
                if shutting_down {
                    eprintln!("[init] trigger ignored: shutting down");
                } else if let Some(pid) = current {
                    // Overlapping runs would race on the payload's own data; skip instead.
                    eprintln!("[init] trigger ignored: run already in progress (pid {})", pid.as_raw());
                } else {
                    match spawn_payload(cmd, cwd) {
                        Ok(pid) => {
                            eprintln!("[init] trigger received; started payload (pid {})", pid.as_raw());
                            current = Some(pid);
                        }
                        // Stay resident: exiting here would restart-loop the container.
                        Err(e) => eprintln!("[init] trigger failed: could not start process: {e}"),
                    }
                }
            }

            // Runtime requested a termination/interruption.
            SIGTERM | SIGINT => {
                eprintln!("[init] shutdown signal received");
                shutting_down = true;
                match current {
                    Some(pid) => forward_signal(pid, Signal::SIGTERM),
                    None => return,
                }
            }

            // A child changed state; reaps orphans even while idle.
            SIGCHLD => {
                if let Some(status) = reap_children(current) {
                    eprintln!("[init] payload exited ({})", describe_status(status));
                    current = None;
                    if shutting_down {
                        return;
                    }
                    eprintln!("[init] idle; waiting for SIGUSR1");
                }
            }

            _ => {}
        }
    }
}

fn main() {
    // Record the start time of the init container system.
    let start_time = Instant::now();

    let process = load_abi()
        .unwrap_or_else(|e| fail(start_time, format!("ABI load failed: {e}")));

    // Create any requested directories before launching the payload, so applications
    // that do not create their own data dirs can operate on a mounted volume.
    if let Some(dirs) = &process.dirs {
        for dir in dirs {
            if let Err(e) = std::fs::create_dir_all(dir) {
                fail(start_time, format!("failed to create directory {dir}: {e}"));
            }
        }
    }

    // ABI baseline first, then whatever the runtime handed us. Appending rather than
    // replacing keeps an image's required flags intact.
    let mut args = process.args.clone().unwrap_or_default();
    args.extend(runtime_args());

    let cmd = resolve_exec(&process.exec, args)
        .unwrap_or_else(|e| fail(start_time, format!("resolve failed: {e}")));

    // Registered before the first spawn so a payload that exits immediately cannot
    // deliver SIGCHLD while we are not yet listening.
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGCHLD, SIGUSR1])
        .expect("signal setup failed");

    let cwd = process.cwd.as_deref();
    match process.mode {
        Mode::Oneshot => run_oneshot(&cmd, cwd, &mut signals, start_time),
        Mode::Triggered => run_triggered(&cmd, cwd, &mut signals),
    }

    // Final reap for anything the payload orphaned on its way out.
    reap_children(None);

    // Apply the rate-limiting delay if the total execution was too brief. Triggered mode
    // only ever exits on request, so delaying it just risks `docker stop` hitting SIGKILL.
    if process.mode == Mode::Oneshot {
        enforce_minimum_runtime(start_time);
    }

    eprintln!("[init] exit complete");
}
