//! VM backup/export bundles (plan.md §9, X-03): the pure part.
//!
//! A bundle is a plain (uncompressed) tar file, `<name>.qemu-ui.tar`:
//!
//! ```text
//! manifest.json          always the first member
//! disks/disk<i>.qcow2    one per writable image disk (i = position in spec.disks)
//! nvram/VARS.qcow2       UEFI variables, when the VM has them
//! tpm/<file>             swtpm state, when the VM has an emulated TPM
//! ```
//!
//! The manifest lists every other member with its size and SHA-256, so an
//! import verifies each byte it unpacks. Read-only media (ISOs) are not
//! included: they are usually large, shared and easy to get again.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::spec::{Media, VmSpec, is_host_disk};

pub const BUNDLE_FORMAT: &str = "qemu-ui-bundle";
pub const BUNDLE_VERSION: u32 = 1;
pub const BUNDLE_EXT: &str = ".qemu-ui.tar";
pub const MANIFEST_NAME: &str = "manifest.json";

/// What a bundle member is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum BundleRole {
    /// The image of `spec.disks[index]`.
    Disk { index: usize },
    /// UEFI variable store.
    Nvram,
    /// One file of the swtpm state directory.
    Tpm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct BundleFile {
    /// Member path inside the tar.
    pub path: String,
    pub role: BundleRole,
    #[ts(type = "number")]
    pub size: u64,
    /// Lower-case hex SHA-256 of the member's content.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct BundleManifest {
    pub format: String,
    pub version: u32,
    #[ts(type = "number")]
    pub created_at: u64,
    /// Name of the VM it was exported from.
    pub source_name: String,
    /// The VM's saved configuration. Disk image references are the source
    /// host's; an import points them at the unpacked images.
    pub spec: VmSpec,
    pub files: Vec<BundleFile>,
    /// Exported while the VM ran (a point-in-time copy of every disk).
    pub live: bool,
    /// The guest's file systems were frozen through the guest agent for
    /// the copy point; otherwise a live export is crash-consistent.
    pub quiesced: bool,
    /// Disk images were copied as-is, keeping internal snapshots.
    pub snapshots_included: bool,
}

/// A bundle in the daemon's backups directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct BackupInfo {
    pub name: String,
    #[ts(type = "number")]
    pub size: u64,
    #[ts(type = "number")]
    pub modified_at: u64,
    /// `None` when the file is not a readable bundle (see `error`).
    pub manifest: Option<BundleManifest>,
    pub error: Option<String>,
}

/// Positions of the disks a bundle carries: writable image disks.
/// Physical host disks can't be bundled.
pub fn bundle_disks(spec: &VmSpec) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for (i, d) in spec.disks.iter().enumerate() {
        if d.readonly || d.media == Media::Cdrom {
            continue;
        }
        if is_host_disk(&d.image) {
            return Err(format!(
                "disk {i} is a physical host disk, which can't go into a bundle; detach it or mark it read-only to export the rest"
            ));
        }
        out.push(i);
    }
    Ok(out)
}

