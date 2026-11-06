//! VM snapshots (plan.md §9.3, M2-06).
//!
//! Internal qcow2 snapshots with one tag across every writable disk of a VM
//! and its UEFI NVRAM file:
//! * running VM, with memory: QMP `snapshot-save` / `snapshot-load` /
//!   `snapshot-delete` (QEMU's own block jobs), wrapped in our job system;
//! * running VM, disk-only: a QMP `transaction` of
//!   `blockdev-snapshot-internal-sync` (crash-consistent);
//! * stopped VM: `qemu-img snapshot -c/-a/-d` on each file.
//!
//! Snapshots form a tree: a new snapshot's parent is the VM's current head;
//! reverting moves the head; deleting re-parents children. A revert can take
//! an automatic safety snapshot first.
//!
//! Every operation holds the supervisor's lifecycle lock, so it cannot race
//! a start, stop or edit.

use super::*;
use crate::jobs::JobCtx;
use crate::store::SnapshotRow;
use qemu_ui_core::snapshot::{
    SnapshotDisk, SnapshotInfo, SnapshotList, snapshot_disks, valid_snapshot_name,
};
use qemu_ui_core::storage::Job;

/// Upper bound for one QEMU snapshot job (large RAM, slow disks).
const QMP_JOB_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn info_of(r: &SnapshotRow) -> SnapshotInfo {
    SnapshotInfo {
        id: r.id.clone(),
        name: r.name.clone(),
        description: r.description.clone(),
        tag: r.tag.clone(),
        parent_id: r.parent_id.clone(),
        with_memory: r.with_memory,
        auto: r.auto,
        disks: serde_json::from_str(&r.disks_json).unwrap_or_default(),
        created_at: r.created_at,
    }
}

/// What a snapshot operation acts on, captured under the lifecycle lock.
struct Target {
    /// The spec of the running process, or the saved spec when stopped.
    spec: VmSpec,
    qmp: Option<Qmp>,
}

/// Run a QEMU block job started by `command` and wait for it, reporting
/// progress. QEMU's snapshot jobs conclude and must be dismissed.
async fn qmp_job(qmp: &Qmp, ctx: &JobCtx, command: &str, mut args: Value) -> Result<()> {
    let job_id = format!("qui-{:08x}", rand::random::<u32>());
    args["job-id"] = json!(job_id);
    ctx.log_line(&format!("QMP {command} {args}"));
    qmp.execute(command, Some(args))
        .await
        .with_context(|| command.to_string())?;
    let deadline = tokio::time::Instant::now() + QMP_JOB_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let jobs = qmp.execute("query-jobs", None).await?;
        let job = jobs
            .as_array()
            .and_then(|a| a.iter().find(|j| j["id"] == json!(job_id)))
            .cloned();
        let Some(job) = job else {
            // Auto-dismissed without an error: done.
            return Ok(());
        };
        let total = job["total-progress"].as_f64().unwrap_or(0.0);
        if total > 0.0 {
            ctx.progress(
                Some(job["current-progress"].as_f64().unwrap_or(0.0) / total),
                None,
            );
        }
        if job["status"] == "concluded" {
            let _ = qmp
                .execute("job-dismiss", Some(json!({"id": job_id})))
                .await;
            if let Some(err) = job["error"].as_str() {
                bail!("{command}: {err}");
            }
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("{command} did not finish in time");
        }
    }
}

impl Supervisor {
    fn snapshot_file(&self, id: &str, d: &SnapshotDisk) -> Result<PathBuf> {
        if d.kind == "nvram" {
            Ok(self.vm_dir(id).join("VARS.qcow2"))
        } else {
            self.pools.resolve(&ImageRef {
                pool: d.pool.clone(),
                name: d.name.clone(),
            })
        }
    }

