//! VM API types (plan.md §7.1) and wizard defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::caps::BinaryCaps;
use crate::compile::{Diagnostic, pin_machine};
use crate::spec::*;

/// Lifecycle state. `paused` always carries a reason (§7.1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum VmState {
    /// Never started, or stopped cleanly.
    Stopped,
    /// Process spawned; waiting for QMP and start-up checks.
    Starting,
    Running,
    Paused,
    /// Graceful shutdown requested; waiting for the guest.
    Stopping,
    /// Exited unexpectedly or failed to start.
    Crashed,
}

impl VmState {
    pub fn as_str(self) -> &'static str {
        match self {
            VmState::Stopped => "stopped",
            VmState::Starting => "starting",
            VmState::Running => "running",
            VmState::Paused => "paused",
            VmState::Stopping => "stopping",
            VmState::Crashed => "crashed",
        }
    }

    pub fn parse(s: &str) -> VmState {
        match s {
            "starting" => VmState::Starting,
            "running" => VmState::Running,
            "paused" => VmState::Paused,
            "stopping" => VmState::Stopping,
            "crashed" => VmState::Crashed,
            _ => VmState::Stopped,
        }
    }

    /// Has a QEMU process (or should have one).
    pub fn is_active(self) -> bool {
        matches!(
            self,
            VmState::Starting | VmState::Running | VmState::Paused | VmState::Stopping
        )
    }
}

/// Map QMP `query-status` `status` to our state and pause reason.
pub fn state_from_qmp(status: &str) -> (VmState, Option<String>) {
    match status {
        "running" => (VmState::Running, None),
        "prelaunch" => (VmState::Starting, None),
        "shutdown" => (VmState::Stopping, None),
        "paused" => (VmState::Paused, Some("user".into())),
        other => (VmState::Paused, Some(other.to_string())),
    }
}

