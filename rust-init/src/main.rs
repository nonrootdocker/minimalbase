use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use signal_hook::iterator::Signals;

use std::path::Path;
use std::process::{Command, exit};

use serde::Deserialize;

const MAIN_ABI: &str = "/app/main";

/// -------------------------
/// ABI STRUCT (strict schema)
/// -------------------------
#[derive(Debug, Deserialize)]
struct Abi {
    process: Process,
}

#[derive(Debug, Deserialize)]
struct Process {
    exec: String,
    args: Option<Vec<String>>,
}

/// -------------------------
/// Load ABI (JSON parsing included)
/// -------------------------
fn load_abi() -> Result<(String, Vec<String>), String> {
    let content = std::fs::read_to_string(MAIN_ABI)
        .map_err(|e| format!("failed to read /app/main: {e}"))?;

    let abi: Abi = serde_json::from_str(&content)
        .map_err(|e| format!("invalid ABI JSON: {e}"))?;

    let args = abi.process.args.unwrap_or_default();

    Ok((abi.process.exec, args))
}

/// -------------------------
/// resolve execution (v2.1 rules)
/// -------------------------
fn resolve_exec(exec: &str, args: Vec<String>) -> Result<Vec<String>, String> {
    match exec {
        /// Python special-case (fixed contract)
        "python" => {
            let mut cmd = vec![
                "/app/python-venv/bin/python".to_string(),
                "/app/main.py".to_string(),
            ];
            cmd.extend(args);
            Ok(cmd)
        }

        /// Named process → strict /app/<name>
        name => {
            let path = format!("/app/{}", name);

            if !Path::new(&path).exists() {
                return Err(format!("process not found: {}", path));
            }

            let mut cmd = vec![path];
            cmd.extend(args);
            Ok(cmd)
        }
    }
}

/// -------------------------
/// forward signal
/// -------------------------
fn forward_signal(pid: Pid, sig: Signal) {
    let _ = signal::kill(pid, sig);
}

/// -------------------------
/// reap children (PID1 hygiene)
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
    // -------------------------
    // Load ABI
    // -------------------------
    let (exec, args) = match load_abi() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[init] ABI load failed: {e}");
            exit(1);
        }
    };

    // -------------------------
    // Resolve command
    // -------------------------
    let cmd = match resolve_exec(&exec, args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[init] resolve failed: {e}");
            exit(1);
        }
    };

    // -------------------------
    // Spawn process
    // -------------------------
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("[init] failed to start process: {e}");
            exit(1);
        });

    let child_pid = Pid::from_raw(child.id() as i32);

    // -------------------------
    // Signal handling
    // -------------------------
    let mut signals = Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGCHLD,
    ]).expect("signal setup failed");

    let mut shutting_down = false;

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

        // exit when child is gone
        if shutting_down {
            match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {}
                _ => break,
            }
        }
    }

    // -------------------------
    // cleanup
    // -------------------------
    forward_signal(child_pid, Signal::SIGTERM);
    reap_children();

    let _ = child.wait();

    eprintln!("[init] exit complete");
}
