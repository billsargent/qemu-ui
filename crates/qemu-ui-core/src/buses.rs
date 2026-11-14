//! Which disk buses a machine can host (plan.md §9.5, M2-09).
//!
//! Derived from introspection, never from machine names: the daemon starts
//! the machine paused, asks QEMU for its device tree (`info qtree`), and the
//! bus types found there decide what a disk can attach to:
//!
//! * `PCI` / `PCIE` root bus → virtio-blk, NVMe, virtio-scsi, USB (xHCI);
//! * `IDE` buses (PIIX IDE, ICH9 AHCI) → IDE / SATA disks and CD-ROMs;
//! * neither (e.g. virtio-mmio only, CCW only) → the bus is unavailable and
//!   the reason names what the machine has instead.
//!
//! Hotplug: PCI root buses accept hotplug directly; PCIe root buses do not,
//! so the compiler adds spare `pcie-root-port`s and puts PCI disks behind
//! them. IDE never supports hotplug.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::caps::BinaryCaps;
use crate::spec::DiskBus;

/// Spare PCIe root ports added on PCIe machines, for disks hotplugged
/// while the VM runs (on top of one per cold-plugged PCI disk).
pub const SPARE_ROOT_PORTS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct BusInfo {
    /// QEMU bus id, e.g. `pcie.0`, `ide.1`.
    pub name: String,
    /// QEMU bus type, e.g. `PCIE`, `PCI`, `IDE`, `virtio-mmio-bus`.
    pub bus_type: String,
}

/// The buses one machine type provides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct MachineBuses {
    pub machine: String,
    pub buses: Vec<BusInfo>,
}

/// Whether a disk bus can be used on a machine, and why not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct DiskBusSupport {
    pub bus: DiskBus,
    pub available: bool,
    /// Disks on this bus can be added and removed while the VM runs.
    pub hotplug: bool,
    /// Why the bus is unavailable, or why hotplug is not possible.
    pub reason: Option<String>,
}

/// Parse `info qtree` (HMP) into the list of buses.
///
/// ```text
/// bus: main-system-bus
///   type System
///   dev: q35-pcihost, id ""
///     bus: pcie.0
///       type PCIE
/// ```
pub fn parse_qtree(text: &str) -> Vec<BusInfo> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim).peekable();
    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("bus: ") else {
            continue;
        };
        let bus_type = lines
            .peek()
            .and_then(|l| l.strip_prefix("type "))
            .unwrap_or_default()
            .to_string();
        out.push(BusInfo {
            name: name.trim().to_string(),
            bus_type,
        });
    }
    out
}

impl MachineBuses {
    /// The PCI(e) root bus, if the machine has one.
    pub fn pci_root(&self) -> Option<&BusInfo> {
        // The root is the first PCI-type bus in the tree (bridges come later).
        self.buses
            .iter()
            .find(|b| b.bus_type == "PCIE" || b.bus_type == "PCI")
    }

    /// PCI devices must go behind root ports to be hot-pluggable.
    pub fn pcie_root(&self) -> bool {
        self.pci_root().is_some_and(|b| b.bus_type == "PCIE")
    }

    pub fn has_ide(&self) -> bool {
        self.buses.iter().any(|b| b.bus_type == "IDE")
    }

    /// A short description of what the machine has, for "why" messages.
    fn summary(&self) -> String {
        let mut types: Vec<&str> = self
            .buses
            .iter()
            .map(|b| b.bus_type.as_str())
            .filter(|t| !t.is_empty() && *t != "System")
            .collect();
        types.sort();
        types.dedup();
        if types.is_empty() {
            "no expansion buses".into()
        } else {
            types.join(", ")
        }
    }

    /// Support for each disk bus on this machine.
    pub fn disk_buses(&self, caps: &BinaryCaps) -> Vec<DiskBusSupport> {
        let has_device = |d: &str| caps.devices.iter().any(|x| x.name == d);
        let pci = self.pci_root().is_some();
        let pci_reason = || {
            format!(
                "machine '{}' has no PCI bus (it has: {})",
                self.machine,
                self.summary()
            )
        };
        let pcie_ports = self.pcie_root() && has_device("pcie-root-port");
        let pci_hotplug = |dev: &str| -> (bool, Option<String>) {
            if self.pcie_root() && !pcie_ports {
                (
                    false,
                    Some(format!(
                        "this QEMU has no pcie-root-port, so {dev} cannot be hotplugged"
                    )),
                )
            } else {
                (true, None)
            }
        };
        let mut out = Vec::new();
        for (bus, device) in [
            (DiskBus::Virtio, "virtio-blk-pci"),
            (DiskBus::Scsi, "virtio-scsi-pci"),
            (DiskBus::Nvme, "nvme"),
            (DiskBus::Usb, "qemu-xhci"),
        ] {
            let entry = if !has_device(device) {
                DiskBusSupport {
                    bus,
                    available: false,
                    hotplug: false,
                    reason: Some(format!("this QEMU has no '{device}' device")),
                }
            } else if !pci {
                DiskBusSupport {
                    bus,
                    available: false,
                    hotplug: false,
                    reason: Some(pci_reason()),
                }
            } else {
                let (hotplug, reason) = pci_hotplug(device);
                DiskBusSupport {
                    bus,
                    available: true,
                    hotplug,
                    reason,
                }
            };
            out.push(entry);
        }
        out.push(if self.has_ide() {
            DiskBusSupport {
                bus: DiskBus::Ide,
                available: true,
                hotplug: false,
                reason: Some("IDE/SATA disks cannot be added or removed while the VM runs".into()),
            }
        } else {
            DiskBusSupport {
                bus: DiskBus::Ide,
                available: false,
                hotplug: false,
                reason: Some(format!(
                    "machine '{}' has no IDE/SATA controller (it has: {})",
                    self.machine,
                    self.summary()
                )),
            }
        });
        out
    }

