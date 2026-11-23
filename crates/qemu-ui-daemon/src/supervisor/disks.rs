//! Disks on a running VM (plan.md §9.5, M2-09): hotplug, unplug and CD-ROM
//! media change, plus the machine's disk-bus support for the UI.
//!
//! A disk is added with exactly the JSON the compiler would emit for a cold
//! start (`compile::disk_objects`): `blockdev-add` for its protocol and
//! format nodes, then `device_add`. Ids follow the compiler's scheme with a
//! pinned slot (`qui-disk<N>`, `qui-dev<N>`) chosen above every index in
//! use, so a hotplugged disk never collides with a cold-plugged one and
//! keeps its ids after a restart.
//!
//! PCIe root buses do not accept hotplug, so PCI disks go into one of the
//! spare `pcie-root-port`s the compiler adds; PCI root buses (i440fx) take
//! them directly. Controllers a new disk needs (virtio-scsi, xHCI) are
//! hotplugged first when absent. IDE never hotplugs.
//!
//! PCI hot-unplug needs the guest's cooperation: `device_del` is a request,
//! and the device goes away when the guest answers. We wait briefly for
//! `DEVICE_DELETED`; otherwise the removal is reported as pending and the
//! monitor frees the block nodes when the event arrives.
//!
//! Changes are applied to the running VM first, then recorded in both the
//! run's spec (what is live) and the saved spec (what the next start uses),
//! so they survive a restart and `restart_pending` stays meaningful.

use super::*;
use qemu_ui_core::buses::DiskBusSupport;
use qemu_ui_core::compile::{disk_objects, is_pci_disk};
use qemu_ui_core::spec::{DiskBus, DiskSpec, disk_position, next_disk_index, pin_disk_slots};
use qemu_ui_core::vm::DiskChange;

/// How long to wait for the guest to release an unplugged device.
const UNPLUG_WAIT: Duration = Duration::from_secs(10);

fn conflict(msg: impl Into<String>) -> OpError {
    OpError::Conflict(msg.into())
}

fn invalid(field: String, rule: &str, msg: String) -> OpError {
    OpError::Invalid(vec![Diagnostic {
        severity: Severity::Error,
        field,
        rule: rule.into(),
        message: msg,
    }])
}

/// `qui-dev<N>` → N.
fn dev_index(id: &str) -> Option<usize> {
    id.strip_prefix("qui-dev").and_then(|n| n.parse().ok())
}

/// Ids of the devices the user (we) created: `/machine/peripheral`.
async fn peripherals(qmp: &Qmp) -> Result<Vec<String>> {
    let v = qmp
        .execute("qom-list", Some(json!({"path": "/machine/peripheral"})))
        .await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"].as_str().is_some_and(|t| t.starts_with("child<")))
        .filter_map(|p| p["name"].as_str().map(String::from))
        .collect())
}

/// A free spare PCIe root port (`qui-rp<K>` with nothing behind it).
///
/// Occupancy comes from QOM (each device's `parent_bus`), not `query-pci`:
/// the latter finds devices behind a bridge by the secondary bus number the
/// guest firmware assigns, so until the firmware has enumerated the bridges
/// every port looks empty.
async fn free_root_port(qmp: &Qmp) -> Result<Option<String>> {
    let ids = peripherals(qmp).await?;
    let mut ports: Vec<String> = ids
        .iter()
        .filter(|i| i.starts_with("qui-rp"))
        .cloned()
        .collect();
    for id in ids.iter().filter(|i| !i.starts_with("qui-rp")) {
        let parent = qmp
            .execute(
                "qom-get",
                Some(
                    json!({"path": format!("/machine/peripheral/{id}"), "property": "parent_bus"}),
                ),
            )
            .await;
        // Only devices have a parent bus; other objects fail and are skipped.
        let Ok(parent) = parent else { continue };
        let Some(bus) = parent.as_str() else { continue };
        // A root port's secondary bus is `/machine/peripheral/<port>/<port>`.
        ports.retain(|p| !bus.ends_with(&format!("/{p}/{p}")));
    }
    ports.sort_by_key(|id| {
        id.trim_start_matches("qui-rp")
            .parse::<usize>()
            .unwrap_or(usize::MAX)
    });
    Ok(ports.into_iter().next())
}

