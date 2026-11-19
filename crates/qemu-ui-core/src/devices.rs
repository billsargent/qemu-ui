//! Generic devices, property editing and the hardware tree (M4-02, M4-05).
//!
//! * [`DeviceSpec`]: a `-device` the curated spec fields don't cover (e.g.
//!   `virtio-rng-pci`). Its QEMU id is `qui-xdev<N>` (N = its position).
//! * [`prop_fields`]: maps QEMU's introspected property types
//!   (`device-list-properties`) to editor widgets, so the property editor is
//!   generated rather than hand-written per device.
//! * [`palette`]: addable devices for a machine, from `-device help` and the
//!   machine's buses.
//! * [`device_tree`]: the VM's devices by bus, read from the compiled argv so
//!   it shows exactly what QEMU gets.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::buses::MachineBuses;
use crate::caps::{DeviceInfo, DeviceProperty};
use crate::compile::Diagnostic;
use crate::spec::VmSpec;

/// Prefix of generic devices' QEMU ids.
pub const GENERIC_ID_PREFIX: &str = "qui-xdev";

/// A device added from the palette.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct DeviceSpec {
    /// QEMU device type, e.g. `virtio-rng-pci`.
    pub driver: String,
    /// Bus to attach to (a QEMU bus id, e.g. `qui-usb.0`); `None` lets QEMU
    /// pick the machine's default bus for the device.
    #[serde(default)]
    pub bus: Option<String>,
    /// Properties the user set, typed as QEMU expects them (bool, number or
    /// string). Unset properties keep QEMU's defaults.
    #[serde(default)]
    #[ts(type = "Record<string, string | number | boolean>")]
    pub props: BTreeMap<String, Value>,
}

/// QEMU id of generic device `i`.
pub fn generic_id(i: usize) -> String {
    format!("{GENERIC_ID_PREFIX}{i}")
}

/// The `-device` JSON for generic device `i`.
pub fn device_json(i: usize, d: &DeviceSpec) -> Value {
    let mut dev = serde_json::json!({"driver": d.driver, "id": generic_id(i)});
    if let Some(b) = &d.bus {
        dev["bus"] = b.as_str().into();
    }
    for (k, v) in &d.props {
        dev[k] = v.clone();
    }
    dev
}

fn valid_prop_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Checks that need only the spec and the binary's device list.
pub fn validate_devices(spec: &VmSpec, devices: &[DeviceInfo]) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, dev) in spec.devices.iter().enumerate() {
        let f = |s: &str| format!("devices[{i}].{s}");
        match devices.iter().find(|x| x.name == dev.driver) {
            None => d.push(Diagnostic::error(
                f("driver"),
                "device.missing",
                format!("this QEMU has no '{}' device", dev.driver),
            )),
            Some(info) if info.no_user => d.push(Diagnostic::error(
                f("driver"),
                "device.not-user-creatable",
                format!(
                    "'{}' is built into machines and cannot be added with -device",
                    dev.driver
                ),
            )),
            Some(_) => {}
        }
        if let Some(b) = &dev.bus
            && (b.is_empty() || b.contains(',') || b.chars().any(char::is_whitespace))
        {
            d.push(Diagnostic::error(
                f("bus"),
                "device.bus-name",
                "bus must be a QEMU bus id such as qui-usb.0",
            ));
        }
        for (k, v) in &dev.props {
            let pf = format!("devices[{i}].props.{k}");
            if matches!(k.as_str(), "id" | "driver" | "bus") {
                d.push(Diagnostic::error(
                    &pf,
                    "device.reserved-prop",
                    format!("'{k}' is set by qemu-ui; use the device's own fields"),
                ));
            } else if !valid_prop_name(k) {
                d.push(Diagnostic::error(
                    &pf,
                    "device.prop-name",
                    format!("'{k}' is not a property name"),
                ));
            }
            if !(v.is_string() || v.is_number() || v.is_boolean()) {
                d.push(Diagnostic::error(
                    &pf,
                    "device.prop-value",
                    "property values are strings, numbers or booleans",
                ));
            }
        }
    }
    d
}

