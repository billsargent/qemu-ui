//! VM snapshots (plan.md §9.3, M2-06): API types and the pure rules.
//!
//! Two kinds, one tree:
//! * internal: a qcow2 snapshot tag written into every writable disk of a VM
//!   (and its UEFI NVRAM file), optionally with memory and device state;
//! * external (disk-only): each writable disk image is frozen and a new qcow2
//!   overlay on top of it becomes the VM's disk. The frozen image *is* the
//!   snapshot; reverting puts a fresh overlay on it. Works for raw disks too.
//!
//! Each snapshot's parent is the snapshot the VM was based on when it was
//! taken. The kind is recorded in the tag: `qui-<id>` internal, `ext-<id>`
//! external.

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
    /// External snapshots: the overlay that became the VM's disk when the
    /// snapshot was taken (`name` is the frozen image).
    #[serde(default)]
    pub overlay: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SnapshotKind {
    Internal,
    External,
}

/// Tag prefix of external snapshots.
pub const EXTERNAL_TAG_PREFIX: &str = "ext-";

pub fn kind_of_tag(tag: &str) -> SnapshotKind {
    if tag.starts_with(EXTERNAL_TAG_PREFIX) {
        SnapshotKind::External
    } else {
        SnapshotKind::Internal
    }
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
    pub kind: SnapshotKind,
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
    /// Why internal snapshots cannot be taken right now, if they cannot.
    pub unsupported_reason: Option<String>,
    /// Why external snapshots cannot be taken right now, if they cannot.
    pub external_unsupported_reason: Option<String>,
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

/// Indices of the disks an external snapshot covers: every writable disk.
/// Any image format works (the overlay is qcow2); physical host disks can't
/// be frozen behind an overlay file.
pub fn external_snapshot_disks(spec: &VmSpec) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for (i, d) in spec.disks.iter().enumerate() {
        if d.readonly || d.media == Media::Cdrom {
            continue;
        }
        if crate::spec::is_host_disk(&d.image) {
            return Err(format!(
                "disk {i} is a physical host disk; external snapshots need image files.                  Mark it read-only or detach it to snapshot the other disks."
            ));
        }
        out.push(i);
    }
    if out.is_empty() {
        return Err("the VM has no writable disk to hold a snapshot".into());
    }
    Ok(out)
}

/// Name stem for the overlay put on `image` by snapshot `sid`:
/// `sys.qcow2` → `sys.snap-<sid>` (the caller adds `.qcow2` and makes it
/// unique). Repeated snapshots don't stack suffixes.
pub fn overlay_stem(image: &str, sid: &str) -> String {
    let stem = image.rsplit_once('.').map_or(image, |(s, _)| s);
    let stem = stem.split(".snap-").next().unwrap_or(stem);
    let stem = stem.split(".rev-").next().unwrap_or(stem);
    format!("{stem}.snap-{sid}")
}

/// Name stem for the fresh overlay a revert puts on a frozen image.
pub fn revert_stem(image: &str, sid: &str) -> String {
    overlay_stem(image, sid).replacen(".snap-", ".rev-", 1)
}

/// Why an external snapshot's frozen image can't be merged away, if it
/// can't. Deleting an external snapshot streams its frozen image into the
/// single overlay on top of it; anything more needs a merge the UI can't
/// make safely.
pub fn external_delete_blocker(
    frozen: &str,
    dependents: usize,
    other_users: &[String],
    vm_running: bool,
) -> Option<String> {
    if vm_running {
        return Some(
            "stop the VM to delete an external snapshot: its frozen image is merged into the              overlay above it, which needs the files closed"
                .into(),
        );
    }
    if !other_users.is_empty() {
        return Some(format!(
            "{frozen} is also used by {}; it can't be merged away",
            other_users.join(", ")
        ));
    }
    match dependents {
        0 => None,
        1 => None,
        n => Some(format!(
            "{n} images are built on {frozen} (the tree branches here after a revert); merging it              into one would break the others. Delete the snapshots on the other branches first."
        )),
    }
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
            slot: None,
        }
    }

    fn spec(disks: Vec<DiskSpec>) -> VmSpec {
        VmSpec {
            limits: Default::default(),
            guest: Default::default(),
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
            shares: vec![],
            input: InputSpec::default(),
            usb: vec![],
            devices: vec![],
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
    fn external_covers_any_format_but_not_host_disks() {
        let s = spec(vec![
            disk("iso", Media::Cdrom, true),
            disk("sys.raw", Media::Disk, false),
            disk("data.qcow2", Media::Disk, false),
        ]);
        assert_eq!(external_snapshot_disks(&s).unwrap(), vec![1, 2]);
        let mut h = s.clone();
        h.disks[1].image.pool = crate::spec::HOST_DISK_POOL.into();
        assert!(
            external_snapshot_disks(&h)
                .unwrap_err()
                .contains("physical host disk")
        );
        assert!(external_snapshot_disks(&spec(vec![disk("iso", Media::Cdrom, true)])).is_err());
    }

    #[test]
    fn overlay_names_dont_stack() {
        assert_eq!(overlay_stem("sys.qcow2", "ab"), "sys.snap-ab");
        assert_eq!(overlay_stem("sys.snap-ab.qcow2", "cd"), "sys.snap-cd");
        assert_eq!(overlay_stem("sys.rev-ab-1.qcow2", "cd"), "sys.snap-cd");
        assert_eq!(revert_stem("sys.snap-ab.qcow2", "ef"), "sys.rev-ef");
        assert_eq!(overlay_stem("noext", "x"), "noext.snap-x");
    }

    #[test]
    fn kinds_from_tags() {
        assert_eq!(kind_of_tag("qui-0123"), SnapshotKind::Internal);
        assert_eq!(kind_of_tag("ext-0123"), SnapshotKind::External);
    }

    #[test]
    fn external_delete_rules() {
        assert!(
            external_delete_blocker("a", 1, &[], true)
                .unwrap()
                .contains("stop the VM")
        );
        assert!(
            external_delete_blocker("a", 1, &["other".into()], false)
                .unwrap()
                .contains("other")
        );
        assert!(
            external_delete_blocker("a", 2, &[], false)
                .unwrap()
                .contains("branches")
        );
        assert!(external_delete_blocker("a", 1, &[], false).is_none());
        assert!(external_delete_blocker("a", 0, &[], false).is_none());
    }

    #[test]
    fn names() {
        assert!(valid_snapshot_name("before upgrade"));
        assert!(!valid_snapshot_name("  "));
        assert!(!valid_snapshot_name("a\nb"));
    }
}