    /// The spec in effect and the monitor if the VM has a process.
    fn snapshot_target(&self, id: &str) -> OpResult<Target> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let qmp = self.qmp_of(id).ok();
        let spec_json = match &qmp {
            Some(_) => self
                .store
                .last_run(id)?
                .filter(|r| r.ended_at.is_none())
                .map(|r| r.spec_json)
                .unwrap_or(row.spec_json),
            None => row.spec_json,
        };
        let spec = serde_json::from_str(&spec_json).map_err(|e| OpError::Failed(e.into()))?;
        Ok(Target { spec, qmp })
    }

    /// The disks (and NVRAM) a new snapshot of this VM would cover.
    async fn plan_snapshot_disks(&self, id: &str, spec: &VmSpec) -> OpResult<Vec<SnapshotDisk>> {
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| OpError::Conflict("qemu-img is not installed".into()))?;
        let mut formats = Vec::new();
        for d in &spec.disks {
            let f = match self.pools.resolve(&d.image) {
                Ok(p) => self.pools.info(&qemu_img, &p).await.ok().map(|(f, _, _)| f),
                Err(_) => None,
            };
            formats.push(f);
        }
        let idx = snapshot_disks(spec, &formats).map_err(OpError::Conflict)?;
        let mut disks: Vec<SnapshotDisk> = idx
            .into_iter()
            .map(|i| SnapshotDisk {
                kind: "image".into(),
                pool: spec.disks[i].image.pool.clone(),
                name: spec.disks[i].image.name.clone(),
                node: format!("qui-disk{i}"),
            })
            .collect();
        if matches!(spec.firmware, FirmwareSpec::Descriptor { .. })
            && self.vm_dir(id).join("VARS.qcow2").exists()
        {
            disks.push(SnapshotDisk {
                kind: "nvram".into(),
                pool: String::new(),
                name: "VARS.qcow2".into(),
                node: "qui-fw-vars".into(),
            });
        }
        Ok(disks)
    }

    pub async fn list_snapshots(&self, id: &str) -> OpResult<SnapshotList> {
        let target = self.snapshot_target(id)?;
        let unsupported_reason = match self.plan_snapshot_disks(id, &target.spec).await {
            Ok(_) => None,
            Err(OpError::Conflict(m)) => Some(m),
            Err(e) => return Err(e),
        };
        Ok(SnapshotList {
            snapshots: self.store.list_snapshots(id)?.iter().map(info_of).collect(),
            head: self.store.snapshot_head(id)?,
            unsupported_reason,
        })
    }

    /// Take a snapshot as a job. `with_memory` requires a running VM.
    pub async fn take_snapshot(
        self: &Arc<Self>,
        id: &str,
        name: &str,
        description: &str,
        with_memory: bool,
    ) -> OpResult<Job> {
        if !valid_snapshot_name(name) {
            return Err(OpError::Conflict(
                "snapshot names are 1–80 printable characters".into(),
            ));
        }
        let target = self.snapshot_target(id)?;
        if with_memory && target.qmp.is_none() {
            return Err(OpError::Conflict(
                "memory snapshots need the VM to be running".into(),
            ));
        }
        self.plan_snapshot_disks(id, &target.spec).await?;
        let this = self.clone();
        let (vm, name, description) = (
            id.to_string(),
            name.trim().to_string(),
            description.to_string(),
        );
        let title = format!(
            "Snapshot '{name}'{}",
            if with_memory { " with memory" } else { "" }
        );
        Ok(self.jobs.spawn(
            "vm.snapshot",
            &title,
            json!({"vm": id}),
            false,
            move |ctx| async move {
                let _g = this.ops.lock().await;
                let snap = this
                    .take_locked(&vm, &name, &description, with_memory, false, &ctx)
                    .await?;
                Ok(format!("Took snapshot '{}'", snap.name))
            },
        ))
    }

    /// Take a snapshot; the caller holds the lifecycle lock.
    async fn take_locked(
        &self,
        id: &str,
        name: &str,
        description: &str,
        with_memory: bool,
        auto: bool,
        ctx: &JobCtx,
    ) -> Result<SnapshotRow> {
        let target = self.snapshot_target(id).map_err(op_to_anyhow)?;
        let disks = self
            .plan_snapshot_disks(id, &target.spec)
            .await
            .map_err(op_to_anyhow)?;
        let sid = format!("{:010x}", rand::random::<u64>() & 0xff_ffff_ffff);
        let tag = format!("qui-{sid}");
        let nodes: Vec<&str> = disks.iter().map(|d| d.node.as_str()).collect();
        match (&target.qmp, with_memory) {
            (Some(qmp), true) => {
                ctx.progress(None, Some("Saving memory and disk state"));
                qmp_job(
                    qmp,
                    ctx,
                    "snapshot-save",
                    json!({"tag": tag, "vmstate": nodes[0], "devices": nodes}),
                )
                .await?;
                // snapshot-save leaves a running VM running; settle the state.
                self.refresh_status(id, qmp).await;
            }
            (Some(qmp), false) => {
                ctx.progress(None, Some("Snapshotting disks"));
                let actions: Vec<Value> = nodes
                    .iter()
                    .map(|n| json!({"type": "blockdev-snapshot-internal-sync", "data": {"device": n, "name": tag}}))
                    .collect();
                qmp.execute("transaction", Some(json!({"actions": actions})))
                    .await
                    .context("disk snapshot")?;
            }
            (None, true) => bail!("memory snapshots need the VM to be running"),
            (None, false) => {
                let qemu_img = self
                    .qemu_img()
                    .await
                    .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
                let mut done: Vec<PathBuf> = Vec::new();
                for (i, d) in disks.iter().enumerate() {
                    ctx.progress(
                        Some(i as f64 / disks.len() as f64),
                        Some(&format!("Snapshotting {}", d.name)),
                    );
                    let path = self.snapshot_file(id, d)?;
                    if let Err(e) = qemu_img_snapshot(&qemu_img, ctx, "-c", &tag, &path).await {
                        // Roll back the tags already written.
                        for p in &done {
                            let _ = qemu_img_snapshot(&qemu_img, ctx, "-d", &tag, p).await;
                        }
                        return Err(e);
                    }
                    done.push(path);
                }
            }
        }
        let row = SnapshotRow {
            id: sid.clone(),
            vm_id: id.into(),
            name: name.into(),
            description: description.into(),
            tag,
            parent_id: self.store.snapshot_head(id)?,
            with_memory,
            auto,
            disks_json: serde_json::to_string(&disks)?,
            created_at: now(),
        };
        self.store.insert_snapshot(&row)?;
        self.store.set_snapshot_head(id, Some(&sid))?;
        self.emit(
            id,
            "snapshot",
            json!({"action": "taken", "snapshot": sid, "name": name, "auto": auto}),
        );
        Ok(row)
    }

    /// Revert to a snapshot as a job, optionally taking a safety snapshot of
    /// the current state first.
    pub async fn revert_snapshot(
        self: &Arc<Self>,
        id: &str,
        sid: &str,
        safety: bool,
    ) -> OpResult<Job> {
        let snap = self.store.get_snapshot(id, sid)?.ok_or(OpError::NotFound)?;
        let target = self.snapshot_target(id)?;
        if target.qmp.is_some() {
            if !snap.with_memory {
                return Err(OpError::Conflict(
                    "this snapshot has no memory state; stop the VM to revert its disks".into(),
                ));
            }
            check_disks_match(&info_of(&snap).disks, &target.spec).map_err(OpError::Conflict)?;
        }
        let this = self.clone();
        let (vm, sid) = (id.to_string(), sid.to_string());
        let title = format!("Revert to snapshot '{}'", snap.name);
        Ok(self.jobs.spawn(
            "vm.snapshot.revert",
            &title,
            json!({"vm": id, "snapshot": sid}),
            false,
            move |ctx| async move {
                let _g = this.ops.lock().await;
                if safety {
                    let target = this.snapshot_target(&vm).map_err(op_to_anyhow)?;
                    let label = format!("Before reverting to '{}'", snap.name);
                    this.take_locked(&vm, &label, "", target.qmp.is_some(), true, &ctx)
                        .await?;
                }
                this.revert_locked(&vm, &snap, &ctx).await?;
                Ok(format!("Reverted to '{}'", snap.name))
            },
        ))
    }

    async fn revert_locked(&self, id: &str, snap: &SnapshotRow, ctx: &JobCtx) -> Result<()> {
        let target = self.snapshot_target(id).map_err(op_to_anyhow)?;
        let disks = info_of(snap).disks;
        match &target.qmp {
            Some(qmp) => {
                if !snap.with_memory {
                    bail!("this snapshot has no memory state; stop the VM to revert its disks");
                }
                check_disks_match(&disks, &target.spec).map_err(|e| anyhow!(e))?;
                let nodes: Vec<&str> = disks.iter().map(|d| d.node.as_str()).collect();
                ctx.progress(None, Some("Loading memory and disk state"));
                qmp_job(
                    qmp,
                    ctx,
                    "snapshot-load",
                    json!({"tag": snap.tag, "vmstate": nodes[0], "devices": nodes}),
                )
                .await?;
                self.refresh_status(id, qmp).await;
            }
            None => {
                let qemu_img = self
                    .qemu_img()
                    .await
                    .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
                for (i, d) in disks.iter().enumerate() {
                    ctx.progress(
                        Some(i as f64 / disks.len() as f64),
                        Some(&format!("Reverting {}", d.name)),
                    );
                    qemu_img_snapshot(&qemu_img, ctx, "-a", &snap.tag, &self.snapshot_file(id, d)?)
                        .await?;
                }
            }
        }
        self.store.set_snapshot_head(id, Some(&snap.id))?;
        self.emit(
            id,
            "snapshot",
            json!({"action": "reverted", "snapshot": snap.id, "name": snap.name}),
        );
        Ok(())
    }

    /// Delete a snapshot (its tags on every disk) as a job.
    pub async fn delete_snapshot(self: &Arc<Self>, id: &str, sid: &str) -> OpResult<Job> {
        let snap = self.store.get_snapshot(id, sid)?.ok_or(OpError::NotFound)?;
        let this = self.clone();
        let (vm, title) = (id.to_string(), format!("Delete snapshot '{}'", snap.name));
        Ok(self.jobs.spawn(
            "vm.snapshot.delete",
            &title,
            json!({"vm": id, "snapshot": sid}),
            false,
            move |ctx| {
                async move {
                    let _g = this.ops.lock().await;
                    let target = this.snapshot_target(&vm).map_err(op_to_anyhow)?;
                    let disks = info_of(&snap).disks;
                    // Files the running VM has open must go through QEMU.
                    let (open, closed): (Vec<_>, Vec<_>) = disks.iter().partition(|d| {
                        target.qmp.is_some()
                            && check_disks_match(std::slice::from_ref(*d), &target.spec).is_ok()
                    });
                    if let (Some(qmp), false) = (&target.qmp, open.is_empty()) {
                        let nodes: Vec<&str> = open.iter().map(|d| d.node.as_str()).collect();
                        qmp_job(
                            qmp,
                            &ctx,
                            "snapshot-delete",
                            json!({"tag": snap.tag, "devices": nodes}),
                        )
                        .await?;
                    }
                    if !closed.is_empty() {
                        let qemu_img = this
                            .qemu_img()
                            .await
                            .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
                        for d in closed {
                            let Ok(path) = this.snapshot_file(&vm, d) else {
                                ctx.log_line(&format!("{} no longer exists; skipped", d.name));
                                continue;
                            };
                            if let Err(e) =
                                qemu_img_snapshot(&qemu_img, &ctx, "-d", &snap.tag, &path).await
                            {
                                // A tag already gone is not a reason to keep the record.
                                ctx.log_line(&format!("{}: {e:#}", d.name));
                            }
                        }
                    }
                    this.store.delete_snapshot(&vm, &snap.id)?;
                    this.emit(
                        &vm,
                        "snapshot",
                        json!({"action": "deleted", "snapshot": snap.id, "name": snap.name}),
                    );
                    Ok(format!("Deleted snapshot '{}'", snap.name))
                }
            },
        ))
    }
}