    pub fn support(&self, caps: &BinaryCaps, bus: DiskBus) -> DiskBusSupport {
        self.disk_buses(caps)
            .into_iter()
            .find(|s| s.bus == bus)
            .expect("every DiskBus is covered")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::*;

    const Q35: &str = include_str!("../tests/fixtures/qtree-x86_64-q35.txt");
    const PC: &str = include_str!("../tests/fixtures/qtree-x86_64-pc.txt");
    const VIRT: &str = include_str!("../tests/fixtures/qtree-aarch64-virt.txt");

    fn caps(devices: &[&str]) -> BinaryCaps {
        BinaryCaps {
            binary: Binary {
                id: String::new(),
                path: String::new(),
                arch: None,
                version: String::new(),
                sha256: String::new(),
                source: String::new(),
            },
            arch: "x86_64".into(),
            qemu_version: QemuVersion {
                major: 11,
                minor: 1,
                micro: 0,
                package: String::new(),
            },
            probe_version: PROBE_VERSION,
            probed_at: 0,
            native: true,
            machines: vec![],
            cpus: vec![],
            devices: devices
                .iter()
                .map(|n| DeviceInfo {
                    name: n.to_string(),
                    category: String::new(),
                    bus: None,
                    alias: None,
                    description: None,
                    no_user: false,
                })
                .collect(),
            accels: vec![],
            backends: Backends::default(),
            qmp_schema_hash: String::new(),
            warnings: vec![],
        }
    }

    const ALL: &[&str] = &[
        "virtio-blk-pci",
        "virtio-scsi-pci",
        "nvme",
        "qemu-xhci",
        "pcie-root-port",
    ];

    fn get(m: &MachineBuses, c: &BinaryCaps, bus: DiskBus) -> DiskBusSupport {
        m.support(c, bus)
    }

    #[test]
    fn q35_pcie_with_ahci() {
        let m = MachineBuses {
            machine: "pc-q35-11.1".into(),
            buses: parse_qtree(Q35),
        };
        assert_eq!(m.pci_root().unwrap().name, "pcie.0");
        assert!(m.pcie_root());
        let c = caps(ALL);
        let v = get(&m, &c, DiskBus::Virtio);
        assert!(v.available && v.hotplug, "{v:?}");
        let ide = get(&m, &c, DiskBus::Ide);
        assert!(ide.available && !ide.hotplug);
        // Without pcie-root-port, PCI disks still work but cannot be hotplugged.
        let no_ports = caps(&["virtio-blk-pci"]);
        let v = get(&m, &no_ports, DiskBus::Virtio);
        assert!(v.available && !v.hotplug && v.reason.unwrap().contains("pcie-root-port"));
    }

    #[test]
    fn pc_pci_with_piix_ide() {
        let m = MachineBuses {
            machine: "pc-i440fx-11.1".into(),
            buses: parse_qtree(PC),
        };
        assert_eq!(m.pci_root().unwrap().name, "pci.0");
        assert!(!m.pcie_root());
        let c = caps(&["virtio-blk-pci"]);
        let v = get(&m, &c, DiskBus::Virtio);
        assert!(v.available && v.hotplug, "PCI root hotplugs directly");
        assert!(m.has_ide());
    }

    #[test]
    fn arm_virt_has_pcie_but_no_ide() {
        let m = MachineBuses {
            machine: "virt-11.1".into(),
            buses: parse_qtree(VIRT),
        };
        assert!(m.pcie_root());
        let c = caps(ALL);
        assert!(get(&m, &c, DiskBus::Virtio).available);
        let ide = get(&m, &c, DiskBus::Ide);
        assert!(!ide.available);
        let why = ide.reason.unwrap();
        assert!(
            why.contains("no IDE/SATA controller") && why.contains("virtio-mmio-bus"),
            "{why}"
        );
    }

    #[test]
    fn no_pci_explains_what_exists() {
        let m = MachineBuses {
            machine: "microvm".into(),
            buses: vec![BusInfo {
                name: "virtio-mmio-bus.0".into(),
                bus_type: "virtio-mmio-bus".into(),
            }],
        };
        let v = get(&m, &caps(ALL), DiskBus::Virtio);
        assert!(!v.available);
        assert!(
            v.reason
                .unwrap()
                .contains("no PCI bus (it has: virtio-mmio-bus)")
        );
    }
}
