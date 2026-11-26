//! VM backup/export bundles (X-03).
//!
//! Export writes `<data>/backups/<name>.qemu-ui.tar` (format in
//! `qemu_ui_core::backup`) in a job:
//!
//! - A stopped VM's disks are locked against a start for the duration and
//!   copied with `qemu-img convert` (flattening backing chains), or copied
//!   byte for byte when internal snapshots are to be kept.
//! - A running VM's disks are copied by QEMU itself: one `transaction` of
//!   `blockdev-backup` (sync=full) jobs gives a single point in time across
//!   every disk. The guest's file systems are frozen through the guest agent
//!   around that point when it answers, so the copy is quiesced; without an
//!   agent it is crash-consistent.
//!
//! Import unpacks a bundle into a pool as a new VM, checking every member's
//! size and SHA-256 against the manifest as it streams.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::backup::{
    BUNDLE_EXT, BUNDLE_FORMAT, BUNDLE_VERSION, BackupInfo, BundleFile, BundleManifest, BundleRole,
    MANIFEST_NAME, bundle_disks, check_manifest, valid_bundle_name,
};
use qemu_ui_core::compile::{Diagnostic, Severity};
use qemu_ui_core::spec::{ImageRef, Media, VmSpec, disk_node, valid_name};
use qemu_ui_core::storage::Job;
use qemu_ui_core::vm::VmState;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{OpError, OpResult, Supervisor, now, path_str};
use crate::jobs::JobCtx;

/// Largest manifest an import will read.
const MAX_MANIFEST: u64 = 16 << 20;
const COPY_BUF: usize = 1 << 20;

fn invalid(field: &str, rule: &str, message: String) -> OpError {
    OpError::Invalid(vec![Diagnostic {
        severity: Severity::Error,
        field: field.into(),
        rule: rule.into(),
        message,
    }])
}

/// SHA-256 and length of a file.
fn hash_file(path: &Path, ctx: &JobCtx) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut n = 0u64;
    loop {
        ctx.check_cancelled()?;
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

/// A reader that reports progress and stops on cancellation.
struct Counting<'a, R> {
    inner: R,
    ctx: &'a JobCtx,
    done: &'a mut u64,
    total: u64,
    label: &'a str,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ctx.is_cancelled() {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        *self.done += n as u64;
        if self.total > 0 {
            self.ctx.progress(
                Some((*self.done as f64 / self.total as f64).min(1.0)),
                Some(self.label),
            );
        }
        Ok(n)
    }
}

/// Read a bundle's manifest (its first member) without unpacking anything.
pub fn read_manifest(path: &Path) -> Result<BundleManifest> {
    let f = std::fs::File::open(path)?;
    let mut ar = tar::Archive::new(f);
    let mut entries = ar.entries()?;
    let mut first = entries
        .next()
        .ok_or_else(|| anyhow!("the bundle is empty"))??;
    if first.path()?.to_string_lossy() != MANIFEST_NAME {
        bail!("not a qemu-ui bundle: the first member is not {MANIFEST_NAME}");
    }
    if first.header().size()? > MAX_MANIFEST {
        bail!("the bundle's manifest is too large");
    }
    let mut s = String::new();
    first.read_to_string(&mut s)?;
    let m: BundleManifest =
        serde_json::from_str(&s).context("the bundle's manifest is unreadable")?;
    check_manifest(&m).map_err(|e| anyhow!(e))?;
    Ok(m)
}