impl Supervisor {
    /// The active run of a VM, if any.
    pub(super) fn active_run(&self, id: &str) -> Option<RunRow> {
        self.store
            .last_run(id)
            .ok()
            .flatten()
            .filter(|r| r.ended_at.is_none())
    }

    /// Disk-bus support for a machine on this host's binary for `arch`.
    /// `None` when the machine could not be probed.
    pub async fn disk_buses(
        &self,
        arch: &str,
        machine: &str,
    ) -> OpResult<Option<Vec<DiskBusSupport>>> {
        let caps = self.caps_for(arch).await?;
        let machine = compile::pin_machine(machine, &caps).map_err(conflict)?;
        Ok(self
            .machine_buses(&caps, &machine)
            .await
            .map(|m| m.disk_buses(&caps)))
    }

    /// Where a new PCI device goes: a free root port on PCIe machines, the
    /// root bus otherwise (`None` lets QEMU choose a slot).
    async fn pci_slot_for(
        &self,
        qmp: &Qmp,
        machine: Option<&MachineBuses>,
    ) -> OpResult<Option<String>> {
        if !machine.is_some_and(|m| m.pcie_root()) {
            return Ok(None);
        }
        match free_root_port(qmp).await.map_err(OpError::Failed)? {
            Some(port) => Ok(Some(port)),
            None => Err(conflict(
                "no free PCIe slot is left for hotplug; stop and start the VM to get new spare slots",
            )),
        }
    }

    /// Add a disk. Running: hotplug it and record it; stopped: save it.
    pub async fn add_disk(&self, id: &str, mut disk: DiskSpec) -> OpResult<DiskChange> {
        let _g = self.ops.lock().await;
        self.refuse_if_suspended(id)?;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let caps = self.caps_for(&saved.arch).await?;
        disk.slot = None;

        if qemu_ui_core::spec::is_host_disk(&disk.image) && self.active_run(id).is_some() {
            return Err(OpError::Conflict(
                "physical host disks are attached while the VM is stopped; stop it, add the disk, then start it"
                    .into(),
            ));
        }
        let Some(run) = self.active_run(id) else {
            saved.disks.push(disk);
            Self::check(&saved, &caps)?;
            self.check_buses(&saved, &caps).await?;
            let json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
            self.store.update_vm_spec(id, &saved.name, &json, now())?;
            self.emit(id, "updated", json!({}));
            return Ok(DiskChange {
                live: false,
                pending: false,
                message: "Disk added; it is attached at the next start".into(),
                vm: self.detail(id)?,
            });
        };
        let qmp = self.qmp_of(id)?;
        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let pos = live.disks.len();
        let field = format!("disks[{}]", saved.disks.len());

        let machine = self.machine_buses(&caps, &live.machine.type_name).await;
        if let Some(m) = &machine {
            let s = m.support(&caps, disk.bus);
            if !s.available || !s.hotplug {
                return Err(invalid(
                    format!("{field}.bus"),
                    "disk.bus-no-hotplug",
                    s.reason
                        .unwrap_or_else(|| "this bus does not support hotplug".into()),
                ));
            }
        } else if disk.bus == DiskBus::Ide {
            return Err(invalid(
                format!("{field}.bus"),
                "disk.bus-no-hotplug",
                "IDE/SATA disks cannot be added while the VM runs".into(),
            ));
        }
        if !disk.readonly && disk.media == Media::Disk && self.pools.is_template(&disk.image) {
            return Err(invalid(
                format!("{field}.image"),
                "disk.template-writable",
                format!(
                    "'{}' is a template and is never opened writable; attach it read-only or clone it",
                    disk.image.name
                ),
            ));
        }
        let mut check = live.clone();
        check.disks.push(disk.clone());
        let errs: Vec<Diagnostic> = compile::validate(&check, &caps)
            .into_iter()
            .filter(|d| {
                d.severity == Severity::Error && d.field.starts_with(&format!("disks[{pos}]"))
            })
            .map(|mut d| {
                d.field = d.field.replacen(&format!("disks[{pos}]"), &field, 1);
                d
            })
            .collect();
        if !errs.is_empty() {
            return Err(OpError::Invalid(errs));
        }
        let path = self.pools.resolve(&disk.image).map_err(|e| {
            invalid(
                format!("{field}.image"),
                "disk.image-missing",
                format!("{e:#}"),
            )
        })?;
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| conflict("qemu-img is not installed"))?;
        let (format, _, _) = self.pools.info(&qemu_img, &path).await?;
        let img = ResolvedImage {
            path: path_str(&path),
            format,
            nbd: None,
        };

