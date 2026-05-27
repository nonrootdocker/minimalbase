use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

/// -------------------------
/// Core paths
/// -------------------------
const INIT: &str = "/app/init";
const MAIN: &str = "/app/main";

/// -------------------------
/// Alias runtime (v2 ABI layer)
/// -------------------------
fn resolve_alias(exec: &str) -> Option<PathBuf> {
    let mut map: HashMap<&str, &str> = HashMap::new();

    map.insert("python", "/app/python-venv/bin/python");
    map.insert("python3", "/app/python-venv/bin/python");

    map.get(exec).map(PathBuf::from)
}

/// -------------------------
/// Validate exec string
/// -------------------------
fn validate_exec(exec: &str) -> Result<(), String> {
    if exec.contains('\\') {
        return Err("backslash not allowed".into());
    }
    if exec.contains("..") {
        return Err("path traversal not allowed".into());
    }
    Ok(())
}

/// -------------------------
/// Resolve exec -> safe /app path
/// -------------------------
fn resolve_exec(exec: &str) -> Result<PathBuf, String> {
    validate_exec(exec)?;

    // 1. alias resolution
    if let Some(p) = resolve_alias(exec) {
        return Ok(p);
    }

    // 2. /app relative resolution
    if exec.starts_with('/') {
        return Err("absolute paths not allowed".into());
    }

    let base = Path::new("/app");
    let full = base.join(exec);

    let canon = fs::canonicalize(&full)
        .map_err(|_| format!("exec not found: {}", full.display()))?;

    let app_root = fs::canonicalize(base)
        .map_err(|_| "missing /app".to_string())?;

    if !canon.starts_with(&app_root) {
        return Err("exec escapes /app".into());
    }

    Ok(canon)
}

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
/// forward signal to child
/// -------------------------
fn forward_signal(pid: Pid, sig: Signal) {
    let _ = signal::kill(pid, sig);
}

/// -------------------------
/// reap zombies (PID1 hygiene)
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
/// spawn main process
/// -------------------------
fn spawn_main() -> Result<(std::process::Child, Pid), String> {
    if !Path::new(MAIN).exists() {
        return Err("/app/main is required but missing".into());
    }

    let mut child = Command::new(MAIN)
        .spawn()
        .map_err(|e| format!("failed to spawn main: {e}"))?;

    let pid = Pid::from_raw(child.id() as i32);

    Ok((child, pid))
}

/// -------------------------
/// PID1 runtime loop
/// -------------------------
fn main() {
    if let Err(e) = run_init() {
        eprintln!("[init] failed: {e}");
        exit(1);
    }

    let (mut child, child_pid) = match spawn_main() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[main] {e}");
            exit(1);
        }
    };

    let mut signals = Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGCHLD,
    ]).expect("signal setup failed");

    let mut shutting_down = false;

    for sig in signals.forever() {
        match sig {
            signal_hook::consts::SIGTERM | signal_hook::consts::SIGINT => {
                eprintln!("[pid1] shutdown signal received");
                shutting_down = true;

                forward_signal(child_pid, Signal::SIGTERM);
            }

            signal_hook::consts::SIGCHLD => {
                reap_children();
            }

            _ => {}
        }

        // exit condition
        if shutting_down {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {}
                _ => break,
            }
        }
    }

    forward_signal(child_pid, Signal::SIGTERM);
    reap_children();

    let _ = child.wait();

    eprintln!("[pid1] exit complete");
}
