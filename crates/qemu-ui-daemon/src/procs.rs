//! Process spawning and identity (plan.md §3.2, M1-05/M1-06).
//!
//! QEMU must outlive the daemon, so it is spawned detached:
//! * stdout/stderr go to a log file, never a pipe (a dead reader would give
//!   QEMU EPIPE/SIGPIPE);
//! * Windows: own process group, no console, and break away from the
//!   daemon's job object when the job allows it;
//! * Unix: own process group, so a terminal Ctrl-C to the daemon does not
//!   reach the VM. (systemd `KillMode`/per-VM scopes are deployment, M6-09.)
//!
//! A pid alone is not an identity — pids are reused — so every run records
//! the process start time and re-attach checks both.

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: u32,
    /// Seconds since the Unix epoch, as reported by the OS.
    pub start_time: u64,
}

/// Information about a live process.
pub struct ProcInfo {
    pub start_time: u64,
}

fn system_for(pid: u32) -> Option<System> {
    let mut sys = System::new().ok()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    Some(sys)
}

pub fn info(pid: u32) -> Option<ProcInfo> {
    let sys = system_for(pid)?;
    let p = sys.process(Pid::from_u32(pid))?;
    Some(ProcInfo {
        start_time: p.start_time(),
    })
}

/// Is this exact process (pid + start time) still running?
pub fn alive(id: ProcId) -> bool {
    // Start times are second-granular and may be derived slightly differently
    // between calls on some platforms; allow one second of slack.
    info(id.pid).is_some_and(|i| i.start_time.abs_diff(id.start_time) <= 1)
}

/// Terminate the process if it is still the same one. `force` = SIGKILL /
/// TerminateProcess; otherwise SIGTERM where supported.
pub fn terminate(id: ProcId, force: bool) -> bool {
    let Some(sys) = system_for(id.pid) else {
        return false;
    };
    let Some(p) = sys.process(Pid::from_u32(id.pid)) else {
        return false;
    };
    if p.start_time().abs_diff(id.start_time) > 1 {
        return false; // pid was reused
    }
    if force {
        p.kill()
    } else {
        p.kill_with(Signal::Term).unwrap_or_else(|| p.kill())
    }
}

/// Spawn `program args…` detached, logging to `log`. Returns the child (so
/// the caller can reap it while it is our child) and its identity.
pub fn spawn_detached(program: &str, args: &[String], log: &Path) -> Result<(Child, ProcId)> {
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let err = out.try_clone()?;

    let build = |breakaway: bool| {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone().expect("dup log handle")))
            .stderr(Stdio::from(err.try_clone().expect("dup log handle")));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
            if breakaway {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            cmd.creation_flags(flags);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let _ = breakaway;
            cmd.process_group(0);
        }
        cmd
    };

    // Breaking away fails with "access denied" when the daemon's job does not
    // permit it; then the VM stays in the job (documented in M6-06).
    let child = match build(true).spawn() {
        Ok(c) => Ok(c),
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(5) => build(false).spawn(),
        Err(e) => Err(e),
    }
    .with_context(|| format!("starting {program}"))?;

    let pid = child.id();
    let mut start_time = info(pid).map(|i| i.start_time).unwrap_or(0);
    if start_time == 0 {
        // Very short-lived processes may already be gone; record "now".
        start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
    }
    Ok((child, ProcId { pid, start_time }))
}

/// Last `n` non-empty lines of a log file, for error messages.
pub fn log_tail(path: &Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_of_self() {
        let me = std::process::id();
        let i = info(me).expect("own process visible");
        assert!(i.start_time > 0);
        assert!(alive(ProcId {
            pid: me,
            start_time: i.start_time
        }));
        assert!(
            !alive(ProcId {
                pid: me,
                start_time: i.start_time + 1000
            }),
            "start time must match"
        );
    }
}