/// Bus checks once the machine's buses are known.
pub fn validate_device_buses(
    spec: &VmSpec,
    devices: &[DeviceInfo],
    machine: &MachineBuses,
) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, dev) in spec.devices.iter().enumerate() {
        // An explicit bus is checked by QEMU (it may be one of our own).
        if dev.bus.is_some() {
            continue;
        }
        let Some(info) = devices.iter().find(|x| x.name == dev.driver) else {
            continue;
        };
        if let Err(why) = bus_fits(info.bus.as_deref(), machine) {
            d.push(Diagnostic::error(
                format!("devices[{i}].driver"),
                "device.bus",
                why,
            ));
        }
    }
    d
}

/// Can a device of bus type `bus` attach to this machine without an
/// explicit bus? `Err` carries the reason.
pub fn bus_fits(bus: Option<&str>, m: &MachineBuses) -> Result<(), String> {
    let Some(bus) = bus else { return Ok(()) };
    let has = |t: &str| m.buses.iter().any(|b| b.bus_type == t);
    let ok = match bus {
        "PCI" => m.pci_root().is_some(),
        "virtio-bus" => m
            .buses
            .iter()
            .any(|b| b.bus_type.starts_with("virtio-mmio")),
        // USB devices go on a controller the VM may add (qemu-xhci).
        "usb-bus" => m.pci_root().is_some() || has("usb-bus"),
        // Buses provided by other devices (a SCSI HBA, virtio-serial, …):
        // add that device first and pick its bus explicitly.
        "SCSI" | "virtio-serial-bus" | "HDA" | "ccid-bus" => true,
        other => has(other),
    };
    if ok {
        return Ok(());
    }
    Err(match bus {
        "PCI" => format!("machine '{}' has no PCI bus", m.machine),
        "virtio-bus" => format!("machine '{}' has no virtio-mmio transport", m.machine),
        "usb-bus" => format!("machine '{}' cannot host a USB controller", m.machine),
        other => format!("machine '{}' has no {other} bus", m.machine),
    })
}

// ------------------------------------------------------------ the palette

/// An addable device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct PaletteEntry {
    pub name: String,
    pub category: String,
    pub bus: Option<String>,
    pub description: Option<String>,
    /// Fits the machine's buses (`true` when the machine wasn't probed).
    pub available: bool,
    /// Why not, when unavailable; or what else it needs.
    pub note: Option<String>,
}

/// Devices that can be added with `-device`, grouped by category order and
/// then name. Built-in (`no-user`) devices are left out.
pub fn palette(devices: &[DeviceInfo], machine: Option<&MachineBuses>) -> Vec<PaletteEntry> {
    let mut out: Vec<PaletteEntry> = devices
        .iter()
        .filter(|d| !d.no_user)
        .map(|d| {
            let fit = machine.map_or(Ok(()), |m| bus_fits(d.bus.as_deref(), m));
            let needs = match d.bus.as_deref() {
                Some("usb-bus") => Some("goes on a USB controller (qemu-xhci)".to_string()),
                Some("SCSI") => Some("needs a SCSI controller; set its bus".into()),
                Some("virtio-serial-bus") => {
                    Some("needs a virtio-serial controller; set its bus".into())
                }
                Some("HDA") => Some("needs an HDA controller (intel-hda); set its bus".into()),
                _ => None,
            };
            PaletteEntry {
                name: d.name.clone(),
                category: if d.category.is_empty() {
                    "Misc".into()
                } else {
                    d.category.clone()
                },
                bus: d.bus.clone(),
                description: d.description.clone(),
                available: fit.is_ok(),
                note: fit.err().or(needs),
            }
        })
        .collect();
    out.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    out
}

// ----------------------------------------------------- property → widget

/// How the editor presents a property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum Widget {
    Toggle,
    /// Whole number in `[min, max]` (as JSON numbers; 64-bit bounds may
    /// round in a browser, the daemon/QEMU check exactly).
    Integer {
        min: f64,
        max: f64,
    },
    /// Byte size; the editor accepts K/M/G suffixes and sends bytes.
    Size,
    Select {
        options: Vec<String>,
    },
    Text {
        pattern: Option<String>,
        hint: Option<String>,
    },
    /// A reference to a backend or object qemu-ui manages (drive, chardev,
    /// netdev, links); not editable here.
    Hidden {
        reason: String,
    },
}

