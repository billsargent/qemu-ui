//! Operational cases (plan.md §7.4, X-02): host sleep and log rotation.
//!
//! * **Host sleep.** A housekeeping loop ticks every few seconds. When a
//!   tick arrives far later than asked for, by the wall clock or the
//!   monotonic one (which one keeps counting through sleep differs per OS),
//!   the host was suspended or stalled. Guest clocks are then behind: VMs
//!   with a guest agent get their clock set from the host, and every running
//!   VM gets a `host-resumed` event saying whether that happened.
//! * **Log rotation.** The serial log is appended to across runs and is
//!   rotated to `serial.log.1` when it passes [`SERIAL_LOG_CAP`], at start
//!   and (on Unix, where truncating a file QEMU appends to is safe) while
//!   the VM runs. Per-run logs (`logs/run-<t>*.log`) keep the newest
//!   [`KEEP_RUNS`] runs within [`LOGS_CAP`] bytes.
//!
//! A QEMU upgrade under a running VM is handled where runs are recorded:
//! the run keeps the build it started with and the VM summary shows the
//! version it moves to on its next start (`qemu_update`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use super::Supervisor;

/// The serial log rotates past this size.
pub const SERIAL_LOG_CAP: u64 = 8 << 20;
/// Per-run logs of this many runs are kept…
pub const KEEP_RUNS: usize = 10;
/// …within this many bytes (the newest run is always kept).
pub const LOGS_CAP: u64 = 64 << 20;

const TICK: Duration = Duration::from_secs(5);
/// A tick this much later than [`TICK`] means the host slept (or stalled
/// long enough for guest clocks to fall behind).
const SLEEP_GAP: Duration = Duration::from_secs(30);
/// Serial logs of running VMs are checked every this many ticks.
const ROTATE_EVERY: u32 = 12;

/// How much later than expected a tick arrived, by either clock.
fn overslept(expected: Duration, mono: Duration, wall: Option<Duration>) -> Duration {
    mono.max(wall.unwrap_or_default()).saturating_sub(expected)
}

impl Supervisor {
    /// Background upkeep; runs for the life of the daemon.
    pub async fn housekeeping(self: Arc<Self>) {
        let mut ticks = 0u32;
        loop {
            let (mono0, wall0) = (Instant::now(), SystemTime::now());
            tokio::time::sleep(TICK).await;
            let late = overslept(
                TICK,
                mono0.elapsed(),
                SystemTime::now().duration_since(wall0).ok(),
            );
            if late >= SLEEP_GAP {
                tracing::info!(
                    seconds = late.as_secs(),
                    "host resumed from sleep (or stalled)"
                );
                self.host_resumed(late).await;
            }
            ticks = ticks.wrapping_add(1);
            if cfg!(unix) && ticks.is_multiple_of(ROTATE_EVERY) {
                let ids: Vec<String> = self.live.lock().unwrap().keys().cloned().collect();
                for id in ids {
                    let _ = truncate_rotate(&self.serial_log(&id), SERIAL_LOG_CAP);
                }
            }
        }
    }

    /// The host was away for `gap`: bring guest clocks back where we can.
    pub(crate) async fn host_resumed(&self, gap: Duration) {
        let vms: Vec<(String, Option<qemu_ui_qmp::Qmp>)> = self
            .live
            .lock()
            .unwrap()
            .iter()
            .map(|(id, l)| (id.clone(), l.qmp.clone()))
            .collect();
        for (id, qmp) in vms {
            let synced = match qmp {
                Some(q) => match crate::qga::socket(&q).await {
                    Some(sock) => crate::qga::set_time(&sock).await.is_ok(),
                    None => false,
                },
                None => false,
            };
            self.emit(
                &id,
                "host-resumed",
                json!({
                    "away_s": gap.as_secs(),
                    "clock_synced": synced,
                    "note": if synced {
                        "The guest clock was set from the host."
                    } else {
                        "The guest clock may be behind; a guest agent lets qemu-ui correct it."
                    },
                }),
            );
        }
    }

    /// Rotate and prune a VM's logs before a run starts.
    pub(crate) fn rotate_logs(&self, id: &str) {
        let _ = rotate(&self.serial_log(id), SERIAL_LOG_CAP);
        let _ = prune_run_logs(&self.vm_dir(id).join("logs"), KEEP_RUNS, LOGS_CAP);
    }
}

/// Move `path` to `path.1` (replacing it) when it is larger than `cap`.
pub fn rotate(path: &Path, cap: u64) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > cap => {
            std::fs::rename(path, old(path))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Like [`rotate`] for a file a process keeps appending to: copy it to
/// `path.1`, then truncate it in place (appends continue at the new end).
pub fn truncate_rotate(path: &Path, cap: u64) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > cap => {
            std::fs::copy(path, old(path))?;
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)?
                .set_len(0)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn old(path: &Path) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".1");
    p.into()
}

