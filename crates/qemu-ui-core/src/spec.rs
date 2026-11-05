//! VM spec v1 (plan.md §5.1): the declarative, versioned desired state of a
//! VM. Compiled to argv by [`crate::compile`].
//!
//! The spec never holds host paths: images are referenced by pool + name,
//! firmware by descriptor id. The only exception is `passthrough`, which is
//! an expert surface (plan.md §5.3).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Current spec format version.
pub const SPEC_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct VmSpec {
    pub spec_version: u32,
    /// `[A-Za-z0-9][A-Za-z0-9._-]{0,62}`; also used in `-name`.
    pub name: String,
    /// Target architecture in QEMU naming.
    pub arch: String,
    /// Versioned machine type; aliases are pinned at creation (§5.2).
    pub machine: MachineSpec,
    /// `auto`, or an accelerator name (`kvm`, `whpx`, `hvf`, `tcg`).
    #[serde(default = "auto")]
    pub accel: String,
    pub cpu: CpuSpec,
    pub memory: MemorySpec,
    #[serde(default)]
    pub firmware: FirmwareSpec,
    #[serde(default)]
    pub disks: Vec<DiskSpec>,
    #[serde(default)]
    pub nics: Vec<NicSpec>,
    #[serde(default)]
    pub display: DisplaySpec,
    #[serde(default)]
    pub input: InputSpec,
    /// Raw argv tokens appended after generated options (§5.3).
    #[serde(default)]
    pub passthrough: Vec<String>,
}