/// One editable property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct PropField {
    pub name: String,
    /// QEMU's type name, e.g. `uint32`, `OnOffAuto`.
    pub type_name: String,
    pub description: Option<String>,
    #[ts(type = "unknown")]
    pub default_value: Option<Value>,
    pub widget: Widget,
}

/// Values of QAPI enums that show up as device property types.
fn enum_options(t: &str) -> Option<&'static [&'static str]> {
    Some(match t {
        "OnOffAuto" => &["auto", "on", "off"],
        "OnOffSplit" => &["on", "off", "split"],
        "LostTickPolicy" => &["discard", "delay", "slew"],
        "BiosAtaTranslation" => &["auto", "none", "lba", "large", "rechs"],
        "FdcDriveType" => &["144", "288", "120", "none", "auto"],
        "PCIELinkSpeed" => &["2_5", "5", "8", "16", "32", "64"],
        "PCIELinkWidth" => &["1", "2", "4", "8", "12", "16", "32"],
        "BlockdevOnError" => &["report", "ignore", "enospc", "stop", "auto"],
        "IOThreadVirtQueueMappingList" => return None,
        "MultiFDCompression" => &["none", "zlib", "zstd"],
        "OffAutoPCIBAR" => &[
            "off", "auto", "bar0", "bar1", "bar2", "bar3", "bar4", "bar5",
        ],
        "ZoneModel" => &["none", "host-managed"],
        "EndianMode" => &["unspecified", "little", "big"],
        _ => return None,
    })
}

/// The editor widget for a QEMU property type.
pub fn widget_for(type_name: &str) -> Widget {
    let t = type_name;
    let int = |min: f64, max: f64| Widget::Integer { min, max };
    match t {
        "bool" => Widget::Toggle,
        "uint8" => int(0.0, u8::MAX as f64),
        "uint16" => int(0.0, u16::MAX as f64),
        "uint32" => int(0.0, u32::MAX as f64),
        "uint64" => int(0.0, u64::MAX as f64),
        "int8" => int(i8::MIN as f64, i8::MAX as f64),
        "int16" => int(i16::MIN as f64, i16::MAX as f64),
        "int32" => int(i32::MIN as f64, i32::MAX as f64),
        "int64" => int(i64::MIN as f64, i64::MAX as f64),
        "size" => Widget::Size,
        "str" | "string" => Widget::Text {
            pattern: None,
            hint: None,
        },
        "macaddr" => Widget::Text {
            pattern: Some("^[0-9a-fA-F]{2}(:[0-9a-fA-F]{2}){5}$".into()),
            hint: Some("xx:xx:xx:xx:xx:xx".into()),
        },
        "pci-devfn" => Widget::Text {
            pattern: Some("^[0-9a-fA-F]{1,2}(\\.[0-7])?$".into()),
            hint: Some("slot[.function], e.g. 05.0".into()),
        },
        "QemuUUID" | "uuid" => Widget::Text {
            pattern: Some(
                "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
                    .into(),
            ),
            hint: Some("a UUID".into()),
        },
        "drive" | "str (drive)" => hidden("a block backend, managed by qemu-ui's disks"),
        "chr" | "str (chr)" => hidden("a chardev, managed by qemu-ui"),
        "netdev" | "str (netdev)" => hidden("a network backend, managed by qemu-ui's NICs"),
        "audiodev" => hidden("an audio backend"),
        "memory-backend" | "iothread" => hidden("an object reference"),
        _ if t.starts_with("link<") || t.starts_with("child<") => {
            hidden("a reference to another object")
        }
        _ => match enum_options(t) {
            Some(o) => Widget::Select {
                options: o.iter().map(|s| s.to_string()).collect(),
            },
            None => Widget::Text {
                pattern: None,
                hint: Some(format!("QEMU type {t}")),
            },
        },
    }
}

fn hidden(reason: &str) -> Widget {
    Widget::Hidden {
        reason: reason.into(),
    }
}

