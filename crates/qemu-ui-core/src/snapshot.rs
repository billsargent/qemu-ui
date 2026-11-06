//! VM snapshots (plan.md §9.3, M2-06): API types and the pure rules.
//!
//! Snapshots are internal qcow2 snapshots taken with the same tag on every
//! writable disk of a VM (and its UEFI NVRAM file), optionally together with
//! the VM's memory and device state. They form a tree: each snapshot's
//! parent is the snapshot the VM was based on when it was taken.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::spec::{Media, VmSpec};

/// One disk (or the NVRAM file) a snapshot covers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct SnapshotDisk {
    /// `image` (a pool image) or `nvram` (the VM's UEFI variable store).
    pub kind: String,
    /// Pool and image name for `image`; empty for `nvram`.
    pub pool: String,
    pub name: String,
    /// QEMU block node name while the VM runs (`qui-diskN`, `qui-fw-vars`).
    pub node: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct SnapshotInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Tag stored inside the qcow2 images.
    pub tag: String,
    pub parent_id: Option<String>,
    /// Includes RAM and device state (taken while the VM ran).
    pub with_memory: bool,
    /// Taken automatically (e.g. before a revert).
    pub auto: bool,
    pub disks: Vec<SnapshotDisk>,
    #[ts(type = "number")]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct SnapshotList {
    pub snapshots: Vec<SnapshotInfo>,
    /// The snapshot the VM's current state descends from.
    pub head: Option<String>,
    /// Why snapshots cannot be taken right now, if they cannot.
    pub unsupported_reason: Option<String>,
}

/// A writable disk of the spec with its on-disk format.
pub struct DiskFormat<'a> {
    pub index: usize,
    pub pool: &'a str,
    pub name: &'a str,
    pub format: &'a str,
}

/// Indices of the disks an internal snapshot must cover: every writable
/// disk. Internal snapshots need every writable node to be qcow2, so any
/// other format is refused with a message naming the disk.
pub fn snapshot_disks(spec: &VmSpec, formats: &[Option<String>]) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for (i, d) in spec.disks.iter().enumerate() {
        if d.readonly || d.media == Media::Cdrom {
            continue;
        }
        match formats.get(i).and_then(|f| f.as_deref()) {
            Some("qcow2") => out.push(i),
            Some(other) => {
                return Err(format!(
                    "disk {i} ({}/{}) is {other}; internal snapshots need every writable disk in qcow2 \
                     format. Convert it to qcow2 on the Storage page, or mark it read-only.",
                    d.image.pool, d.image.name
                ));
            }
            None => {
                return Err(format!(
                    "disk {i} ({}/{}) could not be inspected",
                    d.image.pool, d.image.name
                ));
            }
        }
    }
    if out.is_empty() {
        return Err("the VM has no writable disk to hold a snapshot".into());
    }
    Ok(out)
}

/// Is `name` an acceptable snapshot name?
pub fn valid_snapshot_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n.len() <= 80 && !n.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;
    use std::collections::BTreeMap;

    fn disk(name: &str, media: Media, readonly: bool) -> DiskSpec {
        DiskSpec {
            image: ImageRef {
                pool: "default".into(),
                name: name.into(),
            },
            bus: DiskBus::Virtio,
            media,
            readonly,
            boot_index: None,
            cache: CacheMode::None,
            aio: None,
            discard: true,
        }
    }

    fn spec(disks: Vec<DiskSpec>) -> VmSpec {
        VmSpec {
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
            disks,
            nics: vec![],
            display: DisplaySpec::default(),
            input: InputSpec::default(),
            passthrough: vec![],
        }
    }

    #[test]
    fn covers_writable_qcow2_and_skips_media() {
        let s = spec(vec![
            disk("iso", Media::Cdrom, true),
            disk("sys.qcow2", Media::Disk, false),
            disk("ro.raw", Media::Disk, true),
        ]);
        let f = vec![Some("raw".into()), Some("qcow2".into()), Some("raw".into())];
        assert_eq!(snapshot_disks(&s, &f).unwrap(), vec![1]);
    }

    #[test]
    fn refuses_raw_writable_disk_by_name() {
        let s = spec(vec![
            disk("sys.qcow2", Media::Disk, false),
            disk("data.raw", Media::Disk, false),
        ]);
        let f = vec![Some("qcow2".into()), Some("raw".into())];
        let e = snapshot_disks(&s, &f).unwrap_err();
        assert!(e.contains("disk 1 (default/data.raw) is raw"), "{e}");
        assert!(e.contains("Convert it to qcow2"));
    }

    #[test]
    fn needs_a_writable_disk() {
        let s = spec(vec![disk("iso", Media::Cdrom, true)]);
        assert!(snapshot_disks(&s, &[Some("raw".into())]).is_err());
    }

    #[test]
    fn names() {
        assert!(valid_snapshot_name("before upgrade"));
        assert!(!valid_snapshot_name("  "));
        assert!(!valid_snapshot_name("a\nb"));
    }
}