fn auto() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct MachineSpec {
    #[serde(rename = "type")]
    pub type_name: String,
    /// Extra `-machine` properties, e.g. `smm=on`.
    #[serde(default)]
    pub props: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct CpuSpec {
    /// CPU model; `None` uses the machine default.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra CPU properties, e.g. `hv-passthrough=on`.
    #[serde(default)]
    pub props: BTreeMap<String, String>,
    /// Total vCPUs.
    pub count: u32,
    #[serde(default)]
    pub topology: Option<Topology>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[ts(export)]
pub struct Topology {
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[ts(export)]
pub struct MemorySpec {
    #[ts(type = "number")]
    pub size_mib: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum FirmwareSpec {
    /// Whatever the machine boots by default (SeaBIOS on x86, built-in elsewhere).
    #[default]
    Default,
    /// Firmware from a descriptor, e.g. UEFI. The daemon keeps the per-VM
    /// NVRAM copy (as qcow2, so internal snapshots work).
    Descriptor { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS, JsonSchema)]
#[ts(export)]
pub struct ImageRef {
    pub pool: String,
    /// File name inside the pool. No path separators.
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum DiskBus {
    /// virtio-blk on PCI (or virtio-mmio machines).
    Virtio,
    /// virtio-scsi controller + scsi-hd / scsi-cd.
    Scsi,
    /// NVMe controller per disk.
    Nvme,
    /// IDE / AHCI (q35 has AHCI built in).
    Ide,
    /// usb-storage on an xHCI controller.
    Usb,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum Media {
    #[default]
    Disk,
    Cdrom,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum CacheMode {
    /// O_DIRECT, flushes honoured. Best default for most guests.
    #[default]
    None,
    Writeback,
    Writethrough,
    Directsync,
    /// Ignores flushes: fast, loses data on host crash.
    Unsafe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct DiskSpec {
    pub image: ImageRef,
    pub bus: DiskBus,
    #[serde(default)]
    pub media: Media,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub boot_index: Option<u32>,
    #[serde(default)]
    pub cache: CacheMode,
    /// `threads`, `native`, `io_uring`; `None` picks a platform default.
    #[serde(default)]
    pub aio: Option<String>,
    #[serde(default = "yes")]
    pub discard: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct PortForward {
    /// `tcp` or `udp`.
    pub proto: String,
    /// Host address to bind; loopback unless deliberately widened.
    pub host_addr: String,
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum NicBackend {
    /// slirp NAT (M1). Network objects arrive in M3.
    User {
        #[serde(default)]
        forwards: Vec<PortForward>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct NicSpec {
    pub backend: NicBackend,
    /// Device model, e.g. `virtio-net-pci`, `e1000e`.
    pub model: String,
    /// Locally administered MAC; assigned at creation when absent.
    #[serde(default)]
    pub mac: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct DisplaySpec {
    /// Graphics device (`virtio-vga`, `VGA`, `virtio-gpu-pci`, …); `None` = headless.
    #[serde(default)]
    pub device: Option<String>,
    /// Serve the console over VNC (proxied; never exposed directly).
    #[serde(default = "yes")]
    pub vnc: bool,
    /// qemu-vdagent: clipboard + absolute pointer (needs guest agent).
    #[serde(default)]
    pub vdagent: bool,
}

impl Default for DisplaySpec {
    fn default() -> Self {
        DisplaySpec {
            device: None,
            vnc: true,
            vdagent: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct InputSpec {
    /// USB tablet for absolute pointing without guest agent.
    #[serde(default = "yes")]
    pub usb_tablet: bool,
}

impl Default for InputSpec {
    fn default() -> Self {
        InputSpec { usb_tablet: true }
    }
}

/// Is `name` an acceptable VM name?
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Is `name` acceptable as a file name inside a pool?
pub fn valid_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains(['/', '\\', ':', '\0'])
}

/// Is `mac` a unicast `xx:xx:xx:xx:xx:xx`?
pub fn valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        && u8::from_str_radix(parts[0], 16).is_ok_and(|b| b & 1 == 0)
}

/// JSON Schema for the current spec version (for editors and config-as-code).
pub fn json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(VmSpec)).expect("schema serialises")
}

/// Upgrade an older spec document to [`SPEC_VERSION`]. v1 is the first
/// version, so this only rejects unknown future versions for now.
pub fn migrate(mut doc: serde_json::Value) -> Result<VmSpec, String> {
    let version = doc
        .get("spec_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if version > SPEC_VERSION as u64 {
        return Err(format!(
            "spec_version {version} is newer than this build supports ({SPEC_VERSION})"
        ));
    }
    if version == 0 {
        return Err("missing spec_version".into());
    }
    // Future migrations: `if version < 2 { doc = v1_to_v2(doc) }` …
    doc["spec_version"] = SPEC_VERSION.into();
    serde_json::from_value(doc).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("win11-dev"));
        assert!(valid_name("a"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a,b"));
        assert!(!valid_name(&"a".repeat(64)));
    }

    #[test]
    fn image_names() {
        assert!(valid_image_name("debian-12.qcow2"));
        assert!(valid_image_name("disk, with comma.qcow2"));
        assert!(!valid_image_name("../etc/passwd"));
        assert!(!valid_image_name("a/b"));
        assert!(!valid_image_name("C:x"));
        assert!(!valid_image_name(".hidden"));
    }

    #[test]
    fn macs() {
        assert!(valid_mac("52:54:00:12:34:56"));
        assert!(!valid_mac("53:54:00:12:34:56"), "multicast");
        assert!(!valid_mac("52:54:00:12:34"));
        assert!(!valid_mac("52:54:00:12:34:zz"));
    }

    /// `data/schema/vm-spec.v1.json` must match the Rust types. Regenerate
    /// with `UPDATE_GOLDEN=1 cargo test -p qemu-ui-core`.
    #[test]
    fn json_schema_file_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/schema")
            .join(format!("vm-spec.v{SPEC_VERSION}.json"));
        let actual = serde_json::to_string_pretty(&json_schema()).unwrap() + "
";
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default().replace("
", "
");
        assert_eq!(actual, expected, "stale {}; run with UPDATE_GOLDEN=1", path.display());
    }

    #[test]
    fn migrate_rejects_future_and_missing() {
        assert!(migrate(serde_json::json!({"spec_version": 99})).is_err());
        assert!(migrate(serde_json::json!({})).is_err());
    }
}