        // Never reuse an index still present in QEMU (e.g. an unplug the
        // guest has not acknowledged yet).
        let devs = peripherals(&qmp).await.map_err(OpError::Failed)?;
        let in_qemu = devs
            .iter()
            .filter_map(|d| dev_index(d))
            .map(|n| n + 1)
            .max()
            .unwrap_or(0);
        let n = next_disk_index(&[&live, &saved]).max(in_qemu);
        disk.slot = Some(n as u32);

        // Same lock as a VM start: an image operation must not swap this file.
        let locks = crate::image_ops::lock_for_start(std::slice::from_ref(&disk.image))
            .map_err(conflict)?;

        // Controllers the bus needs.
        let pci_bus = |slot: Option<String>, mut dev: Value| {
            if let Some(s) = slot {
                dev["bus"] = s.into();
            }
            dev
        };
        if disk.bus == DiskBus::Scsi && !devs.iter().any(|d| d == "qui-scsi0") {
            let slot = self.pci_slot_for(&qmp, machine.as_ref()).await?;
            let dev = pci_bus(
                slot,
                json!({"driver": "virtio-scsi-pci", "id": "qui-scsi0"}),
            );
            qmp.execute("device_add", Some(dev))
                .await
                .map_err(|e| OpError::Failed(e.into()))?;
        }
        if disk.bus == DiskBus::Usb && !devs.iter().any(|d| d == "qui-usb") {
            let slot = self.pci_slot_for(&qmp, machine.as_ref()).await?;
            let dev = pci_bus(slot, json!({"driver": "qemu-xhci", "id": "qui-usb"}));
            qmp.execute("device_add", Some(dev))
                .await
                .map_err(|e| OpError::Failed(e.into()))?;
        }

        let (file, mut fmt, mut dev) = disk_objects(n, &disk, &img, std::env::consts::OS);
        // Join the VM's IO throttle group, as at start (M6-07).
        let throttle = (live.limits.io.is_set() && disk.media == qemu_ui_core::spec::Media::Disk)
            .then(|| qemu_ui_core::limits::throttle_disk(&mut fmt));
        if is_pci_disk(disk.bus)
            && let Some(slot) = self.pci_slot_for(&qmp, machine.as_ref()).await?
        {
            dev["bus"] = slot.into();
        }
        let file_node = format!("qui-disk{n}-file");
        let fmt_node = format!("qui-disk{n}");
        let attach = async {
            qmp.execute("blockdev-add", Some(file)).await?;
            if let Some(t) = throttle {
                qmp.execute("blockdev-add", Some(t)).await?;
            }
            qmp.execute("blockdev-add", Some(fmt)).await?;
            qmp.execute("device_add", Some(dev)).await
        }
        .await;
        if let Err(e) = attach {
            let throttle_node = format!("{fmt_node}-throttle");
            for node in [&fmt_node, &throttle_node, &file_node] {
                let _ = qmp
                    .execute("blockdev-del", Some(json!({"node-name": node})))
                    .await;
            }
            return Err(OpError::Failed(anyhow!("QEMU refused the disk: {e}")));
        }