/// A bundle file name a client may use: `<stem>.qemu-ui.tar` with a plain
/// stem (letters, digits, `.`, `_`, `-`; no leading dot).
pub fn valid_bundle_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(BUNDLE_EXT) else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 120
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A member path is relative, plain and stays inside the bundle.
fn safe_member(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Check a manifest before unpacking anything.
pub fn check_manifest(m: &BundleManifest) -> Result<(), String> {
    if m.format != BUNDLE_FORMAT {
        return Err(format!("not a qemu-ui bundle (format '{}')", m.format));
    }
    if m.version != BUNDLE_VERSION {
        return Err(format!(
            "bundle version {} is not supported (this daemon reads version {BUNDLE_VERSION})",
            m.version
        ));
    }
    let want = bundle_disks(&m.spec)?;
    let mut seen_paths = std::collections::HashSet::new();
    let mut seen_disks = Vec::new();
    for f in &m.files {
        if !safe_member(&f.path) || f.path == MANIFEST_NAME {
            return Err(format!("bundle member '{}' has an unsafe path", f.path));
        }
        if !seen_paths.insert(f.path.as_str()) {
            return Err(format!("bundle member '{}' is listed twice", f.path));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("bundle member '{}' has no valid SHA-256", f.path));
        }
        let prefix_ok = match &f.role {
            BundleRole::Disk { index } => {
                if !want.contains(index) {
                    return Err(format!(
                        "bundle member '{}' is for disk {index}, which is not a writable disk of the VM",
                        f.path
                    ));
                }
                if seen_disks.contains(index) {
                    return Err(format!("disk {index} appears twice in the bundle"));
                }
                seen_disks.push(*index);
                f.path.starts_with("disks/")
            }
            BundleRole::Nvram => f.path == "nvram/VARS.qcow2",
            BundleRole::Tpm => f.path.starts_with("tpm/") && f.path.matches('/').count() == 1,
        };
        if !prefix_ok {
            return Err(format!(
                "bundle member '{}' is in the wrong place for its role",
                f.path
            ));
        }
    }
    if let Some(i) = want.iter().find(|i| !seen_disks.contains(i)) {
        return Err(format!("the bundle has no image for disk {i}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{
        CpuSpec, DiskBus, DiskSpec, DisplaySpec, FirmwareSpec, ImageRef, InputSpec, MachineSpec,
        MemorySpec, SPEC_VERSION,
    };
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
            cache: Default::default(),
            aio: None,
            discard: true,
            slot: None,
        }
    }

    fn manifest() -> BundleManifest {
        let mut spec = VmSpec {
            limits: Default::default(),
            guest: Default::default(),
            tpm: None,
            spec_version: SPEC_VERSION,
            name: "web".into(),
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
            disks: vec![],
            nics: vec![],
            display: DisplaySpec::default(),
            shares: vec![],
            input: InputSpec::default(),
            usb: vec![],
            devices: vec![],
            passthrough: vec![],
        };
        spec.disks = vec![
            disk("sys.qcow2", Media::Disk, false),
            disk("inst.iso", Media::Cdrom, true),
            disk("data.qcow2", Media::Disk, false),
        ];
        let f = |path: &str, role| BundleFile {
            path: path.into(),
            role,
            size: 1,
            sha256: "a".repeat(64),
        };
        BundleManifest {
            format: BUNDLE_FORMAT.into(),
            version: BUNDLE_VERSION,
            created_at: 1,
            source_name: "web".into(),
            spec,
            files: vec![
                f("disks/disk0.qcow2", BundleRole::Disk { index: 0 }),
                f("disks/disk2.qcow2", BundleRole::Disk { index: 2 }),
                f("nvram/VARS.qcow2", BundleRole::Nvram),
                f("tpm/tpm2-00.permall", BundleRole::Tpm),
            ],
            live: false,
            quiesced: false,
            snapshots_included: false,
        }
    }

    #[test]
    fn writable_image_disks_are_bundled() {
        assert_eq!(bundle_disks(&manifest().spec).unwrap(), vec![0, 2]);
        let mut spec = manifest().spec;
        spec.disks[0].image = ImageRef {
            pool: crate::spec::HOST_DISK_POOL.into(),
            name: "sdb".into(),
        };
        assert!(bundle_disks(&spec).unwrap_err().contains("host disk"));
    }

    #[test]
    fn names() {
        assert!(valid_bundle_name("web-2026.qemu-ui.tar"));
        for bad in [
            "web.tar",
            ".qemu-ui.tar",
            ".hidden.qemu-ui.tar",
            "../x.qemu-ui.tar",
            "a/b.qemu-ui.tar",
            "a b.qemu-ui.tar",
        ] {
            assert!(!valid_bundle_name(bad), "{bad}");
        }
    }

    #[test]
    fn manifest_checks() {
        check_manifest(&manifest()).unwrap();
        let bad = |edit: fn(&mut BundleManifest), want: &str| {
            let mut m = manifest();
            edit(&mut m);
            let e = check_manifest(&m).unwrap_err();
            assert!(e.contains(want), "{e}");
        };
        bad(|m| m.format = "zip".into(), "not a qemu-ui bundle");
        bad(|m| m.version = 9, "not supported");
        bad(|m| m.files[0].path = "../../etc/x".into(), "unsafe path");
        bad(|m| m.files[0].path = "/abs".into(), "unsafe path");
        bad(|m| m.files[0].path = "C:/x".into(), "unsafe path");
        bad(|m| m.files[3].path = "tpm/a/b".into(), "wrong place");
        bad(|m| m.files[2].path = "nvram/other".into(), "wrong place");
        bad(
            |m| m.files[1].role = BundleRole::Disk { index: 1 },
            "not a writable disk",
        );
        bad(|m| m.files[1].role = BundleRole::Disk { index: 0 }, "twice");
        bad(|m| drop(m.files.remove(1)), "no image for disk 2");
        bad(|m| m.files[0].sha256 = "xyz".into(), "SHA-256");
        bad(
            |m| m.files[1].path = m.files[0].path.clone(),
            "listed twice",
        );
    }
}
