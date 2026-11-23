//! Suspend to disk (plan.md §7.2, M7-01).
//!
//! Suspending migrates the VM into a file (`migrate` to a `file:` URI, with
//! the `mapped-ram` capability when QEMU has it) and then quits QEMU. The VM
//! is `suspended`: no process, its RAM and device state in
//! `<vm dir>/suspend/state.qemu`. The next start rebuilds the same machine
//! from the configuration the process had, starts it with `-incoming defer`,
//! loads the file with `migrate-incoming` and continues.
//!
//! `file:` needs no helper process (unlike `exec:`, which Windows lacks and
//! `-sandbox spawn=deny` blocks). On Windows QEMU cannot migrate to a file
//! (it fails to make the file channel non-blocking), so there the daemon
//! relays the stream over an AF_UNIX socket into the same file, and back
//! for the resume. While a VM is suspended its configuration
//! and disks must not change, or the saved state would no longer match them:
//! editing, snapshots, disk changes, cloning and image operations on its
//! disks are refused until it is resumed or the saved state is discarded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::vm::{VmState, VmSummary};
use qemu_ui_qmp::Qmp;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{OpError, OpResult, Supervisor, now};
use crate::procs;

const STATE_FILE: &str = "state.qemu";
const META_FILE: &str = "suspend.json";
/// How long writing or reading the state file may take.
const MIGRATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the resume needs to rebuild the same machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SuspendMeta {
    /// The configuration the process had (including hot-plugged disks).
    pub spec_json: String,
    /// The state file was written with `mapped-ram`; loading needs it too.
    pub mapped_ram: bool,
    /// The guest was paused when it was suspended: resume leaves it paused.
    pub was_paused: bool,
    pub at: u64,
}


/// `file:` URI for a path. QEMU takes everything after the scheme as the
/// file name (an optional `,offset=` suffix aside), Windows drive letters
/// included.
fn file_uri(p: &Path) -> String {
    format!("file:{}", p.display())
}

impl Supervisor {
    fn suspend_dir(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("suspend")
    }

