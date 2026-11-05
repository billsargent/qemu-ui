//! Capability model: what a host and each QEMU binary on it can do.
//!
//! These types are the API contract for `/api/v1/host/*`; TypeScript
//! definitions are generated from them (`cargo test -p qemu-ui-core` writes
//! them to `web/src/api/generated/`).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::firmware::Firmware;
use crate::parity::ParityRow;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct QemuVersion {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
    pub package: String,
}

impl std::fmt::Display for QemuVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// A discovered QEMU executable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct Binary {
    /// Stable id: first 16 hex chars of the SHA-256 of the file.
    pub id: String,
    pub path: String,
    /// Target architecture in QEMU naming; `None` for `qemu-img`.
    pub arch: Option<String>,
    /// Version string, filled in by the probe (empty until probed).
    pub version: String,
    pub sha256: String,
    /// Where it was found: `project` (.tools/qemu), `config`, `path`.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct MachineInfo {
    pub name: String,
    pub description: String,
    pub alias: Option<String>,
    pub is_default: bool,
    pub deprecated: bool,
    pub cpu_max: Option<u32>,
    pub default_cpu_type: Option<String>,
    pub hotpluggable_cpus: Option<bool>,
    pub numa_mem_supported: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct CpuModel {
    pub name: String,
    pub deprecated: bool,
    /// Present when QEMU reports the model cannot run with the current accel.
    pub unavailable_features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct DeviceInfo {
    pub name: String,
    /// `-device help` category, e.g. `Storage`, `Network`, `Display`.
    pub category: String,
    pub bus: Option<String>,
    pub alias: Option<String>,
    pub description: Option<String>,
    pub no_user: bool,
}

/// One property of a device type, from `device-list-properties`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct DeviceProperty {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub description: Option<String>,
    #[ts(type = "unknown")]
    pub default_value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct AccelInfo {
    pub name: String,
    pub available: bool,
    /// Hardware accelerators only run guests of the host's architecture.
    pub hardware: bool,
    /// Why it is unavailable (raw QEMU error, trimmed).
    pub reason: Option<String>,
    /// What the user can do about it.
    pub remediation: Option<String>,
}

/// Backend lists. `None` means the option is not compiled into this binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct Backends {
    pub netdev: Option<Vec<String>>,
    pub chardev: Option<Vec<String>>,
    pub display: Option<Vec<String>>,
    pub audiodev: Option<Vec<String>>,
    pub tpmdev: Option<Vec<String>>,
    /// `-object` types (e.g. `monitor-qmp`, `memory-backend-file`, `secret`).
    #[serde(default)]
    pub object: Option<Vec<String>>,
}

impl Backends {
    /// QEMU 11.1 deprecates `-mon` in favour of `-object monitor-qmp`.
    pub fn has_object(&self, name: &str) -> bool {
        self.object
            .as_ref()
            .is_some_and(|o| o.iter().any(|x| x == name))
    }
}

/// Bump when the probe collects new data, so cached snapshots made by an
/// older probe are re-probed instead of served without it.
pub const PROBE_VERSION: u32 = 2;

/// Result of probing one `qemu-system-*` binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct BinaryCaps {
    pub binary: Binary,
    pub arch: String,
    pub qemu_version: QemuVersion,
    /// [`PROBE_VERSION`] of the probe that produced this snapshot.
    #[serde(default)]
    pub probe_version: u32,
    /// Seconds since the Unix epoch.
    #[ts(type = "number")]
    pub probed_at: u64,
    /// True when hardware acceleration is possible for this arch on this host.
    pub native: bool,
    pub machines: Vec<MachineInfo>,
    pub cpus: Vec<CpuModel>,
    pub devices: Vec<DeviceInfo>,
    pub accels: Vec<AccelInfo>,
    pub backends: Backends,
    /// SHA-256 of the `query-qmp-schema` result; changes when QMP changes.
    pub qmp_schema_hash: String,
    /// Non-fatal problems hit while probing.
    pub warnings: Vec<String>,
}

impl BinaryCaps {
    /// The best accelerator: an available hardware one, else TCG.
    pub fn best_accel(&self) -> Option<&AccelInfo> {
        self.accels
            .iter()
            .find(|a| a.available && a.hardware)
            .or_else(|| self.accels.iter().find(|a| a.available))
    }

    pub fn default_machine(&self) -> Option<&MachineInfo> {
        self.machines.iter().find(|m| m.is_default)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HostInfo {
    /// `linux`, `windows`, `macos`.
    pub os: String,
    /// QEMU naming (`x86_64`, `aarch64`, …).
    pub arch: String,
}

/// A probe failure for one binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct ProbeError {
    pub binary: Binary,
    pub error: String,
}

/// Everything the host can do, as served by `/api/v1/host/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct HostCaps {
    pub host: HostInfo,
    pub binaries: Vec<BinaryCaps>,
    pub failed: Vec<ProbeError>,
    pub qemu_img: Option<Binary>,
    pub firmware: Vec<Firmware>,
    pub parity: Vec<ParityRow>,
    /// Seconds since the Unix epoch.
    #[ts(type = "number")]
    pub scanned_at: u64,
}

/// Progress of a capability scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct ScanStatus {
    pub running: bool,
    pub total: u32,
    pub done: u32,
    /// Binary currently being probed, for display.
    pub current: Option<String>,
    pub error: Option<String>,
}

/// Response of `GET /api/v1/host/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct CapabilitiesResponse {
    /// `None` until the first scan finishes.
    pub caps: Option<HostCaps>,
    pub scan: ScanStatus,
}
