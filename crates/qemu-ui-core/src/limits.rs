//! Per-VM resource limits (plan.md §13, M6-07) and how each platform
//! enforces them.
//!
//! * Disk IO: a QEMU `throttle-group` object plus a `throttle` filter node
//!   between each disk's format node and its protocol node. Portable (works
//!   on Windows too) and adjustable live with `qom-set`.
//! * CPU and memory: enforced outside QEMU — a cgroup v2 directory on Linux
//!   (through the privileged helper), a job object on Windows. Without either
//!   they are recorded but not enforced, and the UI says so.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::compile::Diagnostic;

/// The result of changing a VM's limits.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct LimitsChange {
    /// Every changed limit took effect on the running VM.
    pub live: bool,
    pub message: String,
    pub vm: crate::vm::VmDetail,
}

/// How limits are enforced on this host.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct LimitsSupport {
    /// `job-object`, `cgroup` or `none`.
    pub cpu_memory: String,
    /// Why CPU/memory limits are not enforced, when they aren't.
    pub reason: Option<String>,
    /// Disk IO throttling works everywhere.
    pub io: bool,
    /// Active Linux security module (`apparmor`, `selinux`), if any; see
    /// data/apparmor for a QEMU profile template.
    pub lsm: Option<String>,
}

/// QEMU id of the VM's throttle group.
pub const THROTTLE_GROUP: &str = "qui-throttle0";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[ts(export)]
pub struct ResourceLimits {
    /// CPU cap in percent of one host core (200 = two full cores).
    #[serde(default)]
    pub cpu_max_percent: Option<u32>,
    /// Relative CPU share against other VMs, 1–10000 (cgroup `cpu.weight`;
    /// 100 is the default share). Linux only.
    #[serde(default)]
    pub cpu_weight: Option<u32>,
    /// Hard memory limit for the whole QEMU process in MiB: guest RAM plus
    /// QEMU's own overhead.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub memory_max_mib: Option<u64>,
    /// Disk IO throttling shared by all of the VM's disks.
    #[serde(default)]
    pub io: IoLimits,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[ts(export)]
pub struct IoLimits {
    #[serde(default)]
    #[ts(type = "number | null")]
    pub read_bytes_per_sec: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub write_bytes_per_sec: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub read_iops: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub write_iops: Option<u64>,
}

impl IoLimits {
    pub fn is_set(&self) -> bool {
        self.read_bytes_per_sec.is_some()
            || self.write_bytes_per_sec.is_some()
            || self.read_iops.is_some()
            || self.write_iops.is_some()
    }

    /// The throttle group's `limits` property (`ThrottleLimits` in QAPI).
    /// Unset fields are 0, which QEMU reads as "no limit".
    pub fn qemu_limits(&self) -> Value {
        json!({
            "bps-read": self.read_bytes_per_sec.unwrap_or(0),
            "bps-write": self.write_bytes_per_sec.unwrap_or(0),
            "iops-read": self.read_iops.unwrap_or(0),
            "iops-write": self.write_iops.unwrap_or(0),
        })
    }
}

impl ResourceLimits {
    /// Limits that need an enforcer outside QEMU (cgroup or job object).
    pub fn needs_host_enforcement(&self) -> bool {
        self.cpu_max_percent.is_some() || self.cpu_weight.is_some() || self.memory_max_mib.is_some()
    }
}

/// The `-object` for the throttle group, when IO limits are set.
pub fn throttle_group_object(limits: &ResourceLimits) -> Option<Value> {
    limits.io.is_set().then(|| {
        json!({"qom-type": "throttle-group", "id": THROTTLE_GROUP, "limits": limits.io.qemu_limits()})
    })
}

/// Smallest memory limit we accept above the guest's RAM: QEMU itself needs
/// room for its code, device emulation and caches.
pub const MIN_OVERHEAD_MIB: u64 = 64;

/// Validate limits against the guest's size.
pub fn validate(limits: &ResourceLimits, vcpus: u32, guest_ram_mib: u64) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    if let Some(p) = limits.cpu_max_percent {
        let max = 100 * vcpus.max(1);
        if p == 0 || p > max {
            d.push(Diagnostic::error(
                "limits.cpu_max_percent",
                "limits.cpu-range",
                format!("the CPU cap must be 1–{max}% (100% per vCPU)"),
            ));
        }
    }
    if let Some(w) = limits.cpu_weight
        && !(1..=10000).contains(&w)
    {
        d.push(Diagnostic::error(
            "limits.cpu_weight",
            "limits.weight-range",
            "the CPU weight must be 1–10000 (100 is the default share)",
        ));
    }
    if let Some(m) = limits.memory_max_mib {
        let need = guest_ram_mib + MIN_OVERHEAD_MIB;
        if m < need {
            d.push(Diagnostic::error(
                "limits.memory_max_mib",
                "limits.memory-too-low",
                format!(
                    "the limit must leave room for the guest's {guest_ram_mib} MiB plus QEMU's own \
                     overhead: at least {need} MiB"
                ),
            ));
        }
    }
    let io = &limits.io;
    for (field, v) in [
        ("read_bytes_per_sec", io.read_bytes_per_sec),
        ("write_bytes_per_sec", io.write_bytes_per_sec),
        ("read_iops", io.read_iops),
        ("write_iops", io.write_iops),
    ] {
        if v == Some(0) {
            d.push(Diagnostic::error(
                format!("limits.io.{field}"),
                "limits.io-zero",
                "a limit of 0 would stop all IO; leave it empty for no limit",
            ));
        }
    }
    d
}