/// While the VM runs, a snapshot's nodes must still hold the same images
/// (the spec may have been edited and the VM restarted since).
fn check_disks_match(disks: &[SnapshotDisk], spec: &VmSpec) -> std::result::Result<(), String> {
    for d in disks {
        if d.kind == "nvram" {
            if !matches!(spec.firmware, FirmwareSpec::Descriptor { .. }) {
                return Err("the VM no longer uses UEFI firmware; stop it to revert".into());
            }
            continue;
        }
        let idx: Option<usize> = d.node.strip_prefix("qui-disk").and_then(|n| n.parse().ok());
        let same = idx
            .and_then(|i| spec.disks.get(i))
            .is_some_and(|x| x.image.pool == d.pool && x.image.name == d.name);
        if !same {
            return Err(format!(
                "the VM's disks changed since this snapshot ({}/{} is no longer {}); stop the VM to revert",
                d.pool, d.name, d.node
            ));
        }
    }
    Ok(())
}

async fn qemu_img_snapshot(
    qemu_img: &Path,
    ctx: &JobCtx,
    op: &str,
    tag: &str,
    file: &Path,
) -> Result<()> {
    ctx.log_line(&format!(
        "$ qemu-img snapshot {op} {tag} {}",
        file.display()
    ));
    let out = crate::probe::command(qemu_img)
        .args(["snapshot", op, tag])
        .arg(file)
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "qemu-img snapshot {op} on {}: {}",
            file.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn op_to_anyhow(e: OpError) -> anyhow::Error {
    match e {
        OpError::Invalid(d) => anyhow!(
            "invalid configuration: {}",
            d.iter()
                .map(|x| x.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
        OpError::Conflict(m) => anyhow!(m),
        OpError::NotFound => anyhow!("no such VM"),
        OpError::Failed(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qemu_ui_core::spec::*;
    use std::collections::BTreeMap;

    #[test]
    fn disks_must_still_match_while_running() {
        let disk = |n: &str| DiskSpec {
            image: ImageRef {
                pool: "default".into(),
                name: n.into(),
            },
            bus: DiskBus::Virtio,
            media: Media::Disk,
            readonly: false,
            boot_index: None,
            cache: CacheMode::None,
            aio: None,
            discard: true,
        };
        let spec = VmSpec {
            spec_version: SPEC_VERSION,
            name: "t".into(),
            arch: "x86_64".into(),
            machine: MachineSpec {
                type_name: "pc-q35-11.1".into(),
                props: BTreeMap::new(),
            },
            accel: "auto".into(),
            cpu: CpuSpec {
                model: None,
                props: BTreeMap::new(),
                count: 1,
                topology: None,
            },
            memory: MemorySpec { size_mib: 128 },
            firmware: FirmwareSpec::Default,
            disks: vec![disk("a.qcow2"), disk("b.qcow2")],
            nics: vec![],
            display: DisplaySpec::default(),
            input: InputSpec::default(),
            passthrough: vec![],
        };
        let snap = |name: &str, node: &str| SnapshotDisk {
            kind: "image".into(),
            pool: "default".into(),
            name: name.into(),
            node: node.into(),
        };
        assert!(
            check_disks_match(
                &[snap("a.qcow2", "qui-disk0"), snap("b.qcow2", "qui-disk1")],
                &spec
            )
            .is_ok()
        );
        let e = check_disks_match(&[snap("a.qcow2", "qui-disk1")], &spec).unwrap_err();
        assert!(e.contains("disks changed"), "{e}");
        let nv = SnapshotDisk {
            kind: "nvram".into(),
            pool: String::new(),
            name: "VARS.qcow2".into(),
            node: "qui-fw-vars".into(),
        };
        assert!(check_disks_match(&[nv], &spec).is_err(), "UEFI removed");
    }
}

/// Snapshots against a real QEMU (skipped without one).
#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;
    use qemu_ui_core::spec::*;
    use qemu_ui_core::storage::JobStatus;

    async fn done(sv: &Supervisor, job: OpResult<Job>) -> Job {
        let job = job.map_err(|e| format!("{e:?}")).unwrap();
        let j = sv
            .jobs
            .wait(&job.id, Duration::from_secs(120))
            .await
            .unwrap();
        assert_eq!(
            j.status,
            JobStatus::Succeeded,
            "{} failed: {:?}",
            j.title,
            j.error
        );
        j
    }

    fn tags(img: &Path, file: &Path) -> Vec<String> {
        let out = std::process::Command::new(img)
            .args(["snapshot", "-U", "-l"])
            .arg(file)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
            .filter(|t| t.starts_with("qui-"))
            .collect()
    }

    /// Offline qemu-io on an image file; true when the command succeeded and
    /// (for reads) the pattern matched.
    fn qemu_io(file: &Path, cmd: &str) -> bool {
        let qio = find("qemu-io").expect("qemu-io next to qemu-img");
        let out = std::process::Command::new(qio)
            .args(["-f", "qcow2", "-c", cmd])
            .arg(file)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out.status.success() && !text.contains("Pattern verification failed")
    }

    /// qemu-io through the running VM's monitor.
    async fn live_io(qmp: &Qmp, cmd: &str) -> bool {
        let r = qmp
            .execute(
                "human-monitor-command",
                Some(json!({"command-line": format!("qemu-io qui-dev0 \"{cmd}\"")})),
            )
            .await
            .unwrap();
        !r.as_str()
            .unwrap_or_default()
            .contains("Pattern verification failed")
    }

    #[test]
    fn offline_and_live_snapshots() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        if find("qemu-io").is_none() {
            eprintln!("skipping: qemu-io not found");
            return;
        }
        let data = short_data_dir("snap");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "snappy").await;
            let disk = sv
                .pools
                .resolve(&ImageRef {
                    pool: "default".into(),
                    name: "snappy.qcow2".into(),
                })
                .unwrap();

            // ---- offline: s1, marker, s2, revert to s1 (with safety), delete s2
            done(&sv, sv.take_snapshot(&id, "s1", "clean disk", false).await).await;
            assert!(qemu_io(&disk, "write -P 0xab 0 4k"));
            done(&sv, sv.take_snapshot(&id, "s2", "", false).await).await;
            let list = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let (s1, s2) = (&list.snapshots[0], &list.snapshots[1]);
            assert_eq!(s2.parent_id.as_deref(), Some(s1.id.as_str()), "tree");
            assert_eq!(list.head.as_deref(), Some(s2.id.as_str()));
            assert_eq!(tags(&img, &disk).len(), 2);
            assert!(
                sv.take_snapshot(&id, "mem", "", true).await.is_err(),
                "memory needs a running VM"
            );

            done(&sv, sv.revert_snapshot(&id, &s1.id, true).await).await;
            assert!(
                qemu_io(&disk, "read -P 0 0 4k"),
                "marker gone after revert to s1"
            );
            let list = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let safety = list
                .snapshots
                .iter()
                .find(|s| s.auto)
                .expect("safety snapshot");
            assert_eq!(safety.parent_id.as_deref(), Some(s2.id.as_str()));
            assert_eq!(list.head.as_deref(), Some(s1.id.as_str()));
            // The safety snapshot kept the marker.
            done(&sv, sv.revert_snapshot(&id, &safety.id, false).await).await;
            assert!(
                qemu_io(&disk, "read -P 0xab 0 4k"),
                "marker back from safety snapshot"
            );

            let s2_tag = s2.tag.clone();
            done(&sv, sv.delete_snapshot(&id, &s2.id).await).await;
            assert!(
                !tags(&img, &disk).contains(&s2_tag),
                "tag removed from the image"
            );
            let list = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let safety = list.snapshots.iter().find(|s| s.auto).unwrap();
            assert_eq!(
                safety.parent_id.as_deref(),
                Some(s1.id.as_str()),
                "re-parented to s1"
            );

            // ---- live: memory snapshot, marker through the VM, revert
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let qmp = sv.qmp_of(&id).map_err(|e| format!("{e:?}")).unwrap();
            assert!(live_io(&qmp, "write -P 0x00 0 4k").await);
            done(
                &sv,
                sv.take_snapshot(&id, "live", "with memory", true).await,
            )
            .await;
            assert_eq!(
                sv.detail(&id).ok().unwrap().summary.state,
                VmState::Running,
                "still running after save"
            );
            assert!(live_io(&qmp, "write -P 0xcd 0 4k").await);
            assert!(live_io(&qmp, "read -P 0xcd 0 4k").await);
            let live = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let live = live
                .snapshots
                .iter()
                .find(|s| s.name == "live")
                .unwrap()
                .clone();
            assert!(live.with_memory);
            done(&sv, sv.revert_snapshot(&id, &live.id, true).await).await;
            assert!(
                live_io(&qmp, "read -P 0x00 0 4k").await,
                "disk state restored by snapshot-load"
            );
            wait_state(&sv, &id, VmState::Running).await;

            // Disk-only snapshot while running; reverting it live is refused.
            done(
                &sv,
                sv.take_snapshot(&id, "crash-consistent", "", false).await,
            )
            .await;
            let list = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let dsk = list
                .snapshots
                .iter()
                .find(|s| s.name == "crash-consistent")
                .unwrap();
            match sv.revert_snapshot(&id, &dsk.id, true).await {
                Err(OpError::Conflict(m)) => assert!(m.contains("stop the VM"), "{m}"),
                other => panic!("expected refusal, got {:?}", other.map(|j| j.title)),
            }
            // Live delete goes through QEMU (the image is locked by the VM).
            done(&sv, sv.delete_snapshot(&id, &live.id).await).await;
            assert!(!tags(&img, &disk).contains(&live.tag));

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;

            // ---- raw writable disk: refused with the disk named
            let raw = ImageRef {
                pool: "default".into(),
                name: "data.raw".into(),
            };
            sv.pools.create(&img, &raw, 8 << 20, "raw").await.unwrap();
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.disks.push(DiskSpec {
                image: raw,
                bus: DiskBus::Virtio,
                media: Media::Disk,
                readonly: false,
                boot_index: None,
                cache: CacheMode::None,
                aio: None,
                discard: true,
            });
            sv.update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            match sv.take_snapshot(&id, "nope", "", false).await {
                Err(OpError::Conflict(m)) => {
                    assert!(m.contains("disk 1 (default/data.raw) is raw"), "{m}")
                }
                other => panic!("expected refusal, got {:?}", other.map(|j| j.title)),
            }
            let reason = sv
                .list_snapshots(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap()
                .unsupported_reason;
            assert!(reason.unwrap().contains("data.raw"));
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
