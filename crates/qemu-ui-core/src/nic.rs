//! NIC models, virtio-net options and MAC addresses (plan.md §10.3, M3-07).
//!
//! * Models are offered per machine: a model fits when the machine has the
//!   bus it plugs into (from introspection, like disk buses), and the list is
//!   ordered by what most guests should use, each with a note on the guest
//!   drivers it needs.
//! * virtio-net offload toggles apply to virtio-net models only.
//! * Multiqueue needs a tap-style backend; slirp has a single queue.
//! * MACs are unique within a VM (error) and across the registry (allocation
//!   avoids every address in use; a shared one is a warning).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::buses::MachineBuses;
use crate::caps::DeviceInfo;
use crate::compile::Diagnostic;
use crate::spec::{NicBackend, NicSpec, VirtioNetOptions};
use crate::vm::mac_from_bytes;

/// Most queues a NIC may ask for (QEMU's own limit is higher; guests rarely
/// benefit beyond the vCPU count).
pub const MAX_QUEUES: u16 = 64;

/// Preferred models, in the order offered, with what the guest needs.
const PREFERRED: &[(&str, &str)] = &[
    (
        "virtio-net-pci",
        "Paravirtual and fastest. Linux and BSD have drivers built in; Windows needs the virtio-win drivers.",
    ),
    (
        "e1000e",
        "Intel 82574L. Drivers ship with Windows 8 and later and with Linux.",
    ),
    (
        "e1000",
        "Intel 82540EM. The widest legacy support (Windows XP onwards, older Linux and BSD).",
    ),
    (
        "rtl8139",
        "Realtek 8139. For very old guests (Windows 9x/2000, DOS packet drivers).",
    ),
    (
        "vmxnet3",
        "VMware paravirtual. For guests imaged on VMware with its tools installed.",
    ),
    ("ne2k_pci", "NE2000 on PCI. Ancient guests only; slow."),
    (
        "virtio-net-device",
        "Paravirtual over virtio-mmio, for machines without PCI. Needs virtio drivers.",
    ),
];

/// One NIC model as offered for a machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct NicModel {
    pub name: String,
    /// QEMU's description of the device, if any.
    pub description: Option<String>,
    /// What the guest needs to drive it.
    pub guest_note: Option<String>,
    /// One of the recommended models (listed first, in order).
    pub preferred: bool,
    /// The machine has the bus this model plugs into.
    pub available: bool,
    /// Why it is unavailable.
    pub reason: Option<String>,
}

/// Is `model` a virtio-net device (any transport)?
pub fn is_virtio_net(model: &str) -> bool {
    model.starts_with("virtio-net")
}

/// Can `bus` (a device's `-device help` bus) be found on the machine?
/// `Err` carries the reason. An unknown device bus is allowed.
pub fn bus_fits(bus: Option<&str>, m: &MachineBuses) -> Result<(), String> {
    let Some(bus) = bus else { return Ok(()) };
    let has = |t: &str| m.buses.iter().any(|b| b.bus_type == t);
    let ok = match bus {
        "PCI" => m.pci_root().is_some(),
        "virtio-bus" => m
            .buses
            .iter()
            .any(|b| b.bus_type.starts_with("virtio-mmio")),
        other => has(other),
    };
    if ok {
        return Ok(());
    }
    Err(match bus {
        "PCI" => format!("machine '{}' has no PCI bus", m.machine),
        "virtio-bus" => format!("machine '{}' has no virtio-mmio transport", m.machine),
        "usb-bus" => "USB network adapters are not supported here; use a PCI model".into(),
        other => format!("machine '{}' has no {other} bus", m.machine),
    })
}

fn device<'a>(devices: &'a [DeviceInfo], name: &str) -> Option<&'a DeviceInfo> {
    devices.iter().find(|d| d.name == name)
}

