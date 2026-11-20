//! External (disk-only) snapshots (plan.md §9.3, M2-06).
//!
//! Taking one freezes every writable disk image of the VM and puts a new
//! qcow2 overlay on top of it, which becomes the VM's disk in the spec
//! (always a pool reference). Running VMs get the overlays through QMP
//! `blockdev-add` + a `transaction` of `blockdev-snapshot`, so the switch is
//! atomic across disks and the guest keeps running; stopped VMs just get the
//! overlay files from `qemu-img create`.
//!
//! * Revert (VM stopped): a fresh overlay on each frozen image becomes the
//!   VM's disk. The previous disk is deleted only if nothing else needs it
//!   (no snapshot froze it, nothing is built on it, no other VM uses it); a
//!   revert's safety snapshot freezes it, so by default nothing is lost.
//! * Delete (VM stopped): the frozen image is streamed into the one image
//!   built on it (`qemu-img rebase`, which copies the data it needs) and
//!   removed. When several images are built on it (the tree branches) or
//!   another VM uses it, deleting is refused with the reason.
//! * Automatic snapshots before risky image operations (shrink, compaction)
//!   keep a frozen copy of the image as an external snapshot of its VM.
//!
//! While a VM runs on overlays added live, its top block nodes are no longer
//! `qui-diskN`; operations addressing those nodes (internal snapshots,
//! disk hot-unplug, media changes) are refused until the VM restarts.