/// Editor fields for a device's properties, by name.
pub fn prop_fields(props: &[DeviceProperty]) -> Vec<PropField> {
    let mut out: Vec<PropField> = props
        .iter()
        .map(|p| PropField {
            name: p.name.clone(),
            type_name: p.type_name.clone(),
            description: p.description.clone(),
            default_value: p.default_value.clone(),
            widget: widget_for(&p.type_name),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse a size like `64M`, `1.5G`, `4096` into bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        'B' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    (v >= 0.0 && v.is_finite()).then(|| (v * mult as f64).round() as u64)
}

/// Turn editor input into the JSON value QEMU expects for `widget`.
pub fn encode_prop(widget: &Widget, input: &str) -> Result<Value, String> {
    match widget {
        Widget::Toggle => match input {
            "true" | "on" | "yes" => Ok(true.into()),
            "false" | "off" | "no" => Ok(false.into()),
            _ => Err("expected on or off".into()),
        },
        Widget::Integer { min, max } => {
            let v: i128 = input
                .trim()
                .parse()
                .map_err(|_| "expected a whole number".to_string())?;
            if (v as f64) < *min || (v as f64) > *max {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(serde_json::Number::from_i128(v)
                .map(Value::Number)
                .unwrap_or_else(|| Value::String(v.to_string())))
        }
        Widget::Size => parse_size(input)
            .map(Value::from)
            .ok_or_else(|| "expected a size like 64M".into()),
        Widget::Select { options } => {
            if options.iter().any(|o| o == input) {
                Ok(input.into())
            } else {
                Err(format!("expected one of {}", options.join(", ")))
            }
        }
        Widget::Text { pattern, .. } => {
            if let Some(p) = pattern
                && !simple_match(p, input)
            {
                return Err("not in the expected format".into());
            }
            Ok(input.into())
        }
        Widget::Hidden { reason } => Err(format!("not editable: {reason}")),
    }
}

/// Minimal check for the few anchored patterns `widget_for` emits (core has
/// no regex dependency; the browser does the full check).
fn simple_match(pattern: &str, s: &str) -> bool {
    if pattern.contains("{2}(:") {
        let parts: Vec<&str> = s.split(':').collect();
        return parts.len() == 6
            && parts
                .iter()
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    }
    if pattern.contains("\\.[0-7]") {
        let (slot, func) = s.split_once('.').unwrap_or((s, "0"));
        return (1..=2).contains(&slot.len())
            && slot.chars().all(|c| c.is_ascii_hexdigit())
            && matches!(func, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7");
    }
    if pattern.contains("{8}-") {
        let lens: Vec<usize> = s.split('-').map(str::len).collect();
        return lens == [8, 4, 4, 4, 12] && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit());
    }
    true
}

// --------------------------------------------------------- hardware tree

/// One device in the VM, from the compiled argv.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct DeviceNode {
    /// QEMU id (empty for anonymous passthrough devices).
    pub id: String,
    pub driver: String,
    /// The device whose bus this one sits on (a controller or root port).
    pub parent: Option<String>,
    /// The machine bus this one sits on, when not behind another device.
    pub bus: Option<String>,
    /// The spec field that owns it, e.g. `disks[0]`, `devices[2]`.
    pub field: Option<String>,
    /// Index into `spec.devices` for generic devices (editable here).
    pub generic: Option<usize>,
    /// Added through raw passthrough tokens.
    pub passthrough: bool,
}

/// A root bus of the machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct BusNode {
    pub name: String,
    pub bus_type: String,
}

/// The VM's devices by bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HardwareTree {
    pub machine: String,
    /// Whether the machine's buses came from introspection.
    pub probed: bool,
    pub buses: Vec<BusNode>,
    pub devices: Vec<DeviceNode>,
}