/// A human explanation for a pause reason.
pub fn explain_pause(reason: &str) -> &'static str {
    match reason {
        "user" => "Paused by request.",
        "io-error" => {
            "Paused after a disk I/O error (often a full host disk). Free space, then resume."
        }
        "guest-panicked" => "The guest kernel panicked.",
        "internal-error" => "QEMU hit an internal error; check the VM log.",
        "debug" => "Stopped by the debugger.",
        "suspended" => "The guest suspended itself (ACPI S3).",
        "watchdog" => "Stopped by the guest watchdog.",
        "postmigrate" | "finish-migrate" | "inmigrate" => "Migration in progress or finished.",
        _ => "Paused.",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct VmSummary {
    pub id: String,
    pub name: String,
    pub arch: String,
    pub state: VmState,
    pub pause_reason: Option<String>,
    /// Accelerator of the current/last run.
    pub accel: Option<String>,
    /// Seconds since epoch, while active.
    #[ts(type = "number | null")]
    pub started_at: Option<u64>,
    /// Why the last start or run failed.
    pub last_error: Option<String>,
    /// The saved spec differs from what the running process was started with.
    pub restart_pending: bool,
    pub has_vnc: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct VmDetail {
    pub summary: VmSummary,
    pub spec: VmSpec,
    /// argv of the current/last run, for the "changed since last start" diff.
    pub last_run_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct CompileResponse {
    /// Shell-quoted rendering for display and copying. Never executed.
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct VmEvent {
    /// Global, monotonically increasing; also the SSE event id.
    #[ts(type = "number")]
    pub seq: i64,
    pub vm_id: String,
    /// Microseconds since epoch.
    #[ts(type = "number")]
    pub at_us: u64,
    /// QMP event name (`SHUTDOWN`, `STOP`, …) or our own (`state`).
    pub kind: String,
    #[ts(type = "unknown")]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ImageInfo {
    pub pool: String,
    pub name: String,
    pub format: String,
    #[ts(type = "number")]
    pub virtual_size: u64,
    #[ts(type = "number")]
    pub actual_size: u64,
    /// Attached to these VMs (by name).
    pub used_by: Vec<String>,
    /// A template: never opened writable; clone it instead (M2-05).
    #[serde(default)]
    pub template: bool,
    /// Pool/name of the backing image when this is an overlay inside a pool.
    #[serde(default)]
    pub backing: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct PoolInfo {
    pub name: String,
    /// Host directory; shown so users know where to copy ISOs until uploads land (M2).
    pub path: String,
    pub images: Vec<ImageInfo>,
}

/// A reasonable starting spec for `arch` on this binary, for the wizard.
pub fn default_spec(name: &str, caps: &BinaryCaps) -> (VmSpec, Vec<Diagnostic>) {
    let has = |d: &str| caps.devices.iter().any(|x| x.name == d);
    let pick = |opts: &[&str]| opts.iter().find(|d| has(d)).map(|d| d.to_string());
    let machine = match caps.arch.as_str() {
        "x86_64" | "i386" => pin_machine("q35", caps).ok(),
        "aarch64" | "arm" | "riscv64" | "riscv32" | "loongarch64" => pin_machine("virt", caps).ok(),
        _ => None,
    }
    .or_else(|| caps.default_machine().map(|m| m.name.clone()))
    .or_else(|| {
        caps.machines
            .iter()
            .find(|m| m.name != "none")
            .map(|m| m.name.clone())
    })
    .unwrap_or_default();
    let hardware = caps.best_accel().is_some_and(|a| a.hardware);
    let cpu_model = if hardware {
        Some("host".to_string())
    } else if caps.cpus.iter().any(|c| c.name == "max") {
        Some("max".to_string())
    } else {
        None
    };
    let spec = VmSpec {
        spec_version: SPEC_VERSION,
        name: name.to_string(),
        arch: caps.arch.clone(),
        machine: MachineSpec {
            type_name: machine,
            props: BTreeMap::new(),
        },
        accel: "auto".into(),
        cpu: CpuSpec {
            model: cpu_model,
            props: BTreeMap::new(),
            count: 2,
            topology: None,
        },
        memory: MemorySpec { size_mib: 2048 },
        firmware: FirmwareSpec::Default,
        disks: vec![],
        nics: pick(&["virtio-net-pci", "e1000e", "e1000", "virtio-net-device"])
            .map(|model| NicSpec {
                backend: NicBackend::User { forwards: vec![] },
                model,
                mac: None,
            })
            .into_iter()
            .collect(),
        display: DisplaySpec {
            device: pick(&[
                "virtio-vga",
                "virtio-gpu-pci",
                "VGA",
                "bochs-display",
                "ramfb",
            ]),
            vnc: true,
            vdagent: false,
        },
        input: InputSpec {
            usb_tablet: has("usb-tablet") && has("qemu-xhci"),
        },
        shares: vec![],
        passthrough: vec![],
    };
    let diags = crate::compile::validate(&spec, caps)
        .into_iter()
        .filter(|d| d.rule != "nic.mac-missing")
        .collect();
    (spec, diags)
}

/// A locally administered unicast MAC in QEMU's `52:54:00` range.
pub fn mac_from_bytes(b: [u8; 3]) -> String {
    format!("52:54:00:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qmp_status_mapping() {
        assert_eq!(state_from_qmp("running"), (VmState::Running, None));
        assert_eq!(
            state_from_qmp("io-error"),
            (VmState::Paused, Some("io-error".into()))
        );
        assert_eq!(state_from_qmp("paused").1.as_deref(), Some("user"));
        for s in [
            "stopped", "starting", "running", "paused", "stopping", "crashed",
        ] {
            assert_eq!(VmState::parse(s).as_str(), s);
        }
    }

    #[test]
    fn macs_are_valid() {
        assert!(valid_mac(&mac_from_bytes([0xab, 0x01, 0xff])));
    }
}