fn unix_mtime(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

impl Supervisor {
    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    /// Path of bundle `name`, which must be a valid bundle name.
    pub fn backup_path(&self, name: &str) -> OpResult<PathBuf> {
        if !valid_bundle_name(name) {
            return Err(invalid(
                "name",
                "backup.name",
                format!("a bundle name is <letters, digits, '.', '_', '-'>{BUNDLE_EXT}"),
            ));
        }
        Ok(self.backups_dir().join(name))
    }

    /// Bundles in the backups directory, newest first.
    pub async fn list_backups(&self) -> Result<Vec<BackupInfo>> {
        let dir = self.backups_dir();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let Ok(rd) = std::fs::read_dir(&dir) else {
                return Ok(out);
            };
            for e in rd.flatten() {
                let Ok(name) = e.file_name().into_string() else {
                    continue;
                };
                if !valid_bundle_name(&name) {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let (manifest, error) = match read_manifest(&e.path()) {
                    Ok(m) => (Some(m), None),
                    Err(err) => (None, Some(format!("{err:#}"))),
                };
                out.push(BackupInfo {
                    name,
                    size: meta.len(),
                    modified_at: unix_mtime(&meta),
                    manifest,
                    error,
                });
            }
            out.sort_by(|a, b| b.modified_at.cmp(&a.modified_at).then(a.name.cmp(&b.name)));
            Ok(out)
        })
        .await?
    }

    pub fn delete_backup(&self, name: &str) -> OpResult<()> {
        let path = self.backup_path(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(OpError::NotFound),
            Err(e) => Err(OpError::Failed(e.into())),
        }
    }

    /// Export VM `id` to a bundle, in a job. `snapshots`: copy stopped VMs'
    /// qcow2 images as they are, keeping internal snapshots.
    pub async fn export_vm(self: &Arc<Self>, id: &str, snapshots: bool) -> OpResult<Job> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| OpError::Failed(anyhow!("qemu-img is not installed")))?;
        let live_qmp = self.live_qmp(id);
        let state = VmState::parse(&row.state);
        if live_qmp.is_none() && state.is_active() {
            return Err(OpError::Conflict(format!(
                "the VM is {}; export it when it is running or stopped",
                row.state
            )));
        }
        // A running VM's configuration is its run's (hot-plugged disks).
        let spec_json = match &live_qmp {
            Some(_) => self
                .store
                .last_run(id)?
                .filter(|r| r.ended_at.is_none())
                .map(|r| r.spec_json)
                .unwrap_or(row.spec_json.clone()),
            None => row.spec_json.clone(),
        };
        let spec: VmSpec =
            serde_json::from_str(&spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let disks = bundle_disks(&spec).map_err(|m| invalid("disks", "backup.host-disk", m))?;
        if live_qmp.is_some()
            && let Some(why) = self.live_overlay_conflict(id)
        {
            return Err(OpError::Conflict(format!(
                "{why}; restart the VM or stop it to export"
            )));
        }
        let mut sources = Vec::new();
        for &i in &disks {
            sources.push((i, self.pools.resolve(&spec.disks[i].image)?));
        }

        std::fs::create_dir_all(self.backups_dir()).map_err(|e| OpError::Failed(e.into()))?;
        let stem: String = row
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let stamp = now();
        let mut bundle_name = format!("{stem}-{stamp}{BUNDLE_EXT}");
        let mut k = 1;
        while self.backups_dir().join(&bundle_name).exists() {
            bundle_name = format!("{stem}-{stamp}-{k}{BUNDLE_EXT}");
            k += 1;
        }
        let live = live_qmp.is_some();
        let this = self.clone();
        let vm_id = id.to_string();
        let vm_name = row.name.clone();
        let title = format!("Export {} to {bundle_name}", row.name);
        let job = self.jobs.spawn(
            "vm.export",
            &title,
            json!({"vm": id, "bundle": bundle_name}),
            true,
            move |ctx| async move {
                let staging = this.backups_dir().join(format!(".export-{}", ctx.id));
                let res = this
                    .export_job(
                        &ctx,
                        &vm_id,
                        &vm_name,
                        spec,
                        sources,
                        live_qmp,
                        snapshots && !live,
                        &qemu_img,
                        &staging,
                        &bundle_name,
                    )
                    .await;
                let _ = std::fs::remove_dir_all(&staging);
                if res.is_err() {
                    let _ = std::fs::remove_file(
                        this.backups_dir().join(format!("{bundle_name}.part")),
                    );
                }
                res
            },
        );
        Ok(job)
    }

    #[allow(clippy::too_many_arguments)]
    async fn export_job(
        &self,
        ctx: &Arc<JobCtx>,
        id: &str,
        vm_name: &str,
        spec: VmSpec,
        sources: Vec<(usize, PathBuf)>,
        live_qmp: Option<qemu_ui_qmp::Qmp>,
        as_is: bool,
        qemu_img: &Path,
        staging: &Path,
        bundle_name: &str,
    ) -> Result<String> {
        std::fs::create_dir_all(staging.join("disks"))?;
        let mut members: Vec<(PathBuf, String, BundleRole)> = Vec::new();
        let mut quiesced = false;
        let mut kept_snapshots = false;

        match &live_qmp {
            None => {
                let images: Vec<ImageRef> = sources
                    .iter()
                    .map(|(i, _)| spec.disks[*i].image.clone())
                    .collect();
                // Keep the VM (and anything else) from opening the images
                // while they are copied.
                let _locks = crate::image_ops::lock_for_start(&images).map_err(|e| anyhow!(e))?;
                if self.live_qmp(id).is_some() {
                    bail!("the VM was started meanwhile; export again");
                }
                let total = sources.len();
                for (k, (i, src)) in sources.iter().enumerate() {
                    let rel = format!("disks/disk{i}.qcow2");
                    let dst = staging.join(&rel);
                    ctx.progress(None, Some(&format!("Copying disk {} of {total}", k + 1)));
                    let info = self.pools.info_json(qemu_img, src).await?;
                    let plain_qcow2 =
                        info["format"] == "qcow2" && info.get("backing-filename").is_none();
                    if as_is && plain_qcow2 {
                        let (s, d) = (src.clone(), dst.clone());
                        tokio::task::spawn_blocking(move || std::fs::copy(s, d)).await??;
                        kept_snapshots = true;
                    } else {
                        let (f, t) = (path_str(src), path_str(&dst));
                        ctx.run_qemu_img(qemu_img, &["convert", "-O", "qcow2", &f, &t])
                            .await?;
                    }
                    members.push((dst, rel, BundleRole::Disk { index: *i }));
                }
                // The locks drop here: the images are copied.
            }
            Some(qmp) => {
                quiesced = self
                    .live_backup(ctx, qmp, &spec, &sources, qemu_img, staging, &mut members)
                    .await?;
            }
        }
        ctx.check_cancelled()?;

        let vm_dir = self.vm_dir(id);
        let vars = vm_dir.join("VARS.qcow2");
        if vars.is_file() {
            std::fs::create_dir_all(staging.join("nvram"))?;
            let dst = staging.join("nvram/VARS.qcow2");
            std::fs::copy(&vars, &dst).context("copying the UEFI variables")?;
            members.push((dst, "nvram/VARS.qcow2".into(), BundleRole::Nvram));
        }
        let tpm = vm_dir.join("tpm");
        if spec.tpm.is_some() && tpm.is_dir() {
            std::fs::create_dir_all(staging.join("tpm"))?;
            for e in std::fs::read_dir(&tpm)?.flatten() {
                let Ok(name) = e.file_name().into_string() else {
                    continue;
                };
                // Sockets, pid and lock files are not state.
                if !e.file_type().is_ok_and(|t| t.is_file()) || name.ends_with(".lock") {
                    continue;
                }
                let dst = staging.join("tpm").join(&name);
                std::fs::copy(e.path(), &dst).context("copying the TPM state")?;
                members.push((dst, format!("tpm/{name}"), BundleRole::Tpm));
            }
        }

        // Checksums, then the manifest, then the tar.
        let mut files = Vec::new();
        for (path, rel, role) in &members {
            ctx.progress(None, Some(&format!("Checksumming {rel}")));
            let (p, c) = (path.clone(), ctx.clone());
            let (sha256, size) = tokio::task::spawn_blocking(move || hash_file(&p, &c)).await??;
            files.push(BundleFile {
                path: rel.clone(),
                role: role.clone(),
                size,
                sha256,
            });
        }
        let manifest = BundleManifest {
            format: BUNDLE_FORMAT.into(),
            version: BUNDLE_VERSION,
            created_at: now(),
            source_name: vm_name.into(),
            spec,
            files,
            live: live_qmp.is_some(),
            quiesced,
            snapshots_included: kept_snapshots,
        };
        check_manifest(&manifest).map_err(|e| anyhow!("internal: {e}"))?;
        let part = self.backups_dir().join(format!("{bundle_name}.part"));
        let fin = self.backups_dir().join(bundle_name);
        let c = ctx.clone();
        let m = manifest.clone();
        tokio::task::spawn_blocking(move || write_tar(&part, &fin, &m, &members, &c)).await??;
        let total: u64 = manifest.files.iter().map(|f| f.size).sum();
        Ok(format!(
            "Exported to {bundle_name} ({} MiB){}",
            total >> 20,
            match (manifest.live, manifest.quiesced) {
                (true, true) => ", quiesced through the guest agent",
                (true, false) => ", crash-consistent (no guest agent answered)",
                _ => "",
            }
        ))
    }

    /// Point-in-time copy of a running VM's disks into `staging`. Returns
    /// whether the guest's file systems were frozen for it.
    #[allow(clippy::too_many_arguments)]
    async fn live_backup(
        &self,
        ctx: &Arc<JobCtx>,
        qmp: &qemu_ui_qmp::Qmp,
        spec: &VmSpec,
        sources: &[(usize, PathBuf)],
        qemu_img: &Path,
        staging: &Path,
        members: &mut Vec<(PathBuf, String, BundleRole)>,
    ) -> Result<bool> {
        let tag = &ctx.id;
        let mut added: Vec<String> = Vec::new();
        let mut jobs: Vec<String> = Vec::new();
        let mut actions = Vec::new();
        let res: Result<bool> = async {
            for (k, (i, src)) in sources.iter().enumerate() {
                let rel = format!("disks/disk{i}.qcow2");
                let dst = staging.join(&rel);
                let (_, size, _) = self.pools.info(qemu_img, src).await?;
                let out = crate::probe::command(qemu_img)
                    .args(["create", "-q", "-f", "qcow2"])
                    .arg(&dst)
                    .arg(size.to_string())
                    .output()
                    .await?;
                if !out.status.success() {
                    bail!(
                        "qemu-img create: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                let node = format!("qui-bk-{tag}-{k}");
                qmp.execute(
                    "blockdev-add",
                    Some(json!({
                        "driver": "qcow2",
                        "node-name": node,
                        "file": {"driver": "file", "filename": path_str(&dst)},
                    })),
                )
                .await
                .map_err(|e| anyhow!("adding the backup target for disk {i}: {e}"))?;
                added.push(node.clone());
                let job = format!("qui-bkjob-{tag}-{k}");
                actions.push(json!({"type": "blockdev-backup", "data": {
                    "job-id": job,
                    "device": disk_node(spec, *i),
                    "target": node,
                    "sync": "full",
                    "auto-dismiss": false,
                }}));
                jobs.push(job);
                members.push((dst, rel, BundleRole::Disk { index: *i }));
            }

            // One point in time for every disk, quiesced when the guest agent
            // answers.
            let agent = crate::qga::socket(qmp).await;
            let mut frozen = false;
            if let Some(sock) = &agent {
                match crate::qga::freeze(sock).await {
                    Ok(n) => {
                        frozen = true;
                        ctx.progress(None, Some(&format!("Froze {n} guest file systems")));
                    }
                    Err(e) => {
                        tracing::info!("export not quiesced: {e:#}");
                        let _ = crate::qga::thaw(sock).await;
                    }
                }
            }
            let started = qmp
                .execute("transaction", Some(json!({"actions": actions})))
                .await;
            if frozen
                && let Some(sock) = &agent
                && let Err(e) = crate::qga::thaw(sock).await
            {
                tracing::warn!("thawing the guest file systems: {e:#}");
            }
            if let Err(e) = started {
                jobs.clear();
                bail!("QEMU refused the backup: {e}");
            }
            self.wait_block_jobs(ctx, qmp, &jobs).await?;
            Ok(frozen)
        }
        .await;
        // Cancel what may still run, then drop every job and target.
        for j in &jobs {
            let _ = qmp.execute("job-cancel", Some(json!({"id": j}))).await;
        }
        for _ in 0..50 {
            let Ok(v) = qmp.execute("query-jobs", None).await else {
                break;
            };
            let ours: Vec<&serde_json::Value> = v
                .as_array()
                .into_iter()
                .flatten()
                .filter(|j| jobs.iter().any(|x| j["id"] == x.as_str()))
                .collect();
            if ours.iter().all(|j| j["status"] == "concluded") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for j in &jobs {
            let _ = qmp.execute("job-dismiss", Some(json!({"id": j}))).await;
        }
        for n in &added {
            let _ = qmp
                .execute("blockdev-del", Some(json!({"node-name": n})))
                .await;
        }
        res
    }

    /// Wait for QEMU block jobs `ids` to conclude, reporting progress.
    async fn wait_block_jobs(
        &self,
        ctx: &JobCtx,
        qmp: &qemu_ui_qmp::Qmp,
        ids: &[String],
    ) -> Result<()> {
        loop {
            ctx.check_cancelled()?;
            let v = qmp
                .execute("query-jobs", None)
                .await
                .map_err(|e| anyhow!("query-jobs: {e}"))?;
            let ours: Vec<&serde_json::Value> = v
                .as_array()
                .into_iter()
                .flatten()
                .filter(|j| ids.iter().any(|x| j["id"] == x.as_str()))
                .collect();
            if ours.len() < ids.len() {
                bail!("a backup job disappeared (did the VM stop?)");
            }
            let (mut cur, mut tot) = (0u64, 0u64);
            for j in &ours {
                cur += j["current-progress"].as_u64().unwrap_or(0);
                tot += j["total-progress"].as_u64().unwrap_or(0);
            }
            if tot > 0 {
                ctx.progress(
                    Some(cur as f64 / tot as f64),
                    Some("Copying the disks of the running VM"),
                );
            }
            if ours.iter().all(|j| j["status"] == "concluded") {
                if let Some(e) = ours.iter().find_map(|j| j["error"].as_str()) {
                    bail!("the backup failed in QEMU: {e}");
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Import bundle `bundle` as a new VM `name` with its disks in `pool`.
    pub async fn import_bundle(
        self: &Arc<Self>,
        bundle: &str,
        name: String,
        pool: String,
    ) -> OpResult<Job> {
        let path = self.backup_path(bundle)?;
        if !path.is_file() {
            return Err(OpError::NotFound);
        }
        let p = path.clone();
        let manifest = tokio::task::spawn_blocking(move || read_manifest(&p))
            .await
            .map_err(|e| OpError::Failed(e.into()))?
            .map_err(|e| invalid("bundle", "backup.bundle", format!("{e:#}")))?;
        if !valid_name(&name) {
            return Err(invalid(
                "name",
                "name.invalid",
                "use 1–63 letters, digits, '.', '_' or '-', starting with a letter or digit".into(),
            ));
        }
        if self.store.vm_name_taken(&name, None)? {
            return Err(OpError::Conflict(format!(
                "a VM named '{name}' already exists"
            )));
        }
        if self.pools.get(&pool)?.readonly {
            return Err(OpError::Conflict(format!("pool '{pool}' is read-only")));
        }
        let need: u64 = manifest
            .files
            .iter()
            .filter(|f| matches!(f.role, BundleRole::Disk { .. }))
            .map(|f| f.size)
            .sum();
        let qemu_img = self.qemu_img().await;
        self.pools
            .check_quota(qemu_img.as_deref(), &pool, need)
            .await?;

        // Where each disk goes.
        let mut targets: HashMap<usize, ImageRef> = HashMap::new();
        let mut taken: Vec<String> = Vec::new();
        for f in &manifest.files {
            if let BundleRole::Disk { index } = f.role {
                let mut n = self
                    .pools
                    .free_name(&pool, &format!("{name}-disk{index}"), "qcow2")?;
                while taken.contains(&n) {
                    n = self.pools.free_name(
                        &pool,
                        &format!("{name}-disk{index}-{}", taken.len()),
                        "qcow2",
                    )?;
                }
                taken.push(n.clone());
                targets.insert(
                    index,
                    ImageRef {
                        pool: pool.clone(),
                        name: n,
                    },
                );
            }
        }
        let this = self.clone();
        let title = format!("Import {bundle} as {name}");
        let job = self.jobs.spawn(
            "vm.import-bundle",
            &title,
            json!({"bundle": bundle, "name": name}),
            true,
            move |ctx| async move {
                let staging = this.backups_dir().join(format!(".import-{}", ctx.id));
                let mut made: Vec<PathBuf> = Vec::new();
                let res = this
                    .import_job(&ctx, &path, manifest, name, targets, &staging, &mut made)
                    .await;
                let _ = std::fs::remove_dir_all(&staging);
                if res.is_err() {
                    made.iter().for_each(|p| drop(std::fs::remove_file(p)));
                }
                res
            },
        );
        Ok(job)
    }

    #[allow(clippy::too_many_arguments)]
    async fn import_job(
        &self,
        ctx: &Arc<JobCtx>,
        bundle: &Path,
        manifest: BundleManifest,
        name: String,
        targets: HashMap<usize, ImageRef>,
        staging: &Path,
        made: &mut Vec<PathBuf>,
    ) -> Result<String> {
        // Destination of each member.
        let mut dest: HashMap<String, PathBuf> = HashMap::new();
        for f in &manifest.files {
            let p = match &f.role {
                BundleRole::Disk { index } => {
                    let p = self.pools.new_image_path(&targets[index])?;
                    made.push(p.clone());
                    p
                }
                BundleRole::Nvram | BundleRole::Tpm => staging.join(&f.path),
            };
            dest.insert(f.path.clone(), p);
        }
        std::fs::create_dir_all(staging.join("nvram"))?;
        std::fs::create_dir_all(staging.join("tpm"))?;
        let (b, c, m) = (bundle.to_path_buf(), ctx.clone(), manifest.clone());
        tokio::task::spawn_blocking(move || unpack(&b, &m, &dest, &c)).await??;
        ctx.check_cancelled()?;

        let mut spec = manifest.spec.clone();
        spec.name = name.clone();
        for nic in &mut spec.nics {
            nic.mac = None;
        }
        for (i, r) in &targets {
            spec.disks[*i].image = r.clone();
        }
        // Read-only media are not in bundles: keep those this host has.
        let before = spec.disks.len();
        let pools = self.pools.clone();
        spec.disks.retain(|d| {
            !(d.readonly || d.media == Media::Cdrom)
                || targets.values().any(|t| t == &d.image)
                || pools.resolve(&d.image).is_ok_and(|p| p.exists())
        });
        let dropped = before - spec.disks.len();
        let vm = self
            .create(spec)
            .await
            .map_err(|e| anyhow!("creating the VM: {e:?}"))?;
        let dir = self.vm_dir(&vm.summary.id);
        let vars = staging.join("nvram/VARS.qcow2");
        if vars.is_file() {
            std::fs::create_dir_all(&dir)?;
            std::fs::copy(&vars, dir.join("VARS.qcow2"))?;
        }
        let tpm_files: Vec<&BundleFile> = manifest
            .files
            .iter()
            .filter(|f| f.role == BundleRole::Tpm)
            .collect();
        if !tpm_files.is_empty() {
            std::fs::create_dir_all(dir.join("tpm"))?;
            for f in tpm_files {
                let file = f.path.trim_start_matches("tpm/");
                std::fs::copy(staging.join(&f.path), dir.join("tpm").join(file))?;
            }
        }
        made.clear(); // the VM owns the images now
        self.emit(
            &vm.summary.id,
            "imported",
            json!({"bundle": bundle.file_name().map(|n| n.to_string_lossy().to_string())}),
        );
        Ok(match dropped {
            0 => format!("Created {}", vm.summary.name),
            n => format!(
                "Created {} ({n} read-only medium/media not found on this host were left out)",
                vm.summary.name
            ),
        })
    }
}

/// Write the tar: the manifest first, then every member.
fn write_tar(
    part: &Path,
    fin: &Path,
    manifest: &BundleManifest,
    members: &[(PathBuf, String, BundleRole)],
    ctx: &JobCtx,
) -> Result<()> {
    let total: u64 = manifest.files.iter().map(|f| f.size).sum();
    let mut done = 0u64;
    let out = std::fs::File::create(part)?;
    let mut tb = tar::Builder::new(std::io::BufWriter::new(out));
    let mtime = manifest.created_at;
    let json = serde_json::to_vec_pretty(manifest)?;
    let mut h = tar::Header::new_gnu();
    h.set_size(json.len() as u64);
    h.set_mode(0o600);
    h.set_mtime(mtime);
    h.set_entry_type(tar::EntryType::Regular);
    tb.append_data(&mut h, MANIFEST_NAME, json.as_slice())?;
    for (path, rel, _) in members {
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        let mut h = tar::Header::new_gnu();
        h.set_size(len);
        h.set_mode(0o600);
        h.set_mtime(mtime);
        h.set_entry_type(tar::EntryType::Regular);
        let label = format!("Writing {rel}");
        let r = Counting {
            inner: f,
            ctx,
            done: &mut done,
            total,
            label: &label,
        };
        tb.append_data(&mut h, rel, r)?;
    }
    let mut w = tb.into_inner()?;
    w.flush()?;
    w.into_inner()
        .map_err(|e| anyhow!("writing the bundle: {}", e.error()))?
        .sync_all()?;
    std::fs::rename(part, fin)?;
    Ok(())
}

/// Stream every member to its destination, checking size and SHA-256.
fn unpack(
    bundle: &Path,
    manifest: &BundleManifest,
    dest: &HashMap<String, PathBuf>,
    ctx: &JobCtx,
) -> Result<()> {
    let by_path: HashMap<&str, &BundleFile> = manifest
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let total: u64 = manifest.files.iter().map(|f| f.size).sum();
    let mut done = 0u64;
    let mut seen = Vec::new();
    let mut ar = tar::Archive::new(std::fs::File::open(bundle)?);
    for (k, e) in ar.entries()?.enumerate() {
        let mut e = e?;
        let p = e.path()?.to_string_lossy().to_string();
        if k == 0 {
            if p != MANIFEST_NAME {
                bail!("not a qemu-ui bundle");
            }
            continue;
        }
        let Some(f) = by_path.get(p.as_str()) else {
            bail!("the bundle has a member the manifest doesn't list: '{p}'");
        };
        if e.header().entry_type() != tar::EntryType::Regular {
            bail!("bundle member '{p}' is not a regular file");
        }
        if seen.contains(&p) {
            bail!("bundle member '{p}' appears twice");
        }
        let to = &dest[&p];
        let mut out = std::fs::File::create(to)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; COPY_BUF];
        let mut n = 0u64;
        let label = format!("Unpacking {p}");
        loop {
            ctx.check_cancelled()?;
            let r = e.read(&mut buf)?;
            if r == 0 {
                break;
            }
            h.update(&buf[..r]);
            out.write_all(&buf[..r])?;
            n += r as u64;
            done += r as u64;
            if total > 0 {
                ctx.progress(Some((done as f64 / total as f64).min(1.0)), Some(&label));
            }
        }
        out.sync_all()?;
        if n != f.size || hex::encode(h.finalize()) != f.sha256 {
            bail!("bundle member '{p}' is damaged: its checksum doesn't match the manifest");
        }
        seen.push(p);
    }
    if let Some(f) = manifest.files.iter().find(|f| !seen.contains(&f.path)) {
        bail!("the bundle is incomplete: '{}' is missing", f.path);
    }
    Ok(())
}

/// Export and import against a real QEMU: offline and live exports, a
/// round trip that keeps the disk's content, and a damaged bundle refused.
#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use qemu_ui_core::storage::JobStatus;
    use qemu_ui_core::vm::VmState;
    use std::time::Duration;

    async fn finish(
        sv: &super::Supervisor,
        job: &qemu_ui_core::storage::Job,
    ) -> qemu_ui_core::storage::Job {
        sv.jobs
            .wait(&job.id, Duration::from_secs(120))
            .await
            .unwrap()
    }

    /// `qemu-img compare` exit code: 0 identical content, 1 different.
    async fn compare(img: &std::path::Path, a: &std::path::Path, b: &std::path::Path) -> i32 {
        crate::probe::command(img)
            .args(["compare", "-q"])
            .arg(a)
            .arg(b)
            .status()
            .await
            .unwrap()
            .code()
            .unwrap_or(-1)
    }

    #[test]
    fn export_import_round_trip_live_and_damaged() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("bk");
        let _kill = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "src").await;
            let src_ref = sv.detail(&id).ok().unwrap().spec.disks[0].image.clone();
            let src = sv.pools.resolve(&src_ref).unwrap();

            // Give the disk recognisable content.
            let raw = data.join("marker.raw");
            let mut bytes = vec![0u8; 1 << 20];
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            std::fs::write(&raw, &bytes).unwrap();
            let st = crate::probe::command(&img)
                .args(["convert", "-f", "raw", "-O", "qcow2"])
                .arg(&raw)
                .arg(&src)
                .status()
                .await
                .unwrap();
            assert!(st.success());

            // Offline export.
            let job = sv
                .export_vm(&id, false)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let done = finish(&sv, &job).await;
            assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
            let list = sv.list_backups().await.unwrap();
            assert_eq!(list.len(), 1, "{list:?}");
            let bundle = list[0].name.clone();
            let m = list[0].manifest.clone().expect("readable bundle");
            assert!(!m.live);
            assert_eq!(m.source_name, "src");

            // Import it as a new VM: same disk content, new image.
            let job = sv
                .import_bundle(&bundle, "restored".into(), "default".into())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let done = finish(&sv, &job).await;
            assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
            let vms = sv.list().unwrap();
            let restored = vms
                .iter()
                .find(|v| v.name == "restored")
                .expect("imported VM");
            let r_ref = sv.detail(&restored.id).ok().unwrap().spec.disks[0]
                .image
                .clone();
            assert_ne!(r_ref, src_ref);
            assert_eq!(
                compare(&img, &src, &sv.pools.resolve(&r_ref).unwrap()).await,
                0
            );
            // The name is now taken.
            assert!(
                sv.import_bundle(&bundle, "restored".into(), "default".into())
                    .await
                    .is_err()
            );

            // A damaged copy is refused while unpacking, leaving nothing behind.
            let path = sv.backups_dir().join(&bundle);
            let mut b = std::fs::read(&path).unwrap();
            let mid = b.len() / 2;
            b[mid] ^= 0xff;
            let bad = "damaged.qemu-ui.tar";
            std::fs::write(sv.backups_dir().join(bad), &b).unwrap();
            let images_before = std::fs::read_dir(src.parent().unwrap()).unwrap().count();
            match sv
                .import_bundle(bad, "broken".into(), "default".into())
                .await
            {
                Ok(job) => {
                    let done = finish(&sv, &job).await;
                    assert_eq!(done.status, JobStatus::Failed, "{done:?}");
                    assert!(done.error.unwrap_or_default().contains("damaged"));
                }
                Err(e) => panic!("the manifest is intact, so the job should start: {e:?}"),
            }
            assert_eq!(
                std::fs::read_dir(src.parent().unwrap()).unwrap().count(),
                images_before
            );
            assert!(!sv.list().unwrap().iter().any(|v| v.name == "broken"));

            // Live export of the running VM: QEMU copies the disk.
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            wait_state(&sv, &id, VmState::Running).await;
            let job = sv
                .export_vm(&id, false)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let done = finish(&sv, &job).await;
            assert_eq!(done.status, JobStatus::Succeeded, "{done:?}");
            let live = sv
                .list_backups()
                .await
                .unwrap()
                .into_iter()
                .find(|b| b.manifest.as_ref().is_some_and(|m| m.live))
                .expect("live bundle");
            // QEMU let go of the backup nodes: no leftover block jobs.
            let jobs = sv
                .live_qmp(&id)
                .unwrap()
                .execute("query-jobs", None)
                .await
                .unwrap();
            assert_eq!(jobs.as_array().map_or(0, |a| a.len()), 0, "{jobs}");
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            let job = sv
                .import_bundle(&live.name, "from-live".into(), "default".into())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(finish(&sv, &job).await.status, JobStatus::Succeeded);
            let vms = sv.list().unwrap();
            let fl = vms.iter().find(|v| v.name == "from-live").unwrap();
            let fl_ref = sv.detail(&fl.id).ok().unwrap().spec.disks[0].image.clone();
            assert_eq!(
                compare(&img, &src, &sv.pools.resolve(&fl_ref).unwrap()).await,
                0
            );

            sv.delete_backup(bad).unwrap();
            assert!(matches!(
                sv.delete_backup(bad),
                Err(super::OpError::NotFound)
            ));
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