    /// The saved state of a suspended VM, if there is a complete one.
    pub(super) fn suspend_meta(&self, id: &str) -> Option<SuspendMeta> {
        let dir = self.suspend_dir(id);
        if !dir.join(STATE_FILE).is_file() {
            return None;
        }
        let json = std::fs::read_to_string(dir.join(META_FILE)).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub(super) fn state_file(&self, id: &str) -> PathBuf {
        self.suspend_dir(id).join(STATE_FILE)
    }

    fn remove_suspend_state(&self, id: &str) {
        let _ = std::fs::remove_dir_all(self.suspend_dir(id));
    }

    /// Refuse changes that would make a suspended VM's saved state invalid.
    pub fn refuse_if_suspended(&self, id: &str) -> OpResult<()> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        if VmState::parse(&row.state) == VmState::Suspended {
            return Err(OpError::Conflict(
                "the VM is suspended to disk; resume it (start) or discard the saved state first"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Save the running VM's state to disk and stop its process.
    pub async fn suspend(&self, id: &str) -> OpResult<VmSummary> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let state = VmState::parse(&row.state);
        if !matches!(state, VmState::Running | VmState::Paused) {
            return Err(OpError::Conflict(
                "only a running or paused VM can be suspended".into(),
            ));
        }
        let qmp = self.qmp_of(id)?;
        let run = self
            .store
            .last_run(id)?
            .filter(|r| r.ended_at.is_none())
            .ok_or_else(|| OpError::Conflict("the VM is not running".into()))?;

        let dir = self.suspend_dir(id);
        self.remove_suspend_state(id);
        std::fs::create_dir_all(&dir).map_err(|e| OpError::Failed(e.into()))?;
        let file = dir.join(STATE_FILE);

        let st = qmp
            .execute("query-status", None)
            .await
            .map_err(|e| OpError::Failed(e.into()))?;
        let was_paused = st["status"].as_str() != Some("running");
        let mapped_ram = !RELAY && has_capability(&qmp, "mapped-ram").await;
        let sock = self.run_dir(id).join("mig.sock");

        let saved = async {
            if !was_paused {
                // One pass over a still guest: nothing is dirtied meanwhile.
                qmp.execute("stop", None).await?;
            }
            if mapped_ram {
                set_capability(&qmp, "mapped-ram").await?;
            }
            if !RELAY {
                qmp.execute("migrate", Some(json!({"uri": file_uri(&file)})))
                    .await
                    .context("starting the migration to the state file")?;
                return wait_migration(&qmp).await;
            }
            let receiving = relay::receive(&sock, &file)?;
            let started = qmp
                .execute("migrate", Some(json!({"uri": unix_uri(&sock)})))
                .await
                .context("starting the migration to the state file");
            let migrated = match started {
                Ok(_) => wait_migration(&qmp).await,
                Err(e) => Err(e),
            };
            if migrated.is_err() {
                relay::wake(&sock);
            }
            let copied = tokio::task::spawn_blocking(move || receiving.join())
                .await
                .map_err(|e| anyhow!("{e}"))?
                .map_err(|_| anyhow!("the state relay panicked"))?;
            let _ = std::fs::remove_file(&sock);
            migrated?;
            copied.context("writing the state file")?;
            Ok(())
        }
        .await;
        if let Err(e) = saved {
            self.remove_suspend_state(id);
            if mapped_ram {
                let _ = qmp
                    .execute(
                        "migrate-set-capabilities",
                        Some(json!({"capabilities": [{"capability": "mapped-ram", "state": false}]})),
                    )
                    .await;
            }
            if !was_paused {
                let _ = qmp.execute("cont", None).await;
            }
            self.refresh_status(id, &qmp).await;
            return Err(OpError::Failed(e.context("suspending to disk")));
        }

        let meta = SuspendMeta {
            spec_json: run.spec_json.clone(),
            mapped_ram,
            was_paused,
            at: now(),
        };
        std::fs::write(
            dir.join(META_FILE),
            serde_json::to_string(&meta).map_err(|e| OpError::Failed(e.into()))?,
        )
        .map_err(|e| OpError::Failed(e.into()))?;

        // The state is safe on disk: end the process. `finish` sees
        // `suspending` and settles on `suspended`.
        let proc_id = {
            let mut live = self.live.lock().unwrap();
            let l = live
                .get_mut(id)
                .ok_or_else(|| OpError::Conflict("the VM is not running".into()))?;
            l.stop_requested = true;
            l.suspending = true;
            l.proc_id
        };
        let _ = qmp
            .execute_with_timeout("quit", None, Duration::from_secs(5))
            .await;
        for _ in 0..40 {
            if !procs::alive(proc_id) && !self.live.lock().unwrap().contains_key(id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if procs::alive(proc_id) {
            procs::terminate(proc_id, true);
        }
        // `finish` runs on the monitor task; wait for it to settle.
        for _ in 0..40 {
            if !self.live.lock().unwrap().contains_key(id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        self.emit(
            id,
            "suspended",
            json!({"bytes": bytes, "mapped_ram": mapped_ram}),
        );
        self.summary_of(id).await
    }

    /// Throw the saved state away; the VM is then simply stopped.
    pub async fn discard_suspend(&self, id: &str) -> OpResult<VmSummary> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        if VmState::parse(&row.state) != VmState::Suspended {
            return Err(OpError::Conflict("the VM is not suspended".into()));
        }
        self.remove_suspend_state(id);
        self.set_state(id, VmState::Stopped, None, None);
        self.emit(id, "suspend-discarded", json!({}));
        self.summary_of(id).await
    }

    /// The resume finished: the state file is no longer needed.
    pub(super) fn resumed(&self, id: &str) {
        self.remove_suspend_state(id);
    }

    /// Load the state file into a QEMU started with `-incoming defer`.
    pub(super) async fn load_state(&self, id: &str, qmp: &Qmp, meta: &SuspendMeta) -> Result<()> {
        let file = self.state_file(id);
        if meta.mapped_ram {
            set_capability(qmp, "mapped-ram")
                .await
                .context("the state file needs mapped-ram, which this QEMU lacks")?;
        }
        if !RELAY {
            qmp.execute("migrate-incoming", Some(json!({"uri": file_uri(&file)})))
                .await
                .context("loading the saved state")?;
            return wait_migration(qmp).await.context("loading the saved state");
        }
        let sock = self.run_dir(id).join("mig.sock");
        let _ = std::fs::remove_file(&sock);
        qmp.execute("migrate-incoming", Some(json!({"uri": unix_uri(&sock)})))
            .await
            .context("loading the saved state")?;
        let sent = {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || relay::send(&sock, &file))
        };
        let loaded = wait_migration(qmp).await;
        let sent = sent.await.map_err(|e| anyhow!("{e}"))?;
        let _ = std::fs::remove_file(&sock);
        sent.context("reading the state file")?;
        loaded.context("loading the saved state")
    }
}

/// Relay the migration stream through the daemon (see the module docs).
const RELAY: bool = cfg!(windows);

fn unix_uri(p: &Path) -> String {
    format!("unix:{}", p.display())
}

/// Copy a migration stream between an AF_UNIX socket and the state file.
#[cfg_attr(not(windows), allow(dead_code))]
mod relay {
    use std::fs::File;
    use std::io::{self, BufReader, BufWriter, Write};
    use std::path::Path;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};
    #[cfg(windows)]
    use uds_windows::{UnixListener, UnixStream};

    /// Listen on `sock`; the thread takes one connection (QEMU's outgoing
    /// migration) and writes everything it sends to `file`.
    pub fn receive(sock: &Path, file: &Path) -> io::Result<JoinHandle<io::Result<u64>>> {
        let _ = std::fs::remove_file(sock);
        let listener = UnixListener::bind(sock)?;
        let mut out = BufWriter::new(File::create(file)?);
        Ok(std::thread::spawn(move || {
            let (conn, _) = listener.accept()?;
            let n = io::copy(&mut BufReader::new(conn), &mut out)?;
            let f = out.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()?;
            Ok(n)
        }))
    }

    /// Unblock a `receive` whose migration never connected.
    pub fn wake(sock: &Path) {
        let _ = UnixStream::connect(sock);
    }

    /// Connect to QEMU's incoming migration socket and send `file`.
    pub fn send(sock: &Path, file: &Path) -> io::Result<u64> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut conn = loop {
            match UnixStream::connect(sock) {
                Ok(c) => break c,
                Err(e) if Instant::now() > deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        let n = io::copy(&mut BufReader::new(File::open(file)?), &mut conn)?;
        conn.flush()?;
        let _ = conn.shutdown(std::net::Shutdown::Write);
        Ok(n)
    }
}

async fn has_capability(qmp: &Qmp, name: &str) -> bool {
    qmp.execute("query-migrate-capabilities", None)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|caps| caps.iter().any(|c| c["capability"] == name))
}

async fn set_capability(qmp: &Qmp, name: &str) -> Result<()> {
    qmp.execute(
        "migrate-set-capabilities",
        Some(json!({"capabilities": [{"capability": name, "state": true}]})),
    )
    .await?;
    Ok(())
}

/// Poll `query-migrate` until the migration completes or fails.
async fn wait_migration(qmp: &Qmp) -> Result<()> {
    let deadline = tokio::time::Instant::now() + MIGRATE_TIMEOUT;
    loop {
        let m = qmp.execute("query-migrate", None).await?;
        match m["status"].as_str().unwrap_or_default() {
            "completed" => return Ok(()),
            "failed" | "cancelled" => {
                let why = m["error-desc"].as_str().unwrap_or("no reason given");
                bail!("migration {}: {why}", m["status"].as_str().unwrap_or("failed"));
            }
            _ => {}
        }
        if tokio::time::Instant::now() > deadline {
            let _ = qmp.execute("migrate_cancel", None).await;
            return Err(anyhow!("timed out"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Suspend and resume against a real QEMU. Skipped without QEMU.
#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;
    use qemu_ui_core::spec::ImageRef;

    async fn pmemsave(qmp: &Qmp, file: &Path) -> Vec<u8> {
        let _ = std::fs::remove_file(file);
        qmp.execute(
            "pmemsave",
            Some(json!({"val": 0, "size": 1 << 20, "filename": file.display().to_string()})),
        )
        .await
        .unwrap();
        std::fs::read(file).unwrap()
    }

    #[test]
    fn suspend_to_disk_and_resume() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("susp");
        let _kill = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "sleepy").await;
            let err = |e: OpError| format!("{e:?}");

            sv.start(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Running).await;
            assert!(
                matches!(sv.discard_suspend(&id).await, Err(OpError::Conflict(_))),
                "nothing to discard"
            );

            // Paused first: RAM is frozen, so it must come back byte for byte.
            sv.qmp_action(&id, "pause").await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Paused).await;
            let before = pmemsave(&sv.live_qmp(&id).unwrap(), &data.join("a.bin")).await;
            assert!(before.iter().any(|b| *b != 0), "firmware is in low memory");

            let s = sv.suspend(&id).await.map_err(err).unwrap();
            assert_eq!(s.state, VmState::Suspended, "{s:?}");
            assert!(sv.live_qmp(&id).is_none(), "the process is gone");
            assert!(sv.state_file(&id).is_file());

            // Nothing may change under the saved state.
            let spec = sv.detail(&id).ok().unwrap().spec;
            assert!(matches!(
                sv.update(&id, spec.clone()).await,
                Err(OpError::Conflict(m)) if m.contains("suspended")
            ));
            let disk: &ImageRef = &spec.disks[0].image;
            let (_, open) = sv.image_users();
            assert_eq!(
                open.get(disk).cloned().unwrap_or_default(),
                vec!["sleepy".to_string()]
            );

            // Resume: paused as it was, with the same memory.
            let s = sv.start(&id).await.map_err(err).unwrap();
            assert_eq!(s.state, VmState::Paused, "{s:?}");
            let args = sv.detail(&id).ok().unwrap().last_run_args.unwrap();
            assert!(
                args.windows(2).any(|w| w[0] == "-incoming" && w[1] == "defer"),
                "{args:?}"
            );
            let after = pmemsave(&sv.live_qmp(&id).unwrap(), &data.join("b.bin")).await;
            assert!(before == after, "guest memory differs after resume");
            assert!(!sv.state_file(&id).exists(), "the state file is consumed");

            sv.qmp_action(&id, "resume").await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Running).await;

            // A running VM suspends and comes back running.
            sv.suspend(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Suspended).await;
            sv.start(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Running).await;

            // Discarding the saved state leaves a plain stopped VM.
            sv.suspend(&id).await.map_err(err).unwrap();
            let s = sv.discard_suspend(&id).await.map_err(err).unwrap();
            assert_eq!(s.state, VmState::Stopped);
            assert!(!sv.state_file(&id).exists());
            sv.update(&id, spec).await.map_err(err).unwrap();
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