        // Record it: live first (what QEMU has), then the saved spec.
        pin_disk_slots(&mut live);
        live.disks.push(disk.clone());
        pin_disk_slots(&mut saved);
        saved.disks.push(disk.clone());
        let live_json = serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
        let saved_json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_run_spec(run.id, &live_json)?;
        self.store
            .update_vm_spec(id, &saved.name, &saved_json, now())?;
        drop(locks);
        self.emit(
            id,
            "disk-added",
            json!({"node": fmt_node, "image": disk.image}),
        );
        Ok(DiskChange {
            live: true,
            pending: false,
            message: format!("{} attached to the running VM", disk.image.name),
            vm: self.detail(id)?,
        })
    }

    /// Remove the disk whose ids use index `n`. Running: hot-unplug it
    /// (boot disks only with `force`); stopped: drop it from the spec.
    /// The image file is never deleted.
    pub async fn remove_disk(&self, id: &str, n: usize, force: bool) -> OpResult<DiskChange> {
        let _g = self.ops.lock().await;
        self.refuse_if_suspended(id)?;
        if self.has_live_overlay(id, &format!("qui-disk{n}")) {
            return Err(OpError::Conflict(
                "this disk sits on an overlay from an external snapshot taken while the VM ran;                  stop and start the VM first"
                    .into(),
            ));
        }
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let caps = self.caps_for(&saved.arch).await?;

        let Some(run) = self.active_run(id) else {
            let pos = disk_position(&saved, n)
                .ok_or_else(|| conflict(format!("the VM has no disk {n}")))?;
            pin_disk_slots(&mut saved);
            saved.disks.remove(pos);
            Self::check(&saved, &caps)?;
            let json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
            self.store.update_vm_spec(id, &saved.name, &json, now())?;
            self.emit(id, "updated", json!({}));
            return Ok(DiskChange {
                live: false,
                pending: false,
                message: "Disk removed from the configuration".into(),
                vm: self.detail(id)?,
            });
        };
        let qmp = self.qmp_of(id)?;
        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let pos = disk_position(&live, n)
            .ok_or_else(|| conflict(format!("the running VM has no disk {n}")))?;
        let disk = live.disks[pos].clone();
        if disk.bus == DiskBus::Ide {
            return Err(conflict(
                "IDE/SATA disks cannot be removed while the VM runs; stop it first (or change the CD-ROM's media)",
            ));
        }
        if disk.boot_index.is_some() && !force {
            return Err(conflict(
                "this is a boot disk; removing it from a running guest can crash it. Stop the VM, or force the removal",
            ));
        }
        let dev_id = format!("qui-dev{n}");
        let machine = self.machine_buses(&caps, &live.machine.type_name).await;
        if is_pci_disk(disk.bus) && machine.as_ref().is_some_and(|m| m.pcie_root()) {
            let parent = qmp
                .execute(
                    "qom-get",
                    Some(json!({"path": format!("/machine/peripheral/{dev_id}"), "property": "parent_bus"})),
                )
                .await
                .map_err(|e| OpError::Failed(e.into()))?;
            if !parent.as_str().unwrap_or_default().contains("qui-rp") {
                return Err(conflict(
                    "this disk sits on the PCIe root bus, which does not support hot-unplug (the VM was started \
                     before hotplug support); stop the VM to remove it",
                ));
            }
        }

        let mut events = qmp.events();
        qmp.execute("device_del", Some(json!({"id": dev_id})))
            .await
            .map_err(|e| OpError::Failed(anyhow!("QEMU refused to unplug the disk: {e}")))?;
        let deleted = tokio::time::timeout(UNPLUG_WAIT, async {
            loop {
                match events.recv().await {
                    Ok(ev)
                        if ev.event == "DEVICE_DELETED" && ev.data["device"] == dev_id.as_str() =>
                    {
                        return true;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        if deleted {
            self.free_disk_nodes(&qmp, n).await;
        }

        // Record it. Other disks keep their ids (slots pinned).
        pin_disk_slots(&mut live);
        live.disks.remove(pos);
        pin_disk_slots(&mut saved);
        let saved_pos = saved
            .disks
            .iter()
            .enumerate()
            .position(|(i, d)| d.node_index(i) == n && d.image == disk.image);
        if let Some(p) = saved_pos {
            saved.disks.remove(p);
        }
        let live_json = serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
        let saved_json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_run_spec(run.id, &live_json)?;
        self.store
            .update_vm_spec(id, &saved.name, &saved_json, now())?;
        self.emit(
            id,
            "disk-removed",
            json!({"node": format!("qui-disk{n}"), "pending": !deleted}),
        );
        let mut message = if deleted {
            format!("{} detached from the running VM", disk.image.name)
        } else {
            format!(
                "Unplug of {} requested; it completes when the guest releases the device",
                disk.image.name
            )
        };
        if saved_pos.is_none() {
            message.push_str(
                ". The saved configuration differs from the running one and was left unchanged",
            );
        }
        Ok(DiskChange {
            live: true,
            pending: !deleted,
            message,
            vm: self.detail(id)?,
        })
    }

    /// Put another image into the CD-ROM whose ids use index `n`.
    pub async fn change_media(&self, id: &str, n: usize, image: ImageRef) -> OpResult<DiskChange> {
        let _g = self.ops.lock().await;
        self.refuse_if_suspended(id)?;
        if self.has_live_overlay(id, &format!("qui-disk{n}")) {
            return Err(OpError::Conflict(
                "this disk sits on an overlay from an external snapshot taken while the VM ran;                  stop and start the VM first"
                    .into(),
            ));
        }
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let path = self
            .pools
            .resolve(&image)
            .map_err(|e| invalid("image".into(), "disk.image-missing", format!("{e:#}")))?;

        let Some(run) = self.active_run(id) else {
            let pos = disk_position(&saved, n)
                .ok_or_else(|| conflict(format!("the VM has no disk {n}")))?;
            if saved.disks[pos].media != Media::Cdrom {
                return Err(conflict("only CD-ROM media can be changed"));
            }
            saved.disks[pos].image = image;
            let json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
            self.store.update_vm_spec(id, &saved.name, &json, now())?;
            self.emit(id, "updated", json!({}));
            return Ok(DiskChange {
                live: false,
                pending: false,
                message: "Media changed; it is used at the next start".into(),
                vm: self.detail(id)?,
            });
        };
        let qmp = self.qmp_of(id)?;
        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let pos = disk_position(&live, n)
            .ok_or_else(|| conflict(format!("the running VM has no disk {n}")))?;
        if live.disks[pos].media != Media::Cdrom {
            return Err(conflict("only CD-ROM media can be changed"));
        }
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| conflict("qemu-img is not installed"))?;
        let (format, _, _) = self.pools.info(&qemu_img, &path).await?;
        let mut disk = live.disks[pos].clone();
        disk.image = image.clone();
        let img = ResolvedImage {
            path: path_str(&path),
            format,
            nbd: None,
        };
        let (file, fmt, _) = disk_objects(n, &disk, &img, std::env::consts::OS);
        let dev_id = format!("qui-dev{n}");

        // Eject (forcing the tray even if the guest locked it), swap the
        // nodes keeping their names, insert, close.
        let swap = async {
            qmp.execute(
                "blockdev-open-tray",
                Some(json!({"id": dev_id, "force": true})),
            )
            .await?;
            qmp.execute("blockdev-remove-medium", Some(json!({"id": dev_id})))
                .await?;
            for node in [format!("qui-disk{n}"), format!("qui-disk{n}-file")] {
                let _ = qmp
                    .execute("blockdev-del", Some(json!({"node-name": node})))
                    .await;
            }
            qmp.execute("blockdev-add", Some(file)).await?;
            qmp.execute("blockdev-add", Some(fmt)).await?;
            qmp.execute(
                "blockdev-insert-medium",
                Some(json!({"id": dev_id, "node-name": format!("qui-disk{n}")})),
            )
            .await?;
            qmp.execute("blockdev-close-tray", Some(json!({"id": dev_id})))
                .await
        }
        .await;
        swap.map_err(|e| OpError::Failed(anyhow!("QEMU could not change the media: {e}")))?;

        pin_disk_slots(&mut live);
        live.disks[pos].image = image.clone();
        pin_disk_slots(&mut saved);
        if let Some(p) = saved
            .disks
            .iter()
            .enumerate()
            .position(|(i, d)| d.node_index(i) == n && d.media == Media::Cdrom)
        {
            saved.disks[p].image = image.clone();
        }
        let live_json = serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
        let saved_json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_run_spec(run.id, &live_json)?;
        self.store
            .update_vm_spec(id, &saved.name, &saved_json, now())?;
        self.emit(
            id,
            "media-changed",
            json!({"node": format!("qui-disk{n}"), "image": image}),
        );
        Ok(DiskChange {
            live: true,
            pending: false,
            message: format!("{} inserted", image.name),
            vm: self.detail(id)?,
        })
    }

    async fn free_disk_nodes(&self, qmp: &Qmp, n: usize) {
        for node in [
            format!("qui-disk{n}"),
            format!("qui-disk{n}-throttle"),
            format!("qui-disk{n}-file"),
        ] {
            let _ = qmp
                .execute("blockdev-del", Some(json!({"node-name": node})))
                .await;
        }
    }

    /// `DEVICE_DELETED` for one of our disks: free its block nodes (the
    /// guest acknowledged an unplug after `remove_disk` stopped waiting).
    pub(super) async fn disk_device_deleted(&self, qmp: &Qmp, data: &Value) {
        if let Some(n) = data["device"].as_str().and_then(dev_index) {
            self.free_disk_nodes(qmp, n).await;
        }
    }
}

/// Disk hotplug against a real QEMU (TCG), skipped when QEMU is absent.
#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;
    use qemu_ui_core::spec::*;

    fn blocks(v: &Value) -> Vec<(String, String)> {
        // (device qdev id, inserted file name) from query-block.
        v.as_array()
            .into_iter()
            .flatten()
            .map(|b| {
                (
                    // QEMU reports either a path ("/machine/peripheral/
                    // qui-dev0/virtio-backend") or the bare id ("qui-dev1",
                    // IDE CD-ROMs): take whichever component is our id.
                    b["qdev"]
                        .as_str()
                        .unwrap_or_default()
                        .split('/')
                        .find(|p| p.starts_with("qui-"))
                        .unwrap_or_default()
                        .to_string(),
                    b["inserted"]["file"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    }

    async fn nodes(sv: &Supervisor, id: &str) -> Vec<String> {
        let qmp = sv.live_qmp(id).unwrap();
        let v = qmp.execute("query-named-block-nodes", None).await.unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b["node-name"].as_str().map(String::from))
            .collect()
    }

    fn disk(name: &str, bus: DiskBus, media: Media) -> DiskSpec {
        DiskSpec {
            image: ImageRef {
                pool: "default".into(),
                name: name.into(),
            },
            bus,
            media,
            readonly: media == Media::Cdrom,
            boot_index: None,
            cache: CacheMode::None,
            aio: None,
            discard: true,
            slot: None,
        }
    }

    #[test]
    fn hotplug_unplug_media_and_persistence() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("hp");
        // Stops the VM even if an assertion below fails.
        let guard = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "hotplug").await; // q35, virtio disk 0
            for (name, size) in [("data.qcow2", 32u64 << 20), ("scsi.qcow2", 32 << 20)] {
                sv.pools
                    .create(
                        &img,
                        &ImageRef {
                            pool: "default".into(),
                            name: name.into(),
                        },
                        size,
                        "qcow2",
                    )
                    .await
                    .unwrap();
            }
            let dir = sv.pools.pool_dir("default").unwrap();
            std::fs::write(dir.join("one.iso"), vec![0u8; 2048 * 16]).unwrap();
            std::fs::write(dir.join("two.iso"), vec![1u8; 2048 * 16]).unwrap();

            // Stopped: an IDE CD-ROM is simply saved (disk 1).
            let c = sv
                .add_disk(&id, disk("one.iso", DiskBus::Ide, Media::Cdrom))
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(!c.live);
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let qmp = sv.live_qmp(&id).unwrap();

            // q35: spare PCIe root ports exist; the boot disk sits behind one.
            let pci = qmp.execute("query-pci", None).await.unwrap().to_string();
            assert!(
                pci.contains("qui-rp0") && pci.contains("qui-rp4"),
                "root ports: one per PCI disk + 4 spare"
            );
            let parent = qmp
                .execute(
                    "qom-get",
                    Some(json!({"path": "/machine/peripheral/qui-dev0", "property": "parent_bus"})),
                )
                .await
                .unwrap();
            assert!(parent.as_str().unwrap().contains("qui-rp"), "{parent}");

            // Hotplug a virtio disk: next free index 2, behind a spare port.
            let c = sv
                .add_disk(&id, disk("data.qcow2", DiskBus::Virtio, Media::Disk))
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live && !c.pending, "{}", c.message);
            assert!(nodes(&sv, &id).await.contains(&"qui-disk2".to_string()));
            let live = c.vm.live_spec.clone().unwrap();
            assert_eq!(live.disks.last().unwrap().slot, Some(2));
            assert!(
                !c.vm.summary.restart_pending,
                "live and saved specs stay in step"
            );

            // IDE cannot hotplug.
            match sv
                .add_disk(&id, disk("two.iso", DiskBus::Ide, Media::Cdrom))
                .await
            {
                Err(OpError::Invalid(d)) => assert_eq!(d[0].rule, "disk.bus-no-hotplug"),
                other => panic!(
                    "IDE hotplug must be refused: {:?}",
                    other.map(|c| c.message)
                ),
            }

            // Change the CD-ROM's media (disk 1).
            let c = sv
                .change_media(
                    &id,
                    1,
                    ImageRef {
                        pool: "default".into(),
                        name: "two.iso".into(),
                    },
                )
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live);
            let b = blocks(&qmp.execute("query-block", None).await.unwrap());
            assert!(
                b.iter()
                    .any(|(dev, file)| dev == "qui-dev1" && file.ends_with("two.iso")),
                "{b:?}"
            );

            // SCSI: the controller is hotplugged too; SCSI unplug needs no guest.
            let c = sv
                .add_disk(&id, disk("scsi.qcow2", DiskBus::Scsi, Media::Disk))
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live);
            assert!(nodes(&sv, &id).await.contains(&"qui-disk3".to_string()));
            let c = sv
                .remove_disk(&id, 3, false)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(!c.pending, "{}", c.message);
            assert!(!nodes(&sv, &id).await.contains(&"qui-disk3".to_string()));

            // The boot disk needs force; IDE cannot be unplugged at all.
            assert!(matches!(
                sv.remove_disk(&id, 1, false).await,
                Err(OpError::Conflict(_))
            ));

            // Restart: the hotplugged disk and the new media persist, same ids.
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            let saved = sv.detail(&id).ok().unwrap().spec;
            assert_eq!(saved.disks.len(), 3, "{:?}", saved.disks);
            assert_eq!(saved.disks[1].image.name, "two.iso");
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let n = nodes(&sv, &id).await;
            assert!(
                n.contains(&"qui-disk2".to_string()) && !n.contains(&"qui-disk3".to_string()),
                "{n:?}"
            );

            // Unplugging a PCI disk needs the guest (none here): it is either
            // done or reported pending, and the spec is updated either way.
            let c = sv
                .remove_disk(&id, 2, false)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live, "{}", c.message);
            assert_eq!(c.vm.spec.disks.len(), 2);
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;

            // Bus availability by introspection.
            let q35 = sv
                .disk_buses("x86_64", "q35")
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap()
                .unwrap();
            let pc = sv
                .disk_buses("x86_64", "pc")
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap()
                .unwrap();
            let ide =
                |v: &[DiskBusSupport]| v.iter().find(|s| s.bus == DiskBus::Ide).unwrap().clone();
            let virtio =
                |v: &[DiskBusSupport]| v.iter().find(|s| s.bus == DiskBus::Virtio).unwrap().clone();
            assert!(ide(&q35).available && !ide(&q35).hotplug);
            assert!(virtio(&q35).hotplug && virtio(&pc).hotplug);
            if let Some(arm) = find("qemu-system-aarch64") {
                let probed = crate::probe::machine_buses(&arm, "virt").await.unwrap();
                let m = MachineBuses {
                    machine: "virt".into(),
                    buses: probed,
                };
                assert!(m.pcie_root() && !m.has_ide());
            }
        });
        drop(rt);
        drop(guard);
        let _ = std::fs::remove_dir_all(&data);
    }
}