/// Keep the per-run logs of the newest `keep` runs, dropping older runs
/// while the total is over `cap` bytes. A run's logs share the
/// `run-<t>` prefix (`run-<t>.log`, `run-<t>.swtpm.log`, …).
pub fn prune_run_logs(dir: &Path, keep: usize, cap: u64) -> std::io::Result<usize> {
    let mut runs: std::collections::BTreeMap<u64, Vec<(std::path::PathBuf, u64)>> =
        Default::default();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        let Some(t) = name
            .strip_prefix("run-")
            .and_then(|r| r.split('.').next())
            .and_then(|t| t.parse::<u64>().ok())
        else {
            continue;
        };
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        runs.entry(t).or_default().push((e.path(), size));
    }
    let mut total: u64 = runs.values().flatten().map(|(_, s)| s).sum();
    let mut count = runs.len();
    let mut removed = 0;
    // Oldest first; never the newest run.
    for (_, files) in runs.into_iter() {
        if count <= 1 || (count <= keep && total <= cap) {
            break;
        }
        for (path, size) in files {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
                total = total.saturating_sub(size);
            }
        }
        count -= 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("qui-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sleep_detection_uses_the_later_clock() {
        let s = Duration::from_secs;
        assert_eq!(overslept(s(5), s(5), Some(s(5))), s(0));
        // Linux: the monotonic clock stops during suspend, the wall clock not.
        assert_eq!(overslept(s(5), s(5), Some(s(605))), s(600));
        // A clock that keeps counting through sleep.
        assert_eq!(overslept(s(5), s(605), Some(s(5))), s(600));
        // Wall clock set backwards.
        assert_eq!(overslept(s(5), s(5), None), s(0));
    }

    #[test]
    fn serial_log_rotates_past_the_cap() {
        let d = tmp("serial");
        let p = d.join("serial.log");
        std::fs::write(&p, vec![b'x'; 100]).unwrap();
        assert!(!rotate(&p, 100).unwrap(), "at the cap is fine");
        std::fs::write(&p, vec![b'y'; 101]).unwrap();
        assert!(rotate(&p, 100).unwrap());
        assert!(!p.exists());
        assert_eq!(std::fs::read(d.join("serial.log.1")).unwrap().len(), 101);

        std::fs::write(&p, vec![b'z'; 150]).unwrap();
        assert!(truncate_rotate(&p, 100).unwrap());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        assert_eq!(
            std::fs::read(d.join("serial.log.1")).unwrap(),
            vec![b'z'; 150]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn run_logs_keep_the_newest_runs_within_the_cap() {
        let d = tmp("runs");
        for t in 1..=5u64 {
            std::fs::write(d.join(format!("run-{t}.log")), vec![0u8; 10]).unwrap();
            std::fs::write(d.join(format!("run-{t}.swtpm.log")), vec![0u8; 10]).unwrap();
        }
        std::fs::write(d.join("other.txt"), b"keep").unwrap();
        // Count limit: 3 runs.
        assert_eq!(prune_run_logs(&d, 3, u64::MAX).unwrap(), 4);
        let mut left: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "other.txt",
                "run-3.log",
                "run-3.swtpm.log",
                "run-4.log",
                "run-4.swtpm.log",
                "run-5.log",
                "run-5.swtpm.log"
            ]
        );
        // Size limit: 25 bytes leaves one run (20 bytes).
        prune_run_logs(&d, 10, 25).unwrap();
        assert!(d.join("run-5.log").exists() && !d.join("run-4.log").exists());
        // The newest run stays even when it alone is over the cap.
        prune_run_logs(&d, 10, 1).unwrap();
        assert!(d.join("run-5.log").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn io_error_messages_name_the_disk() {
        use qemu_ui_core::spec::*;
        let spec: VmSpec = serde_json::from_value(json!({
            "spec_version": 1, "name": "v", "arch": "x86_64",
            "machine": {"type": "q35"}, "cpu": {"count": 1}, "memory": {"size_mib": 64},
            "disks": [{"image": {"pool": "default", "name": "root.qcow2"}, "bus": "virtio",
                       "media": "disk", "readonly": false, "boot_index": null,
                       "cache": "none", "discard": false}]
        }))
        .unwrap();
        let full = super::super::io_error_message(
            &json!({"device": "qui-dev0", "node-name": "qui-disk0", "operation": "write",
                    "action": "stop", "nospace": true, "reason": "No space left on device"}),
            Some(&spec),
        );
        assert!(
            full.contains("default/root.qcow2") && full.contains("free space"),
            "{full}"
        );
        let other = super::super::io_error_message(
            &json!({"device": "", "node-name": "qui-disk7", "operation": "read",
                    "action": "stop", "reason": "Input/output error"}),
            Some(&spec),
        );
        assert!(
            other.contains("qui-disk7") && other.contains("Input/output error"),
            "{other}"
        );
    }
}