/// The NIC models this binary offers on a machine (`None`: not probed, so
/// every model counts as available), preferred models first.
pub fn nic_models(devices: &[DeviceInfo], machine: Option<&MachineBuses>) -> Vec<NicModel> {
    let mut out: Vec<NicModel> = devices
        .iter()
        .filter(|d| d.category == "Network" && !d.no_user)
        .map(|d| {
            let note = PREFERRED.iter().find(|(n, _)| *n == d.name);
            let fit = machine.map_or(Ok(()), |m| bus_fits(d.bus.as_deref(), m));
            NicModel {
                name: d.name.clone(),
                description: d.description.clone(),
                guest_note: note.map(|(_, s)| (*s).to_string()),
                preferred: note.is_some(),
                available: fit.is_ok(),
                reason: fit.err(),
            }
        })
        .collect();
    let rank = |n: &str| {
        PREFERRED
            .iter()
            .position(|(p, _)| *p == n)
            .unwrap_or(PREFERRED.len())
    };
    out.sort_by(|a, b| {
        rank(&a.name)
            .cmp(&rank(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Does the backend give the NIC more than one queue?
pub fn backend_supports_mq(backend: &NicBackend) -> bool {
    match backend {
        NicBackend::User { .. } => false,
        // Decided per network kind, which needs the network objects:
        // `compile::validate_tap_networks` refuses kinds without queues.
        NicBackend::Network { .. } => true,
    }
}

/// Validation that needs only the spec and caps: options, queues, MACs.
pub fn validate_nics(nics: &[NicSpec]) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    for (i, nic) in nics.iter().enumerate() {
        let f = |s: &str| format!("nics[{i}].{s}");
        if let Some(q) = nic.queues {
            if q == 0 || q > MAX_QUEUES {
                d.push(Diagnostic::error(
                    f("queues"),
                    "nic.queues-range",
                    format!("queues must be between 1 and {MAX_QUEUES}"),
                ));
            } else if q > 1 && !is_virtio_net(&nic.model) {
                d.push(Diagnostic::error(
                    f("queues"),
                    "nic.mq-model",
                    "multiple queues need a virtio-net model",
                ));
            } else if q > 1 && !backend_supports_mq(&nic.backend) {
                d.push(Diagnostic::error(
                    f("queues"),
                    "nic.mq-backend",
                    "NAT (slirp) has a single queue; multiqueue needs a bridged or tap network",
                ));
            }
        }
        if let Some(v) = &nic.virtio {
            if !is_virtio_net(&nic.model) {
                if *v != VirtioNetOptions::default() {
                    d.push(Diagnostic::error(
                        f("virtio"),
                        "nic.virtio-options",
                        format!(
                            "offload options apply to virtio-net models, not '{}'",
                            nic.model
                        ),
                    ));
                }
            } else if v.tso == Some(true) && v.csum == Some(false) {
                d.push(Diagnostic::error(
                    f("virtio.tso"),
                    "nic.tso-needs-csum",
                    "segmentation offload needs checksum offload",
                ));
            }
        }
        if let Some(mac) = &nic.mac {
            let norm = mac.to_ascii_lowercase();
            if let Some((_, j)) = seen.iter().find(|(m, _)| *m == norm) {
                d.push(Diagnostic::error(
                    f("mac"),
                    "nic.mac-duplicate",
                    format!("NIC {j} already uses {mac}"),
                ));
            } else {
                seen.push((norm, i));
            }
        }
    }
    d
}

/// Models that cannot plug into the machine are errors.
pub fn validate_nic_buses(
    nics: &[NicSpec],
    devices: &[DeviceInfo],
    m: &MachineBuses,
) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, nic) in nics.iter().enumerate() {
        let Some(dev) = device(devices, &nic.model) else {
            continue; // nic.model-missing covers it
        };
        if let Err(why) = bus_fits(dev.bus.as_deref(), m) {
            d.push(Diagnostic::error(
                format!("nics[{i}].model"),
                "nic.model-bus",
                why,
            ));
        }
    }
    d
}

/// Warnings for MACs another registered VM also uses. `others` yields
/// (VM name, its MACs).
pub fn shared_mac_warnings<'a>(
    nics: &[NicSpec],
    others: impl IntoIterator<Item = (&'a str, Vec<String>)>,
) -> Vec<Diagnostic> {
    let others: Vec<(&str, HashSet<String>)> = others
        .into_iter()
        .map(|(n, macs)| (n, macs.iter().map(|m| m.to_ascii_lowercase()).collect()))
        .collect();
    let mut d = Vec::new();
    for (i, nic) in nics.iter().enumerate() {
        let Some(mac) = &nic.mac else { continue };
        let norm = mac.to_ascii_lowercase();
        if let Some((name, _)) = others.iter().find(|(_, s)| s.contains(&norm)) {
            d.push(Diagnostic::warning(
                format!("nics[{i}].mac"),
                "nic.mac-shared",
                format!(
                    "VM '{name}' also uses {mac}; both on one network will conflict (clear the MAC to get a new one)"
                ),
            ));
        }
    }
    d
}

/// The MACs a spec uses, lowercased.
pub fn nic_macs(nics: &[NicSpec]) -> Vec<String> {
    nics.iter()
        .filter_map(|n| n.mac.as_ref().map(|m| m.to_ascii_lowercase()))
        .collect()
}

/// Give every NIC without a MAC a new one, avoiding `used` and each other.
/// `rand` supplies the three low bytes; a few collisions in a row just mean
/// more draws (the space has 16M addresses).
pub fn assign_macs(
    nics: &mut [NicSpec],
    used: &HashSet<String>,
    mut rand: impl FnMut() -> [u8; 3],
) {
    let mut taken: HashSet<String> = used.clone();
    taken.extend(
        nics.iter()
            .filter_map(|n| n.mac.as_ref().map(|m| m.to_ascii_lowercase())),
    );
    for nic in nics.iter_mut().filter(|n| n.mac.is_none()) {
        let mac = loop {
            let m = mac_from_bytes(rand());
            if !taken.contains(&m) {
                break m;
            }
        };
        taken.insert(mac.clone());
        nic.mac = Some(mac);
    }
}

/// Device properties for the NIC's virtio-net options and queues.
pub fn device_props(nic: &NicSpec, dev: &mut Value) {
    if !is_virtio_net(&nic.model) {
        return;
    }
    if let Some(v) = &nic.virtio {
        if let Some(c) = v.csum {
            dev["csum"] = c.into();
            dev["guest_csum"] = c.into();
        }
        // Without checksum offload there is no segmentation offload either.
        let tso = match (v.tso, v.csum) {
            (_, Some(false)) => Some(false),
            (t, _) => t,
        };
        if let Some(t) = tso {
            for p in ["host_tso4", "host_tso6", "guest_tso4", "guest_tso6"] {
                dev[p] = t.into();
            }
        }
        if let Some(m) = v.mrg_rxbuf {
            dev["mrg_rxbuf"] = m.into();
        }
    }
    if let Some(q) = nic.queues.filter(|q| *q > 1)
        && backend_supports_mq(&nic.backend)
    {
        dev["mq"] = true.into();
        // One vector per rx and tx queue, plus config and control.
        dev["vectors"] = json!(2 * u32::from(q) + 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::BusInfo;

    fn m(types: &[&str]) -> MachineBuses {
        MachineBuses {
            machine: "m".into(),
            buses: types
                .iter()
                .enumerate()
                .map(|(i, t)| BusInfo {
                    name: format!("b{i}"),
                    bus_type: (*t).into(),
                })
                .collect(),
        }
    }

    fn dev(name: &str, bus: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            category: "Network".into(),
            bus: Some(bus.into()),
            alias: None,
            description: None,
            no_user: false,
        }
    }

    fn nic(model: &str) -> NicSpec {
        NicSpec {
            backend: NicBackend::User { forwards: vec![] },
            model: model.into(),
            mac: None,
            queues: None,
            virtio: None,
        }
    }

    #[test]
    fn bus_fit() {
        let q35 = m(&["System", "PCIE", "ISA"]);
        let virt = m(&["System", "PCIE", "virtio-mmio-bus"]);
        let mmio_only = m(&["System", "virtio-mmio-bus"]);
        assert!(bus_fits(Some("PCI"), &q35).is_ok());
        assert!(bus_fits(Some("ISA"), &q35).is_ok());
        assert!(bus_fits(Some("ISA"), &virt).is_err());
        assert!(bus_fits(Some("virtio-bus"), &virt).is_ok());
        assert!(bus_fits(Some("virtio-bus"), &q35).is_err());
        assert!(
            bus_fits(Some("PCI"), &mmio_only)
                .unwrap_err()
                .contains("no PCI")
        );
        assert!(bus_fits(Some("usb-bus"), &q35).unwrap_err().contains("USB"));
        assert!(bus_fits(None, &mmio_only).is_ok());
    }

    #[test]
    fn models_are_ordered_and_filtered() {
        let devices = vec![
            dev("ne2k_isa", "ISA"),
            dev("rtl8139", "PCI"),
            dev("e1000", "PCI"),
            dev("virtio-net-pci", "PCI"),
            dev("igb", "PCI"),
            dev("virtio-net-device", "virtio-bus"),
        ];
        let q35 = m(&["PCIE", "ISA"]);
        let list = nic_models(&devices, Some(&q35));
        let names: Vec<&str> = list.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "virtio-net-pci",
                "e1000",
                "rtl8139",
                "virtio-net-device",
                "igb",
                "ne2k_isa"
            ]
        );
        assert!(list[0].guest_note.as_deref().unwrap().contains("Windows"));
        let mmio = list.iter().find(|x| x.name == "virtio-net-device").unwrap();
        assert!(!mmio.available && mmio.reason.is_some());
        assert!(nic_models(&devices, None).iter().all(|x| x.available));
    }

    fn rules(d: &[Diagnostic]) -> Vec<&str> {
        d.iter().map(|x| x.rule.as_str()).collect()
    }

    #[test]
    fn queues_and_options() {
        let mut a = nic("virtio-net-pci");
        a.queues = Some(4);
        assert_eq!(rules(&validate_nics(&([a]))), ["nic.mq-backend"]);
        let mut b = nic("e1000");
        b.queues = Some(2);
        assert_eq!(rules(&validate_nics(&([b]))), ["nic.mq-model"]);
        let mut c = nic("virtio-net-pci");
        c.queues = Some(0);
        assert_eq!(rules(&validate_nics(&([c]))), ["nic.queues-range"]);
        let mut ok = nic("virtio-net-pci");
        ok.queues = Some(1);
        assert!(validate_nics(&([ok])).is_empty());

        let mut e = nic("e1000");
        e.virtio = Some(VirtioNetOptions {
            csum: Some(false),
            ..Default::default()
        });
        assert_eq!(rules(&validate_nics(&([e]))), ["nic.virtio-options"]);
        let mut empty = nic("e1000");
        empty.virtio = Some(VirtioNetOptions::default());
        assert!(
            validate_nics(&([empty])).is_empty(),
            "an empty block is harmless"
        );
        let mut t = nic("virtio-net-pci");
        t.virtio = Some(VirtioNetOptions {
            csum: Some(false),
            tso: Some(true),
            mrg_rxbuf: None,
        });
        assert_eq!(rules(&validate_nics(&([t]))), ["nic.tso-needs-csum"]);
    }

    #[test]
    fn device_props_for_virtio() {
        let mut n = nic("virtio-net-pci");
        n.virtio = Some(VirtioNetOptions {
            csum: Some(false),
            tso: None,
            mrg_rxbuf: Some(false),
        });
        let mut dev = json!({"driver": "virtio-net-pci"});
        device_props(&n, &mut dev);
        assert_eq!(dev["csum"], false);
        assert_eq!(dev["guest_csum"], false);
        assert_eq!(dev["host_tso4"], false, "no TSO without checksum offload");
        assert_eq!(dev["mrg_rxbuf"], false);
        // Single-queue backend: no mq props even if asked.
        n.queues = Some(4);
        let mut dev = json!({"driver": "virtio-net-pci"});
        device_props(&n, &mut dev);
        assert!(dev.get("mq").is_none());
        // Non-virtio models get nothing.
        let mut e = nic("e1000");
        e.virtio = Some(VirtioNetOptions {
            csum: Some(true),
            ..Default::default()
        });
        let mut dev = json!({"driver": "e1000"});
        device_props(&e, &mut dev);
        assert_eq!(dev, json!({"driver": "e1000"}));
    }

    #[test]
    fn macs_unique_in_vm_and_registry() {
        let mut a = nic("e1000");
        a.mac = Some("52:54:00:00:00:01".into());
        let mut b = nic("e1000");
        b.mac = Some("52:54:00:00:00:01".into());
        assert_eq!(
            rules(&validate_nics(&([a.clone(), b]))),
            ["nic.mac-duplicate"]
        );

        let s = vec![a];
        let w = shared_mac_warnings(&s, [("other", vec!["52:54:00:00:00:01".to_string()])]);
        assert_eq!(rules(&w), ["nic.mac-shared"]);
        assert!(w[0].message.contains("other"));
        assert!(shared_mac_warnings(&s, [("x", vec!["52:54:00:00:00:02".into()])]).is_empty());

        // Allocation skips used addresses and keeps one VM's NICs distinct.
        let used: HashSet<String> = ["52:54:00:00:00:00".to_string()].into();
        let mut draws = vec![[0, 0, 0], [0, 0, 0], [0, 0, 7], [0, 0, 7], [0, 0, 9]].into_iter();
        let mut nics = vec![nic("e1000"), nic("e1000")];
        assign_macs(&mut nics, &used, || draws.next().unwrap());
        assert_eq!(nics[0].mac.as_deref(), Some("52:54:00:00:00:07"));
        assert_eq!(nics[1].mac.as_deref(), Some("52:54:00:00:00:09"));
    }
}
