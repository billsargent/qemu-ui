//! Disk exports: one privileged `qemu-storage-daemon` per exported disk,
//! serving NBD on an AF_UNIX socket (docs/design/privileged-helper.md).
//!
//! Exports are persisted so they survive helper restarts and are re-attached
//! by pid + process start time, like VMs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::helper::{DiskExport, HostDisk};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    export: DiskExport,
    pid: u32,
    start_time: u64,
}

pub struct Exports {
    dir: PathBuf,
    qsd: PathBuf,
    records: Mutex<BTreeMap<String, Record>>,
}

fn start_time(pid: u32) -> Option<u64> {
    let mut sys = System::new().ok()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).map(|p| p.start_time())
}

fn alive(pid: u32, started: u64) -> bool {
    start_time(pid).is_some_and(|t| t.abs_diff(started) <= 1)
}

fn kill(pid: u32, started: u64) {
    let Ok(mut sys) = System::new() else { return };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(p) = sys.process(Pid::from_u32(pid))
        && p.start_time().abs_diff(started) <= 1
    {
        p.kill();
    }
}

/// The blockdev for a device path: devices need `host_device`; the `file`
/// driver is only reachable from tests (the protocol only exports listed disks).
fn blockdev(path: &str, writable: bool) -> serde_json::Value {
    let device = path.starts_with("/dev/") || path.starts_with(r"\\.\");
    serde_json::json!({
        "node-name": "disk",
        "driver": if device { "host_device" } else { "file" },
        "filename": path,
        "read-only": !writable,
        "aio": "threads",
    })
}

impl Exports {
    /// `dir` holds sockets, logs and the state file; `qsd` is the
    /// `qemu-storage-daemon` binary.
    pub fn open(dir: &Path, qsd: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let state = dir.join("exports.json");
        let mut records: BTreeMap<String, Record> = std::fs::read_to_string(&state)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Re-attach: keep exports whose storage daemon is still the same process.
        records.retain(|_, r| alive(r.pid, r.start_time));
        let e = Exports {
            dir: dir.to_path_buf(),
            qsd: qsd.to_path_buf(),
            records: Mutex::new(records),
        };
        e.save()?;
        Ok(e)
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&*self.records.lock().unwrap())?;
        std::fs::write(self.dir.join("exports.json"), json)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<DiskExport> {
        let mut recs = self.records.lock().unwrap();
        recs.retain(|_, r| alive(r.pid, r.start_time));
        recs.values().map(|r| r.export.clone()).collect()
    }

    /// Start exporting `disk`. Policy checks happen before this is called.
    pub async fn export(&self, disk: &HostDisk, writable: bool) -> Result<DiskExport> {
        if let Some(r) = self
            .records
            .lock()
            .unwrap()
            .values()
            .find(|r| r.export.disk_id == disk.id)
        {
            bail!(
                "'{}' is already exported as {}",
                disk.label,
                r.export.export_id
            );
        }
        let id = format!("e{:08x}", rand::random::<u32>());
        let socket = self.dir.join(format!("{id}.sock"));
        qemu_ui_qmp::transport::check_unix_path(&socket)?;
        let socket_str = socket.display().to_string().replace('\\', "/");
        let log = std::fs::File::create(self.dir.join(format!("{id}.log")))?;
        let mut cmd = std::process::Command::new(&self.qsd);
        cmd.arg("--blockdev")
            .arg(blockdev(&disk.device_path, writable).to_string())
            .arg("--nbd-server")
            .arg(format!(
                "addr.type=unix,addr.path={}",
                socket_str.replace(',', ",,")
            ))
            .arg("--export")
            .arg(format!(
                "type=nbd,id={id},node-name=disk,name={id},writable={}",
                if writable { "on" } else { "off" }
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", self.qsd.display()))?;
        let pid = child.id();
        // Ready when the socket appears; failed if the process exits first.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if socket.exists() {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                let log =
                    std::fs::read_to_string(self.dir.join(format!("{id}.log"))).unwrap_or_default();
                bail!("qemu-storage-daemon exited ({status}): {}", log.trim());
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                bail!("qemu-storage-daemon did not create its socket in time");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The storage daemon outlives this handle; reap it in the background
        // while we are its parent.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let export = DiskExport {
            export_id: id.clone(),
            disk_id: disk.id.clone(),
            writable,
            socket: socket_str,
            export_name: id.clone(),
        };
        let started = start_time(pid).unwrap_or(0);
        self.records.lock().unwrap().insert(
            id,
            Record {
                export: export.clone(),
                pid,
                start_time: started,
            },
        );
        self.save()?;
        Ok(export)
    }

    pub fn unexport(&self, export_id: &str) -> Result<()> {
        let rec = self
            .records
            .lock()
            .unwrap()
            .remove(export_id)
            .ok_or_else(|| anyhow!("no export '{export_id}'"))?;
        kill(rec.pid, rec.start_time);
        let _ = std::fs::remove_file(self.dir.join(format!("{export_id}.sock")));
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(exe: &str) -> Option<PathBuf> {
        let exe = if cfg!(windows) {
            format!("{exe}.exe")
        } else {
            exe.to_string()
        };
        let mut dirs: Vec<PathBuf> = std::env::var_os("QEMU_UI_QEMU_DIR")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
    }

    /// End to end with a file standing in for a disk: the storage daemon
    /// exports it, an unprivileged QEMU attaches it over NBD, a wrong export
    /// name is refused, and the export survives "helper restart".
    #[tokio::test]
    async fn export_attach_and_reattach() {
        let (Some(qsd), Some(qemu), Some(img)) = (
            find("qemu-storage-daemon"),
            find("qemu-system-x86_64"),
            find("qemu-img"),
        ) else {
            eprintln!("skipping: QEMU tools not found");
            return;
        };
        let base = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let dir = base.join(format!("quih-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fake-disk.raw");
        assert!(
            std::process::Command::new(&img)
                .args(["create", "-q", "-f", "raw"])
                .arg(&file)
                .arg("16M")
                .status()
                .unwrap()
                .success()
        );

        let disk = HostDisk {
            id: "test-disk".into(),
            label: "test".into(),
            size_bytes: 16 << 20,
            bus: "file".into(),
            removable: false,
            system: false,
            read_only: false,
            in_use: vec![],
            offline: true,
            device_path: file.display().to_string(),
        };
        let ex = Exports::open(&dir.join("x"), &qsd).unwrap();
        let e = ex.export(&disk, false).await.unwrap();
        assert!(
            ex.export(&disk, false).await.is_err(),
            "one export per disk"
        );

        let attach = |name: &str| {
            let bd = serde_json::json!({"node-name": "pd", "driver": "nbd",
                "server": {"type": "unix", "path": e.socket}, "export": name, "read-only": true});
            let mut c = std::process::Command::new(&qemu);
            c.args(["-machine", "none", "-nodefaults", "-display", "none", "-S"])
                .arg("-blockdev")
                .arg(bd.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let mut child = c.spawn().unwrap();
            std::thread::sleep(Duration::from_secs(2));
            let exited = child.try_wait().unwrap();
            let _ = child.kill();
            let mut err = String::new();
            if let Some(mut s) = child.stderr.take() {
                use std::io::Read;
                let _ = s.read_to_string(&mut err);
            }
            (exited.is_none(), err)
        };
        let (running, err) = attach(&e.export_name);
        assert!(running, "QEMU should attach the export: {err}");
        let (running, err) = attach("wrong-name");
        assert!(!running && err.contains("export"), "{err}");

        // Helper restart: the export is re-attached, not lost.
        drop(ex);
        let ex = Exports::open(&dir.join("x"), &qsd).unwrap();
        assert_eq!(ex.list().len(), 1);
        ex.unexport(&e.export_id).unwrap();
        assert!(ex.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