/// Put a disk behind the VM's throttle group: a `throttle` filter node goes
/// between the format node and its protocol node, so everything that names
/// the format node (devices, snapshots, media changes) is unaffected.
/// Returns the filter node, to be added after the protocol node.
pub fn throttle_disk(format: &mut Value) -> Value {
    let fmt_node = format["node-name"].as_str().unwrap_or_default().to_string();
    let filter = format!("{fmt_node}-throttle");
    let node = json!({
        "node-name": filter,
        "driver": "throttle",
        "throttle-group": THROTTLE_GROUP,
        "file": format["file"].clone(),
    });
    format["file"] = filter.into();
    node
}

// ------------------------------------------------------------ enforcement

/// cgroup v2 interface files for the limits (Linux), as `(file, value)`.
/// `cpu.max` is `"<quota> <period>"` in microseconds.
pub fn cgroup_v2_settings(limits: &ResourceLimits) -> Vec<(&'static str, String)> {
    const PERIOD: u64 = 100_000;
    let mut out = Vec::new();
    if let Some(p) = limits.cpu_max_percent {
        out.push((
            "cpu.max",
            format!("{} {PERIOD}", PERIOD * u64::from(p) / 100),
        ));
    }
    if let Some(w) = limits.cpu_weight {
        out.push(("cpu.weight", w.to_string()));
    }
    if let Some(m) = limits.memory_max_mib {
        out.push(("memory.max", (m * 1024 * 1024).to_string()));
        // No swap: a VM pushed into swap is slower than one that is told no.
        out.push(("memory.swap.max", "0".into()));
    }
    out
}

/// Windows job object limits: `cpu_rate` is in hundredths of a percent of
/// the whole machine (the `CpuRate` of `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobLimits {
    pub memory_bytes: Option<u64>,
    pub cpu_rate: Option<u32>,
}

pub fn job_limits(limits: &ResourceLimits, host_cpus: u32) -> JobLimits {
    JobLimits {
        memory_bytes: limits.memory_max_mib.map(|m| m * 1024 * 1024),
        cpu_rate: limits.cpu_max_percent.map(|p| {
            // p is "percent of one core"; the job wants a share of all cores.
            (u64::from(p) * 100 / u64::from(host_cpus.max(1))).clamp(1, 10_000) as u32
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(r: Option<u64>) -> ResourceLimits {
        ResourceLimits {
            io: IoLimits {
                read_bytes_per_sec: r,
                ..IoLimits::default()
            },
            ..ResourceLimits::default()
        }
    }

    #[test]
    fn throttle_object_only_when_io_is_limited() {
        assert!(throttle_group_object(&ResourceLimits::default()).is_none());
        let o = throttle_group_object(&io(Some(1 << 20))).unwrap();
        assert_eq!(o["qom-type"], "throttle-group");
        assert_eq!(o["id"], THROTTLE_GROUP);
        assert_eq!(o["limits"]["bps-read"], 1 << 20);
        assert_eq!(o["limits"]["iops-write"], 0, "unset means unlimited");
    }

    #[test]
    fn validation() {
        let ok = ResourceLimits {
            cpu_max_percent: Some(150),
            cpu_weight: Some(200),
            memory_max_mib: Some(1024 + 64),
            io: IoLimits::default(),
        };
        assert!(validate(&ok, 2, 1024).is_empty());
        let rules = |l: &ResourceLimits| -> Vec<String> {
            validate(l, 2, 1024).into_iter().map(|d| d.rule).collect()
        };
        assert_eq!(
            rules(&ResourceLimits {
                cpu_max_percent: Some(201),
                ..ok.clone()
            }),
            ["limits.cpu-range"]
        );
        assert_eq!(
            rules(&ResourceLimits {
                cpu_weight: Some(0),
                ..ok.clone()
            }),
            ["limits.weight-range"]
        );
        assert_eq!(
            rules(&ResourceLimits {
                memory_max_mib: Some(1024),
                ..ok.clone()
            }),
            ["limits.memory-too-low"]
        );
        assert_eq!(rules(&io(Some(0))), ["limits.io-zero"]);
    }

    #[test]
    fn cgroup_and_job_mapping() {
        let l = ResourceLimits {
            cpu_max_percent: Some(50),
            cpu_weight: Some(300),
            memory_max_mib: Some(2048),
            io: IoLimits::default(),
        };
        let s = cgroup_v2_settings(&l);
        assert!(s.contains(&("cpu.max", "50000 100000".into())));
        assert!(s.contains(&("cpu.weight", "300".into())));
        assert!(s.contains(&("memory.max", (2048u64 << 20).to_string())));
        let j = job_limits(&l, 8);
        assert_eq!(j.memory_bytes, Some(2048 << 20));
        assert_eq!(
            j.cpu_rate,
            Some(625),
            "half a core of eight = 6.25% of the machine"
        );
        assert_eq!(
            job_limits(&ResourceLimits::default(), 8),
            JobLimits {
                memory_bytes: None,
                cpu_rate: None
            }
        );
    }

    #[test]
    fn old_specs_load() {
        let l: ResourceLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(l, ResourceLimits::default());
    }
}
