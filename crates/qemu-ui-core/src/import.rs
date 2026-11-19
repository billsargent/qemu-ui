//! Importing VMs from elsewhere (plan.md §5.4, M4-07): shared result types.
//!
//! Importers are best-effort and pure. They never put host paths in the
//! spec: every file the source refers to comes back in `files_to_map`, and
//! the disk that uses it holds a placeholder image reference until the user
//! (or the daemon, when the file already sits in a pool) maps it. Anything
//! an importer cannot express is reported in `warnings` or kept verbatim in
//! `passthrough`, never dropped silently.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::spec::{ImageRef, VmSpec};

/// Pool that placeholder image references point at until mapped.
pub const PLACEHOLDER_POOL: &str = "default";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ImportFormat {
    /// A QEMU command line.
    Argv,
    /// A libvirt domain XML document.
    Libvirt,
}

/// What a referenced host file is used for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FileUse {
    /// A disk or CD/DVD image: `disk` says which entry of `spec.disks`.
    Image,
    /// A block device or physical drive: attach it as a host disk through
    /// the privileged helper instead (M2-11).
    HostDevice,
}

/// A host file the imported VM refers to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct FileToMap {
    /// The path exactly as written in the source.
    pub path: String,
    pub usage: FileUse,
    /// Index into `spec.disks` of the disk using this file.
    pub disk: usize,
    /// Image name suggested from the file name.
    pub suggested_name: String,
    /// Image format the source declared, if any.
    #[serde(default)]
    pub format: Option<String>,
    /// Set by the daemon when the file already lives in a registered pool;
    /// the spec's disk then already points at it.
    #[serde(default)]
    pub mapped: Option<ImageRef>,
}

/// Firmware the source asked for, for the daemon to match to a descriptor.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FirmwareHint {
    Uefi,
    UefiSecureBoot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct ImportResult {
    pub spec: VmSpec,
    /// What was approximated, ignored or needs attention, in plain words.
    pub warnings: Vec<String>,
    pub files_to_map: Vec<FileToMap>,
    /// UEFI was requested; the daemon resolves it to a firmware descriptor.
    #[serde(default)]
    pub firmware_hint: Option<FirmwareHint>,
}

/// An image name derived from a host path: its file name, made acceptable
/// to `valid_image_name` (no separators, no leading dot, printable).
pub fn suggest_image_name(path: &str) -> String {
    let base = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("disk.img");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches(['.', '_', '-']).to_string();
    if cleaned.is_empty() || !crate::spec::valid_image_name(&cleaned) {
        "disk.img".into()
    } else {
        cleaned
    }
}

/// A VM name derived from a source name, acceptable to `valid_name`.
pub fn suggest_vm_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let cleaned: String = cleaned.chars().take(63).collect();
    if crate::spec::valid_name(&cleaned) {
        cleaned
    } else {
        "imported-vm".into()
    }
}

/// Does `path` name a raw host block device rather than an image file?
pub fn is_block_device_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.starts_with("/dev/")
        || p.starts_with(r"\\.\physicaldrive")
        || p.starts_with("//./physicaldrive")
        || p.starts_with(r"\\?\")
}

/// Does a firmware file name look like UEFI (edk2/OVMF/AAVMF)?
pub fn looks_like_uefi(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    ["ovmf", "edk2", "aavmf", "qemu_efi", "uefi"]
        .iter()
        .any(|k| p.contains(k))
}

/// Does a firmware file name look like a Secure Boot build?
pub fn looks_like_secure_boot(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    ["secboot", ".secure", "-secure", "_secure", ".ms.", "_ms."]
        .iter()
        .any(|k| p.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            suggest_image_name("/srv/vm/disk one.qcow2"),
            "disk_one.qcow2"
        );
        assert_eq!(suggest_image_name(r"D:\vms\win.qcow2"), "win.qcow2");
        assert_eq!(suggest_image_name("/x/.hidden"), "hidden");
        assert_eq!(suggest_image_name("/"), "disk.img");
        assert_eq!(suggest_vm_name("My VM (test)"), "My-VM--test-");
        assert_eq!(suggest_vm_name("---"), "imported-vm");
    }

    #[test]
    fn device_paths() {
        assert!(is_block_device_path("/dev/sdb"));
        assert!(is_block_device_path(r"\\.\PhysicalDrive1"));
        assert!(!is_block_device_path("/srv/disk.img"));
    }

    #[test]
    fn firmware_names() {
        assert!(looks_like_uefi("/usr/share/OVMF/OVMF_CODE.secboot.fd"));
        assert!(looks_like_secure_boot(
            "/usr/share/OVMF/OVMF_CODE.secboot.fd"
        ));
        assert!(looks_like_secure_boot("edk2-x86_64-secure-code.fd"));
        assert!(!looks_like_secure_boot("/usr/share/OVMF/OVMF_CODE.fd"));
        assert!(!looks_like_uefi("/usr/share/seabios/bios.bin"));
    }
}