/// Parse a `-device` value: JSON or `driver,key=value,…`.
fn parse_device_arg(v: &str) -> Option<(String, String, Option<String>)> {
    if v.starts_with('{') {
        let j: Value = serde_json::from_str(v).ok()?;
        return Some((
            j["driver"].as_str()?.to_string(),
            j["id"].as_str().unwrap_or_default().to_string(),
            j["bus"].as_str().map(String::from),
        ));
    }
    let mut driver = None;
    let (mut id, mut bus) = (String::new(), None);
    for part in v.split(',') {
        match part.split_once('=') {
            Some(("driver", d)) => driver = Some(d.to_string()),
            Some(("id", i)) => id = i.to_string(),
            Some(("bus", b)) => bus = Some(b.to_string()),
            None if driver.is_none() => driver = Some(part.to_string()),
            _ => {}
        }
    }
    Some((driver?, id, bus))
}

/// Root bus names when the machine wasn't probed.
const PCI_FALLBACK: &str = "pci";
const SYSTEM_BUS: &str = "main-system-bus";

/// Build the tree from a compiled argv. `field_for_id` maps our ids to spec
/// fields (see `lint`); `passthrough_from` is the argv index where the
/// passthrough tokens start.
pub fn device_tree(
    args: &[String],
    passthrough_from: usize,
    spec: &VmSpec,
    devices: &[DeviceInfo],
    machine: Option<&MachineBuses>,
) -> HardwareTree {
    let pci_root = machine
        .and_then(|m| m.pci_root())
        .map_or(PCI_FALLBACK.to_string(), |b| b.name.clone());
    let mut buses: Vec<BusNode> = match machine {
        Some(m) => m
            .buses
            .iter()
            .filter(|b| b.bus_type != "System" || b.name == SYSTEM_BUS)
            .map(|b| BusNode {
                name: b.name.clone(),
                bus_type: b.bus_type.clone(),
            })
            .collect(),
        None => vec![],
    };

    let mut parsed = Vec::new();
    let mut i = 0;
    while i + 1 < args.len() {
        if args[i] == "-device"
            && let Some(p) = parse_device_arg(&args[i + 1])
        {
            parsed.push((p, i >= passthrough_from));
        }
        i += 1;
    }
    let ids: Vec<String> = parsed
        .iter()
        .map(|((_, id, _), _)| id.clone())
        .filter(|id| !id.is_empty())
        .collect();
    let first_usb = parsed
        .iter()
        .find(|((d, _, _), _)| d.contains("xhci") || d.contains("ehci") || d.contains("uhci"))
        .map(|((_, id, _), _)| id.clone());

    let mut out = Vec::new();
    for ((driver, id, bus), passthrough) in parsed {
        let (mut parent, mut on_bus) = (None, None);
        match bus {
            Some(b) => {
                // `<dev>.<n>` or `<dev>` (root ports name their bus after
                // themselves) points at another device; else a machine bus.
                let owner = b.rsplit_once('.').map_or(b.as_str(), |(o, _)| o);
                if ids.iter().any(|x| x == &b) {
                    parent = Some(b.clone());
                } else if ids.iter().any(|x| x == owner) {
                    parent = Some(owner.to_string());
                } else {
                    on_bus = Some(b.clone());
                }
            }
            None => {
                let kind = devices
                    .iter()
                    .find(|d| d.name == driver)
                    .and_then(|d| d.bus.clone());
                match kind.as_deref() {
                    Some("PCI") => on_bus = Some(pci_root.clone()),
                    Some("usb-bus") if first_usb.is_some() => parent = first_usb.clone(),
                    Some(other) => on_bus = Some(other.to_string()),
                    None => on_bus = Some(SYSTEM_BUS.to_string()),
                }
            }
        }
        let generic = id
            .strip_prefix(GENERIC_ID_PREFIX)
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n < spec.devices.len());
        let field = if let Some(n) = generic {
            Some(format!("devices[{n}]"))
        } else if passthrough {
            Some("passthrough".into())
        } else {
            crate::lint::field_for_id(&id, spec)
        };
        out.push(DeviceNode {
            id,
            driver,
            parent,
            bus: on_bus,
            field,
            generic,
            passthrough,
        });
    }
    // Buses devices sit on that the probe didn't list (or no probe).
    for d in &out {
        if let Some(b) = &d.bus
            && !buses.iter().any(|x| &x.name == b)
        {
            buses.push(BusNode {
                name: b.clone(),
                bus_type: if *b == pci_root {
                    "PCI".into()
                } else {
                    String::new()
                },
            });
        }
    }
    HardwareTree {
        machine: spec.machine.type_name.clone(),
        probed: machine.is_some(),
        buses,
        devices: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::BusInfo;

    fn info(name: &str, bus: Option<&str>, no_user: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            category: "Misc".into(),
            bus: bus.map(String::from),
            alias: None,
            description: None,
            no_user,
        }
    }

    fn q35() -> MachineBuses {
        MachineBuses {
            machine: "pc-q35-11.1".into(),
            buses: vec![
                BusInfo {
                    name: "main-system-bus".into(),
                    bus_type: "System".into(),
                },
                BusInfo {
                    name: "pcie.0".into(),
                    bus_type: "PCIE".into(),
                },
                BusInfo {
                    name: "ide.0".into(),
                    bus_type: "IDE".into(),
                },
            ],
        }
    }

    fn mmio_only() -> MachineBuses {
        MachineBuses {
            machine: "microvm".into(),
            buses: vec![BusInfo {
                name: "virtio-mmio-bus.0".into(),
                bus_type: "virtio-mmio-bus".into(),
            }],
        }
    }

    #[test]
    fn widgets_follow_qemu_types() {
        assert_eq!(widget_for("bool"), Widget::Toggle);
        assert_eq!(
            widget_for("uint8"),
            Widget::Integer {
                min: 0.0,
                max: 255.0
            }
        );
        assert_eq!(
            widget_for("int32"),
            Widget::Integer {
                min: -2147483648.0,
                max: 2147483647.0
            }
        );
        assert_eq!(widget_for("size"), Widget::Size);
        assert!(
            matches!(widget_for("OnOffAuto"), Widget::Select { options } if options == ["auto", "on", "off"])
        );
        assert!(matches!(
            widget_for("str"),
            Widget::Text { pattern: None, .. }
        ));
        assert!(matches!(widget_for("drive"), Widget::Hidden { .. }));
        assert!(matches!(widget_for("chr"), Widget::Hidden { .. }));
        assert!(matches!(
            widget_for("link<iothread>"),
            Widget::Hidden { .. }
        ));
        assert!(matches!(widget_for("child<foo>"), Widget::Hidden { .. }));
        // Unknown types stay editable as raw text, labelled with the type.
        assert!(
            matches!(widget_for("SomeNewEnum"), Widget::Text { hint: Some(h), .. } if h.contains("SomeNewEnum"))
        );
    }

    #[test]
    fn encoding_is_typed_and_checked() {
        assert_eq!(
            encode_prop(&Widget::Toggle, "on").unwrap(),
            Value::Bool(true)
        );
        assert!(encode_prop(&Widget::Toggle, "maybe").is_err());
        let u8w = widget_for("uint8");
        assert_eq!(encode_prop(&u8w, "200").unwrap(), serde_json::json!(200));
        assert!(encode_prop(&u8w, "256").is_err());
        assert!(encode_prop(&u8w, "-1").is_err());
        assert!(encode_prop(&u8w, "1.5").is_err());
        assert_eq!(
            encode_prop(&Widget::Size, "64M").unwrap(),
            serde_json::json!(64u64 << 20)
        );
        assert_eq!(
            encode_prop(&Widget::Size, "1.5k").unwrap(),
            serde_json::json!(1536)
        );
        assert!(encode_prop(&Widget::Size, "lots").is_err());
        assert_eq!(
            encode_prop(&widget_for("OnOffAuto"), "off").unwrap(),
            serde_json::json!("off")
        );
        assert!(encode_prop(&widget_for("OnOffAuto"), "sometimes").is_err());
        assert!(encode_prop(&widget_for("macaddr"), "52:54:00:12:34:56").is_ok());
        assert!(encode_prop(&widget_for("macaddr"), "52:54:00:12:34").is_err());
        assert!(encode_prop(&widget_for("pci-devfn"), "05.1").is_ok());
        assert!(encode_prop(&widget_for("pci-devfn"), "05.9").is_err());
        assert!(encode_prop(&widget_for("drive"), "x").is_err());
    }

    #[test]
    fn prop_fields_are_sorted_with_widgets() {
        let props = vec![
            DeviceProperty {
                name: "max-bytes".into(),
                type_name: "uint64".into(),
                description: None,
                default_value: None,
            },
            DeviceProperty {
                name: "chardev".into(),
                type_name: "chr".into(),
                description: None,
                default_value: None,
            },
        ];
        let f = prop_fields(&props);
        assert_eq!(f[0].name, "chardev");
        assert!(matches!(f[0].widget, Widget::Hidden { .. }));
        assert!(matches!(f[1].widget, Widget::Integer { .. }));
    }

    #[test]
    fn palette_hides_builtins_and_explains_unfit_buses() {
        let devs = vec![
            info("virtio-rng-pci", Some("PCI"), false),
            info("virtio-rng-device", Some("virtio-bus"), false),
            info("ICH9-LPC", Some("PCI"), true),
            info("usb-kbd", Some("usb-bus"), false),
        ];
        let p = palette(&devs, Some(&q35()));
        assert!(
            !p.iter().any(|e| e.name == "ICH9-LPC"),
            "built-in devices are not addable"
        );
        let get = |n: &str| p.iter().find(|e| e.name == n).unwrap().clone();
        assert!(get("virtio-rng-pci").available);
        assert!(!get("virtio-rng-device").available);
        assert!(
            get("virtio-rng-device")
                .note
                .unwrap()
                .contains("virtio-mmio")
        );
        assert!(get("usb-kbd").note.unwrap().contains("USB controller"));

        let p = palette(&devs, Some(&mmio_only()));
        let get = |n: &str| p.iter().find(|e| e.name == n).unwrap().clone();
        assert!(!get("virtio-rng-pci").available);
        assert!(get("virtio-rng-device").available);
        // Unprobed machine: everything is offered.
        assert!(palette(&devs, None).iter().all(|e| e.available));
    }

    fn spec_with(devices: Vec<DeviceSpec>) -> VmSpec {
        let mut s: VmSpec = serde_json::from_value(serde_json::json!({
            "spec_version": 1, "name": "t", "arch": "x86_64",
            "machine": {"type": "pc-q35-11.1"},
            "cpu": {"count": 1}, "memory": {"size_mib": 128},
        }))
        .unwrap();
        s.devices = devices;
        s
    }

    fn rng() -> DeviceSpec {
        DeviceSpec {
            driver: "virtio-rng-pci".into(),
            bus: None,
            props: BTreeMap::new(),
        }
    }

    #[test]
    fn validation() {
        let devs = vec![
            info("virtio-rng-pci", Some("PCI"), false),
            info("ICH9-LPC", Some("PCI"), true),
            info("virtio-rng-device", Some("virtio-bus"), false),
        ];
        let mut bad = rng();
        bad.props.insert("id".into(), "x".into());
        bad.props.insert("max-bytes".into(), serde_json::json!([1]));
        bad.props.insert("a b".into(), "x".into());
        let s = spec_with(vec![
            rng(),
            DeviceSpec {
                driver: "nope".into(),
                bus: None,
                props: BTreeMap::new(),
            },
            DeviceSpec {
                driver: "ICH9-LPC".into(),
                bus: None,
                props: BTreeMap::new(),
            },
            bad,
            DeviceSpec {
                driver: "virtio-rng-pci".into(),
                bus: Some("a,b".into()),
                props: BTreeMap::new(),
            },
        ]);
        let rules: Vec<(String, String)> = validate_devices(&s, &devs)
            .into_iter()
            .map(|d| (d.field, d.rule))
            .collect();
        assert!(
            !rules.iter().any(|(f, _)| f.starts_with("devices[0]")),
            "{rules:?}"
        );
        for want in [
            ("devices[1].driver", "device.missing"),
            ("devices[2].driver", "device.not-user-creatable"),
            ("devices[3].props.id", "device.reserved-prop"),
            ("devices[3].props.max-bytes", "device.prop-value"),
            ("devices[3].props.a b", "device.prop-name"),
            ("devices[4].bus", "device.bus-name"),
        ] {
            assert!(
                rules.contains(&(want.0.into(), want.1.into())),
                "missing {want:?} in {rules:?}"
            );
        }

        let s = spec_with(vec![DeviceSpec {
            driver: "virtio-rng-device".into(),
            bus: None,
            props: BTreeMap::new(),
        }]);
        let d = validate_device_buses(&s, &devs, &q35());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].rule, "device.bus");
    }

    #[test]
    fn device_json_carries_props_and_id() {
        let mut d = rng();
        d.props.insert("max-bytes".into(), serde_json::json!(1024));
        d.bus = Some("pcie.0".into());
        let j = device_json(3, &d);
        assert_eq!(
            j,
            serde_json::json!({"driver": "virtio-rng-pci", "id": "qui-xdev3", "bus": "pcie.0", "max-bytes": 1024})
        );
    }

    #[test]
    fn tree_places_devices_by_bus() {
        let devs = vec![
            info("virtio-rng-pci", Some("PCI"), false),
            info("qemu-xhci", Some("PCI"), false),
            info("usb-tablet", Some("usb-bus"), false),
            info("usb-kbd", Some("usb-bus"), false),
            info("pcie-root-port", Some("PCI"), false),
            info("virtio-blk-pci", Some("PCI"), false),
            info("ide-cd", Some("IDE"), false),
        ];
        let spec = spec_with(vec![rng()]);
        let dev = |j: Value| vec!["-device".to_string(), j.to_string()];
        let mut args = vec!["-machine".to_string(), "q35".to_string()];
        args.extend(dev(
            serde_json::json!({"driver": "qemu-xhci", "id": "qui-usb"}),
        ));
        args.extend(dev(
            serde_json::json!({"driver": "pcie-root-port", "id": "qui-rp0", "bus": "pcie.0"}),
        ));
        args.extend(dev(
            serde_json::json!({"driver": "virtio-blk-pci", "id": "qui-dev0", "bus": "qui-rp0"}),
        ));
        args.extend(dev(
            serde_json::json!({"driver": "ide-cd", "id": "qui-dev1", "bus": "ide.0"}),
        ));
        args.extend(dev(
            serde_json::json!({"driver": "usb-tablet", "id": "qui-tablet", "bus": "qui-usb.0"}),
        ));
        args.extend(dev(
            serde_json::json!({"driver": "virtio-rng-pci", "id": "qui-xdev0"}),
        ));
        let pt = args.len();
        args.extend(["-device".to_string(), "usb-kbd,id=kbd".to_string()]);

        let t = device_tree(&args, pt, &spec, &devs, Some(&q35()));
        assert!(t.probed);
        let node = |id: &str| t.devices.iter().find(|d| d.id == id).unwrap().clone();
        assert_eq!(node("qui-usb").bus.as_deref(), Some("pcie.0"));
        assert_eq!(node("qui-rp0").bus.as_deref(), Some("pcie.0"));
        assert_eq!(node("qui-dev0").parent.as_deref(), Some("qui-rp0"));
        assert_eq!(node("qui-dev0").field.as_deref(), Some("disks[0]"));
        assert_eq!(node("qui-dev1").bus.as_deref(), Some("ide.0"));
        assert_eq!(node("qui-tablet").parent.as_deref(), Some("qui-usb"));
        let x = node("qui-xdev0");
        assert_eq!(
            (x.bus.as_deref(), x.generic, x.field.as_deref()),
            (Some("pcie.0"), Some(0), Some("devices[0]"))
        );
        let k = node("kbd");
        assert!(k.passthrough);
        assert_eq!(
            k.parent.as_deref(),
            Some("qui-usb"),
            "USB devices without a bus go on the controller"
        );
        assert!(t.buses.iter().any(|b| b.name == "pcie.0"));
        assert!(
            !t.buses
                .iter()
                .any(|b| b.bus_type == "System" && b.name != "main-system-bus")
        );

        // Unprobed: a synthetic PCI root appears.
        let t = device_tree(&args, pt, &spec, &devs, None);
        assert!(!t.probed);
        assert!(t.buses.iter().any(|b| b.name == "pci"));
        assert_eq!(
            t.devices
                .iter()
                .find(|d| d.id == "qui-xdev0")
                .unwrap()
                .bus
                .as_deref(),
            Some("pci")
        );
    }
}