use super::*;
use qemu_ui_core::snapshot::{
    EXTERNAL_TAG_PREFIX, SnapshotKind, external_delete_blocker, external_snapshot_disks,
    kind_of_tag, overlay_stem, revert_stem,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Automatic snapshots kept per VM; older ones are pruned when they can be.
const AUTO_KEEP: usize = 3;

const LIVE_KEY: &str = "ext-live:";

/// Overlays added to a running VM: base node → current top node, valid for
/// one run only.
#[derive(Serialize, Deserialize, Default)]
struct LiveOverlays {
    run_id: i64,
    tops: BTreeMap<String, String>,
}

impl Supervisor {
    fn live_overlays(&self, id: &str) -> LiveOverlays {
        let run = self
            .store
            .last_run(id)
            .ok()
            .flatten()
            .filter(|r| r.ended_at.is_none());
        let Some(run) = run else {
            return LiveOverlays::default();
        };
        self.store
            .get_kv(&format!("{LIVE_KEY}{id}"))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<LiveOverlays>(&s).ok())
            .filter(|l| l.run_id == run.id)
            .unwrap_or_default()
    }

    /// Has this running VM switched disk `node` onto a live overlay?
    pub(crate) fn has_live_overlay(&self, id: &str, node: &str) -> bool {
        self.live_overlays(id).tops.contains_key(node)
    }

    /// Refusal for operations that address `qui-diskN` nodes directly.
    pub(crate) fn live_overlay_conflict(&self, id: &str) -> Option<String> {
        (!self.live_overlays(id).tops.is_empty()).then(|| {
            "this VM took an external snapshot while running, so its disks now sit on overlays; \
             stop and start it before this operation"
                .to_string()
        })
    }

    /// VMs other than `except` whose saved spec uses `r`.
    fn other_users(&self, except: &str, r: &ImageRef) -> Vec<String> {
        self.store
            .list_vms()
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v.id != except)
            .filter(|v| {
                serde_json::from_str::<VmSpec>(&v.spec_json)
                    .is_ok_and(|s| s.disks.iter().any(|d| &d.image == r))
            })
            .map(|v| v.name)
            .collect()
    }

    /// Create an empty qcow2 overlay named after `stem` on `base`.
    async fn create_overlay(
        &self,
        qemu_img: &Path,
        base: &ImageRef,
        stem: &str,
    ) -> Result<ImageRef> {
        let base_path = self.pools.resolve(base)?;
        // -U: the base may be open in the running VM.
        let (format, size, _) = self.pools.info(qemu_img, &base_path).await?;
        let name = self.pools.free_name(&base.pool, stem, "qcow2")?;
        let r = ImageRef {
            pool: base.pool.clone(),
            name,
        };
        let path = self.pools.new_image_path(&r)?;
        self.pools
            .check_quota(Some(qemu_img), &r.pool, 1 << 20)
            .await?;
        // -u: don't open the base (it may be locked by QEMU); the size is given.
        let out = crate::probe::command(qemu_img)
            .args(["create", "-q", "-f", "qcow2", "-F", &format, "-u", "-b"])
            .arg(&base_path)
            .arg(&path)
            .arg(size.to_string())
            .output()
            .await?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&path);
            bail!(
                "qemu-img create: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(r)
    }

    /// Point disk `i` (by node) of the saved spec, and of the running spec
    /// when there is a run, from `old` to `new`.
    fn repoint_disk(&self, id: &str, node: &str, old: &ImageRef, new: &ImageRef) -> Result<()> {
        let swap = |spec_json: &str| -> Result<Option<String>> {
            let mut spec: VmSpec = serde_json::from_str(spec_json)?;
            let n: Option<usize> = node.strip_prefix("qui-disk").and_then(|n| n.parse().ok());
            let idx = n.and_then(|n| qemu_ui_core::spec::disk_position(&spec, n));
            match idx.and_then(|i| spec.disks.get_mut(i)) {
                Some(d) if &d.image == old => {
                    d.image = new.clone();
                    Ok(Some(serde_json::to_string(&spec)?))
                }
                _ => Ok(None),
            }
        };
        if let Some(run) = self.store.last_run(id)?.filter(|r| r.ended_at.is_none())
            && let Some(j) = swap(&run.spec_json)?
        {
            self.store.update_run_spec(run.id, &j)?;
        }
        let row = self
            .store
            .get_vm(id)?
            .ok_or_else(|| anyhow!("no such VM"))?;
        if let Some(j) = swap(&row.spec_json)? {
            self.store.update_vm_spec(id, &row.name, &j, now())?;
        }
        Ok(())
    }

    /// Take an external snapshot; the caller holds the lifecycle lock.
    pub(super) async fn take_external_locked(
        &self,
        id: &str,
        name: &str,
        description: &str,
        auto: bool,
        ctx: &JobCtx,
    ) -> Result<SnapshotRow> {
        let target = self.snapshot_target(id).map_err(op_to_anyhow)?;
        let idx = external_snapshot_disks(&target.spec).map_err(|e| anyhow!(e))?;
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
        let sid = format!("{:010x}", rand::random::<u64>() & 0xff_ffff_ffff);
        // (frozen image, overlay, node)
        let mut made: Vec<(ImageRef, ImageRef, String)> = Vec::new();
        for (k, &i) in idx.iter().enumerate() {
            let base = target.spec.disks[i].image.clone();
            ctx.progress(
                Some(k as f64 / idx.len() as f64),
                Some(&format!("Creating an overlay on {}", base.name)),
            );
            match self
                .create_overlay(&qemu_img, &base, &overlay_stem(&base.name, &sid))
                .await
            {
                Ok(o) => made.push((base, o, qemu_ui_core::spec::disk_node(&target.spec, i))),
                Err(e) => {
                    self.remove_images(made.iter().map(|m| &m.1));
                    return Err(e);
                }
            }
        }

        if let Some(qmp) = &target.qmp {
            ctx.progress(None, Some("Switching the running VM onto the overlays"));
            let mut live = self.live_overlays(id);
            let run_id = self
                .store
                .last_run(id)?
                .filter(|r| r.ended_at.is_none())
                .map(|r| r.id)
                .ok_or_else(|| anyhow!("the VM stopped meanwhile"))?;
            let mut added = Vec::new();
            let mut actions = Vec::new();
            let mut fail: Option<anyhow::Error> = None;
            for (k, (_, o, node)) in made.iter().enumerate() {
                let new_node = format!("qui-ext-{sid}-{k}");
                let path = self.pools.resolve(o)?;
                let add = json!({
                    "driver": "qcow2",
                    "node-name": new_node,
                    "file": {"driver": "file", "filename": path_str(&path)},
                    "backing": null,
                });
                if let Err(e) = qmp.execute("blockdev-add", Some(add)).await {
                    fail = Some(anyhow!("adding the overlay for {node}: {e}"));
                    break;
                }
                added.push(new_node.clone());
                let top = live.tops.get(node).cloned().unwrap_or_else(|| node.clone());
                actions.push(json!({"type": "blockdev-snapshot", "data": {"node": top, "overlay": new_node}}));
            }
            if fail.is_none()
                && let Err(e) = qmp
                    .execute("transaction", Some(json!({"actions": actions})))
                    .await
            {
                fail = Some(anyhow!("QEMU refused the snapshot: {e}"));
            }
            if let Some(e) = fail {
                for n in &added {
                    let _ = qmp
                        .execute("blockdev-del", Some(json!({"node-name": n})))
                        .await;
                }
                self.remove_images(made.iter().map(|m| &m.1));
                return Err(e);
            }
            live.run_id = run_id;
            for ((_, _, node), new_node) in made.iter().zip(&added) {
                live.tops.insert(node.clone(), new_node.clone());
            }
            self.store
                .put_kv(&format!("{LIVE_KEY}{id}"), &serde_json::to_string(&live)?)?;
        }
        for (base, o, node) in &made {
            self.repoint_disk(id, node, base, o)?;
        }

        let disks: Vec<SnapshotDisk> = made
            .iter()
            .map(|(base, o, node)| SnapshotDisk {
                kind: "image".into(),
                pool: base.pool.clone(),
                name: base.name.clone(),
                node: node.clone(),
                overlay: Some(o.name.clone()),
            })
            .collect();
        let row = self.record_external(id, &sid, name, description, auto, disks)?;
        Ok(row)
    }

    fn record_external(
        &self,
        id: &str,
        sid: &str,
        name: &str,
        description: &str,
        auto: bool,
        disks: Vec<SnapshotDisk>,
    ) -> Result<SnapshotRow> {
        let row = SnapshotRow {
            id: sid.into(),
            vm_id: id.into(),
            name: name.into(),
            description: description.into(),
            tag: format!("{EXTERNAL_TAG_PREFIX}{sid}"),
            parent_id: self.store.snapshot_head(id)?,
            with_memory: false,
            auto,
            disks_json: serde_json::to_string(&disks)?,
            created_at: now(),
        };
        self.store.insert_snapshot(&row)?;
        self.store.set_snapshot_head(id, Some(sid))?;
        self.emit(
            id,
            "snapshot",
            json!({"action": "taken", "snapshot": sid, "name": name, "auto": auto, "kind": "external"}),
        );
        Ok(row)
    }

    fn remove_images<'a>(&self, images: impl Iterator<Item = &'a ImageRef>) {
        for r in images {
            if let Ok(p) = self.pools.resolve(r) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Images any snapshot of this VM lives in: frozen images of external
    /// snapshots and images holding internal snapshot tags.
    fn snapshot_images(&self, id: &str) -> Vec<ImageRef> {
        self.store
            .list_snapshots(id)
            .unwrap_or_default()
            .iter()
            .flat_map(|s| info_of(s).disks)
            .filter(|d| d.kind == "image")
            .map(|d| ImageRef {
                pool: d.pool,
                name: d.name,
            })
            .collect()
    }

    /// Revert to an external snapshot; the VM is stopped and the caller
    /// holds the lifecycle lock.
    pub(super) async fn revert_external_locked(
        &self,
        id: &str,
        snap: &SnapshotRow,
        ctx: &JobCtx,
    ) -> Result<()> {
        let target = self.snapshot_target(id).map_err(op_to_anyhow)?;
        if target.qmp.is_some() {
            bail!("stop the VM to revert to an external snapshot");
        }
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
        let disks = info_of(snap).disks;
        // Check every disk maps before changing anything.
        let mut plan = Vec::new();
        for d in &disks {
            let n: Option<usize> = d.node.strip_prefix("qui-disk").and_then(|n| n.parse().ok());
            let i = n
                .and_then(|n| qemu_ui_core::spec::disk_position(&target.spec, n))
                .ok_or_else(|| {
                    anyhow!(
                        "the VM no longer has the disk {} was taken from ({})",
                        d.name,
                        d.node
                    )
                })?;
            let frozen = ImageRef {
                pool: d.pool.clone(),
                name: d.name.clone(),
            };
            self.pools
                .resolve(&frozen)
                .with_context(|| format!("the snapshot's image {}/{} is gone", d.pool, d.name))?;
            plan.push((target.spec.disks[i].image.clone(), frozen, d.node.clone()));
        }
        let sid = format!("{:06x}", rand::random::<u32>() & 0xff_ffff);
        let mut made: Vec<ImageRef> = Vec::new();
        for (k, (_, frozen, _)) in plan.iter().enumerate() {
            ctx.progress(
                Some(k as f64 / plan.len() as f64),
                Some(&format!("Creating an overlay on {}", frozen.name)),
            );
            match self
                .create_overlay(&qemu_img, frozen, &revert_stem(&frozen.name, &sid))
                .await
            {
                Ok(o) => made.push(o),
                Err(e) => {
                    self.remove_images(made.iter());
                    return Err(e);
                }
            }
        }
        let frozen_set = self.snapshot_images(id);
        for ((old, _, node), new) in plan.iter().zip(&made) {
            self.repoint_disk(id, node, old, new)?;
            // The state being left: delete it unless something still needs it.
            let keep = frozen_set.contains(old)
                || !self.other_users(id, old).is_empty()
                || self.pools.is_template(old)
                || match self.pools.resolve(old) {
                    Ok(p) => !self
                        .pools
                        .dependents(&qemu_img, &p)
                        .await
                        .unwrap_or_default()
                        .is_empty(),
                    Err(_) => true,
                };
            if keep {
                ctx.log_line(&format!("kept {}/{} (still needed)", old.pool, old.name));
            } else {
                ctx.log_line(&format!("discarded {}/{}", old.pool, old.name));
                self.remove_images(std::iter::once(old));
            }
        }
        Ok(())
    }

    /// Why this external snapshot can't be deleted now, if it can't.
    pub(super) async fn external_delete_check(
        &self,
        id: &str,
        snap: &SnapshotRow,
    ) -> Option<String> {
        let target = match self.snapshot_target(id) {
            Ok(t) => t,
            Err(e) => return Some(format!("{:#}", op_to_anyhow(e))),
        };
        let qemu_img = self.qemu_img().await?;
        for d in info_of(snap).disks {
            let frozen = ImageRef {
                pool: d.pool.clone(),
                name: d.name.clone(),
            };
            if target.spec.disks.iter().any(|x| x.image == frozen) {
                return Some(format!("{} is the VM's current disk", d.name));
            }
            let Ok(path) = self.pools.resolve(&frozen) else {
                continue; // already gone: nothing to merge
            };
            let deps = self
                .pools
                .dependents(&qemu_img, &path)
                .await
                .unwrap_or_default();
            let mut users = self.other_users(id, &frozen);
            if self.pools.is_template(&frozen) {
                users.push("a template".into());
            }
            if let Some(why) =
                external_delete_blocker(&d.name, deps.len(), &users, target.qmp.is_some())
            {
                return Some(why);
            }
            // Internal snapshots stored inside the frozen image would be lost.
            let tags = self
                .store
                .list_snapshots(id)
                .unwrap_or_default()
                .into_iter()
                .filter(|s| kind_of_tag(&s.tag) == SnapshotKind::Internal)
                .filter(|s| {
                    info_of(s)
                        .disks
                        .iter()
                        .any(|x| x.pool == d.pool && x.name == d.name)
                })
                .map(|s| s.name)
                .collect::<Vec<_>>();
            if !tags.is_empty() {
                return Some(format!(
                    "internal snapshots live inside {} ({}); delete them first",
                    d.name,
                    tags.join(", ")
                ));
            }
        }
        None
    }

    /// Merge each frozen image into the one image built on it, then remove
    /// it. `external_delete_check` passed under the same lock.
    pub(super) async fn delete_external_locked(
        &self,
        id: &str,
        snap: &SnapshotRow,
        ctx: &JobCtx,
    ) -> Result<()> {
        if let Some(why) = self.external_delete_check(id, snap).await {
            bail!(why);
        }
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| anyhow!("qemu-img is not installed"))?;
        for d in info_of(snap).disks {
            let frozen = ImageRef {
                pool: d.pool.clone(),
                name: d.name.clone(),
            };
            let Ok(path) = self.pools.resolve(&frozen) else {
                continue;
            };
            let deps = self.pools.dependents(&qemu_img, &path).await?;
            if let Some(dep) = deps.first() {
                let dep_path = self.pools.resolve(dep)?;
                // Rebase the dependent onto the frozen image's own backing
                // (or onto nothing); safe mode copies the data it needs.
                let mut args: Vec<String> =
                    vec!["rebase".into(), "-q".into(), "-f".into(), "qcow2".into()];
                match self.pools.backing_of(&qemu_img, &path).await {
                    Some(b) => {
                        let (bf, _, _) = self.pools.info(&qemu_img, &b).await?;
                        args.extend(["-F".into(), bf, "-b".into(), path_str(&b)]);
                    }
                    None => args.extend(["-b".into(), String::new()]),
                }
                args.push(path_str(&dep_path));
                ctx.progress(None, Some(&format!("Merging {} into {}", d.name, dep.name)));
                ctx.log_line(&format!("$ qemu-img {}", args.join(" ")));
                let out = crate::probe::command(&qemu_img)
                    .args(&args)
                    .output()
                    .await?;
                if !out.status.success() {
                    bail!(
                        "qemu-img rebase: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
            ctx.log_line(&format!("removed {}/{}", d.pool, d.name));
            std::fs::remove_file(&path).with_context(|| format!("removing {}", d.name))?;
        }
        Ok(())
    }

    /// Keep a frozen copy of `r` as an automatic external snapshot of the
    /// VM that uses it, before an operation that rewrites it in place.
    /// Returns the copy's name, or `None` when no VM uses the image.
    pub async fn auto_snapshot_image(
        &self,
        r: &ImageRef,
        op: &str,
        ctx: &JobCtx,
    ) -> Result<Option<String>> {
        let owner = self.store.list_vms()?.into_iter().find_map(|v| {
            let spec: VmSpec = serde_json::from_str(&v.spec_json).ok()?;
            let i = spec.disks.iter().position(|d| {
                &d.image == r && !d.readonly && d.media == qemu_ui_core::spec::Media::Disk
            })?;
            Some((v.id, qemu_ui_core::spec::disk_node(&spec, i)))
        });
        let Some((vm, node)) = owner else {
            return Ok(None);
        };
        let _g = self.ops.lock().await;
        let src = self.pools.resolve(r)?;
        let size = std::fs::metadata(&src)?.len();
        self.pools.check_quota(None, &r.pool, size).await?;
        let (stem, ext) = r.name.rsplit_once('.').unwrap_or((r.name.as_str(), "img"));
        let sid = format!("{:010x}", rand::random::<u64>() & 0xff_ffff_ffff);
        let copy = ImageRef {
            pool: r.pool.clone(),
            name: self.pools.free_name(
                &r.pool,
                &format!("{stem}.before-{op}-{}", &sid[..6]),
                ext,
            )?,
        };
        let dst = self.pools.new_image_path(&copy)?;
        ctx.progress(
            None,
            Some(&format!("Keeping a copy of {} before {op}", r.name)),
        );
        let (s, d) = (src.clone(), dst.clone());
        tokio::task::spawn_blocking(move || std::fs::copy(s, d)).await??;
        let disks = vec![SnapshotDisk {
            kind: "image".into(),
            pool: copy.pool.clone(),
            name: copy.name.clone(),
            node,
            overlay: None,
        }];
        self.record_external(
            &vm,
            &sid,
            &format!("Before {op} of {}", r.name),
            "",
            true,
            disks,
        )?;
        self.prune_auto(&vm, ctx).await;
        Ok(Some(copy.name))
    }

    /// Drop automatic snapshots beyond the newest `AUTO_KEEP` where that is
    /// possible without a merge (nothing built on them).
    pub(super) async fn prune_auto(&self, id: &str, ctx: &JobCtx) {
        let autos: Vec<SnapshotRow> = self
            .store
            .list_snapshots(id)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.auto)
            .collect();
        if autos.len() <= AUTO_KEEP {
            return;
        }
        let Some(qemu_img) = self.qemu_img().await else {
            return;
        };
        let excess = autos.len() - AUTO_KEEP;
        for s in autos.into_iter().take(excess) {
            if kind_of_tag(&s.tag) != SnapshotKind::External {
                continue; // internal ones are pruned by hand
            }
            let mut free = true;
            for d in info_of(&s).disks {
                if let Ok(p) = self.pools.resolve(&ImageRef {
                    pool: d.pool,
                    name: d.name,
                }) {
                    free &= self
                        .pools
                        .dependents(&qemu_img, &p)
                        .await
                        .is_ok_and(|v| v.is_empty());
                }
            }
            if !free || self.store.snapshot_head(id).ok().flatten().as_deref() == Some(&s.id) {
                continue;
            }
            for d in info_of(&s).disks {
                let r = ImageRef {
                    pool: d.pool,
                    name: d.name,
                };
                self.remove_images(std::iter::once(&r));
            }
            let _ = self.store.delete_snapshot(id, &s.id);
            ctx.log_line(&format!("pruned automatic snapshot '{}'", s.name));
        }
    }
}
