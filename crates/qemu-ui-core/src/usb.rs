//! USB passthrough (plan.md §9.6, M2-12): host device model and the pure
//! parsers that turn platform enumeration output into it.
//!
//! * Linux: attributes read from `/sys/bus/usb/devices/<dev>/…` (the daemon
//!   reads the files; [`parse_sysfs`] interprets them).
//! * Windows: `Win32_PnPEntity` rows for `USB\VID_…` instances, as JSON from
//!   `Get-CimInstance … | ConvertTo-Json` ([`parse_windows_pnp`]).
//!
//! QEMU's `usb-host` uses libusb. On Linux libusb detaches the kernel driver
//! itself, so a device is usable when its device node is accessible. On
//! Windows libusb can only open devices bound to a libusb-compatible driver
//! (WinUSB, libusbK, libusb0); devices on their normal driver (HID, mass
//! storage, …) need the driver replaced first, typically with Zadig.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::spec::UsbPassthrough;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum UsbUsability {
    /// QEMU should be able to open it.
    Ready,
    /// Works after a host-side change (driver or permissions); see `note`.
    NeedsSetup,
    /// Hubs and controllers cannot be passed through.
    Unsupported,
    Unknown,
}

/// A USB device present on the host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct UsbDevice {
    /// 4 lowercase hex digits.
    pub vendor_id: String,
    pub product_id: String,
    /// Linux bus number (matches QEMU `hostbus`).
    pub bus: Option<u32>,
    /// Linux device address (`hostaddr`); changes on every replug.
    pub address: Option<u32>,
    /// Linux port path, e.g. `1.2` (QEMU `hostport`); stable per physical port.
    pub port: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    /// Bound driver(s), e.g. `usbhid`, `HidUsb`, `WINUSB`.
    pub drivers: Vec<String>,
    pub usability: UsbUsability,
    pub note: Option<String>,
}

impl UsbDevice {
    /// A human label: product name, or vendor:product.
    pub fn label(&self) -> String {
        match (&self.manufacturer, &self.product) {
            (Some(m), Some(p)) if !p.contains(m.as_str()) => format!("{m} {p}"),
            (_, Some(p)) => p.clone(),
            _ => format!("{}:{}", self.vendor_id, self.product_id),
        }
    }

    /// Does this host device match a passthrough rule?
    pub fn matches(&self, rule: &UsbPassthrough) -> bool {
        self.vendor_id.eq_ignore_ascii_case(&rule.vendor_id)
            && self.product_id.eq_ignore_ascii_case(&rule.product_id)
            && rule.host_bus.is_none_or(|b| self.bus == Some(b))
            && rule
                .host_port
                .as_ref()
                .is_none_or(|p| self.port.as_ref() == Some(p))
    }
}

/// Host USB inventory, as served by `GET /api/v1/host/usb`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HostUsb {
    pub devices: Vec<UsbDevice>,
    /// The installed QEMU has the `usb-host` device (built with libusb).
    pub qemu_supported: bool,
    /// Enumeration failed or is not implemented on this host.
    pub error: Option<String>,
}

/// Normalise a vendor/product id to 4 lowercase hex digits.
pub fn normalize_id(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    (s.len() == 4 && s.chars().all(|c| c.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// Is `s` a Linux USB port path like `1`, `1.2` or `1.2.3`?
pub fn valid_port(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The QEMU device id for passthrough rule `index` of `rules`: stable for a
/// given device (not positional), so live attach/detach and the compiled argv
/// agree after other rules are added or removed.
pub fn device_id(rules: &[UsbPassthrough], index: usize) -> String {
    let r = &rules[index];
    let base = format!(
        "qui-usb-{}-{}",
        r.vendor_id.to_ascii_lowercase(),
        r.product_id.to_ascii_lowercase()
    );
    let dup = rules[..index]
        .iter()
        .filter(|o| {
            o.vendor_id.eq_ignore_ascii_case(&r.vendor_id)
                && o.product_id.eq_ignore_ascii_case(&r.product_id)
        })
        .count();
    if dup == 0 {
        base
    } else {
        format!("{base}-{dup}")
    }
}

// ------------------------------------------------------------------ Linux

/// One `/sys/bus/usb/devices/<name>` device directory, already read.
#[derive(Debug, Clone, Default)]
pub struct SysfsDevice {
    /// Directory name, e.g. `1-2.3` (or `usb1` for root hubs).
    pub name: String,
    /// Attribute file → trimmed contents (`idVendor`, `busnum`, …).
    pub attrs: BTreeMap<String, String>,
    /// Drivers bound to the device's interfaces (`<name>:1.0/driver`).
    pub interface_drivers: Vec<String>,
    /// Whether `/dev/bus/usb/BBB/DDD` is readable and writable by the daemon.
    pub node_accessible: Option<bool>,
}

/// Interpret sysfs device directories. Interface directories (`1-2:1.0`)
/// must not be passed in; root hubs and hubs are reported as unsupported.
pub fn parse_sysfs(devs: &[SysfsDevice]) -> Vec<UsbDevice> {
    let mut out: Vec<UsbDevice> = devs
        .iter()
        .filter(|d| !d.name.contains(':'))
        .filter_map(|d| {
            let a = |k: &str| d.attrs.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let vendor_id = normalize_id(&a("idVendor")?)?;
            let product_id = normalize_id(&a("idProduct")?)?;
            let hub = a("bDeviceClass").as_deref() == Some("09") || d.name.starts_with("usb");
            // "1-2.3" → port path "2.3" on bus 1.
            let port = d.name.split_once('-').map(|(_, p)| p.to_string()).filter(|p| valid_port(p));
            let mut drivers = d.interface_drivers.clone();
            drivers.sort();
            drivers.dedup();
            let (usability, note) = if hub {
                (UsbUsability::Unsupported, Some("Hubs cannot be passed through; pass the devices behind it.".into()))
            } else {
                match d.node_accessible {
                    Some(true) => (UsbUsability::Ready, None),
                    Some(false) => (
                        UsbUsability::NeedsSetup,
                        Some(
                            "The daemon cannot open this device's node under /dev/bus/usb. Grant access with a \
                             udev rule (e.g. GROUP or MODE for this vendor/product) and replug it."
                                .into(),
                        ),
                    ),
                    None => (UsbUsability::Unknown, None),
                }
            };
            Some(UsbDevice {
                vendor_id,
                product_id,
                bus: a("busnum").and_then(|v| v.parse().ok()),
                address: a("devnum").and_then(|v| v.parse().ok()),
                port,
                manufacturer: a("manufacturer"),
                product: a("product"),
                drivers,
                usability,
                note,
            })
        })
        .collect();
    out.sort_by(|x, y| (x.bus, &x.port).cmp(&(y.bus, &y.port)));
    out
}

// ---------------------------------------------------------------- Windows

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PnpRow {
    #[serde(rename = "PNPDeviceID")]
    pnp_device_id: Option<String>,
    name: Option<String>,
    manufacturer: Option<String>,
    service: Option<String>,
    #[serde(rename = "PNPClass")]
    pnp_class: Option<String>,
}

/// Drivers through which libusb (and so QEMU) can open a device on Windows.
const LIBUSB_DRIVERS: &[&str] = &["winusb", "libusbk", "libusb0"];

/// `(vid, pid, interface)` from `USB\VID_046D&PID_C52B&MI_00\…`.
fn parse_instance_id(id: &str) -> Option<(String, String, Option<String>)> {
    let hw = id.strip_prefix("USB\\")?.split('\\').next()?;
    let mut vid = None;
    let mut pid = None;
    let mut mi = None;
    for part in hw.split('&') {
        if let Some(v) = part.strip_prefix("VID_") {
            vid = normalize_id(v);
        } else if let Some(v) = part.strip_prefix("PID_") {
            pid = normalize_id(v);
        } else if let Some(v) = part.strip_prefix("MI_") {
            mi = Some(v.to_string());
        }
    }
    Some((vid?, pid?, mi))
}

/// Interpret `Win32_PnPEntity` rows (a JSON array, or a single object when
/// there is one row) for `USB\VID_…` instances. Composite devices appear as a
/// parent (`usbccgp`) plus one row per interface (`MI_xx`); they are merged.
pub fn parse_windows_pnp(json: &str) -> Result<Vec<UsbDevice>, String> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).map_err(|e| e.to_string())?;
    let rows: Vec<PnpRow> = match v {
        serde_json::Value::Array(_) => serde_json::from_value(v).map_err(|e| e.to_string())?,
        serde_json::Value::Null => vec![],
        other => vec![serde_json::from_value(other).map_err(|e| e.to_string())?],
    };

    struct Acc {
        name: Option<String>,
        manufacturer: Option<String>,
        class: Option<String>,
        own_driver: Option<String>,
        iface_names: Vec<String>,
        iface_drivers: Vec<String>,
    }
    let mut by_id: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for r in rows {
        let Some((vid, pid, mi)) = r.pnp_device_id.as_deref().and_then(parse_instance_id) else {
            continue;
        };
        let acc = by_id.entry((vid, pid)).or_insert(Acc {
            name: None,
            manufacturer: None,
            class: None,
            own_driver: None,
            iface_names: vec![],
            iface_drivers: vec![],
        });
        if mi.is_some() {
            acc.iface_names.extend(r.name);
            acc.iface_drivers.extend(r.service);
        } else {
            acc.name = r.name;
            acc.manufacturer = r.manufacturer.filter(|m| !m.starts_with('('));
            acc.class = r.pnp_class;
            acc.own_driver = r.service;
        }
    }

    let generic = |n: &str| {
        matches!(
            n,
            "USB Composite Device" | "USB Input Device" | "USB Mass Storage Device"
        )
    };
    let mut out = Vec::new();
    for ((vendor_id, product_id), acc) in by_id {
        let product = acc
            .name
            .clone()
            .filter(|n| !generic(n))
            .or_else(|| acc.iface_names.iter().find(|n| !generic(n)).cloned())
            .or(acc.name.clone());
        let mut drivers: Vec<String> = acc
            .own_driver
            .iter()
            .chain(acc.iface_drivers.iter())
            .cloned()
            .collect();
        drivers.dedup();
        let lower: Vec<String> = drivers.iter().map(|d| d.to_ascii_lowercase()).collect();
        let is_hub = lower.iter().any(|d| d == "usbhub" || d == "usbhub3")
            || product
                .as_deref()
                .is_some_and(|p| p.to_ascii_lowercase().contains("hub"));
        let (usability, note) = if is_hub {
            (
                UsbUsability::Unsupported,
                Some("Hubs cannot be passed through; pass the devices behind it.".into()),
            )
        } else if lower.iter().any(|d| LIBUSB_DRIVERS.contains(&d.as_str())) {
            (UsbUsability::Ready, None)
        } else {
            let current = drivers
                .iter()
                .find(|d| !d.eq_ignore_ascii_case("usbccgp"))
                .or(drivers.first())
                .cloned()
                .unwrap_or_else(|| "no driver".into());
            (
                UsbUsability::NeedsSetup,
                Some(format!(
                    "Windows uses the {current} driver for this device. QEMU on Windows can only open devices \
                     bound to WinUSB (or libusbK/libusb0), for example with the Zadig tool. While it is bound to \
                     WinUSB the host itself cannot use it."
                )),
            )
        };
        let _ = acc.class;
        out.push(UsbDevice {
            vendor_id,
            product_id,
            bus: None,
            address: None,
            port: None,
            manufacturer: acc.manufacturer,
            product,
            drivers,
            usability,
            note,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: &str = include_str!("../tests/fixtures/windows-usb-pnp.json");

    fn sys(name: &str, attrs: &[(&str, &str)], drivers: &[&str], ok: Option<bool>) -> SysfsDevice {
        SysfsDevice {
            name: name.into(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            interface_drivers: drivers.iter().map(|d| d.to_string()).collect(),
            node_accessible: ok,
        }
    }

    #[test]
    fn ids() {
        assert_eq!(normalize_id("046D").as_deref(), Some("046d"));
        assert_eq!(normalize_id("0x1d6b").as_deref(), Some("1d6b"));
        assert_eq!(normalize_id("46d"), None);
        assert_eq!(normalize_id("zzzz"), None);
        assert!(valid_port("1.2.3"));
        assert!(!valid_port("1..2"));
        assert!(!valid_port("1-2"));
    }

    #[test]
    fn stable_device_ids() {
        let r = |v: &str, p: &str| UsbPassthrough {
            vendor_id: v.into(),
            product_id: p.into(),
            host_bus: None,
            host_port: None,
            label: None,
        };
        let rules = vec![r("046d", "c52b"), r("1234", "5678"), r("046D", "C52B")];
        assert_eq!(device_id(&rules, 0), "qui-usb-046d-c52b");
        assert_eq!(device_id(&rules, 1), "qui-usb-1234-5678");
        assert_eq!(device_id(&rules, 2), "qui-usb-046d-c52b-1");
        // Removing an unrelated rule keeps the id.
        assert_eq!(device_id(&rules[1..], 0), "qui-usb-1234-5678");
    }

    #[test]
    fn linux_sysfs() {
        let devs = vec![
            sys(
                "usb1",
                &[
                    ("idVendor", "1d6b"),
                    ("idProduct", "0002"),
                    ("bDeviceClass", "09"),
                    ("busnum", "1"),
                    ("devnum", "1"),
                ],
                &["hub"],
                Some(true),
            ),
            sys(
                "1-2",
                &[
                    ("idVendor", "046d"),
                    ("idProduct", "c52b"),
                    ("bDeviceClass", "00"),
                    ("busnum", "1"),
                    ("devnum", "4"),
                    ("manufacturer", "Logitech"),
                    ("product", "USB Receiver"),
                ],
                &["usbhid", "usbhid"],
                Some(true),
            ),
            sys(
                "1-3.1",
                &[
                    ("idVendor", "0781"),
                    ("idProduct", "5581"),
                    ("busnum", "1"),
                    ("devnum", "7"),
                    ("product", "Ultra"),
                ],
                &["usb-storage"],
                Some(false),
            ),
            sys("1-2:1.0", &[("bInterfaceClass", "03")], &[], None),
            sys("2-1", &[("product", "no ids")], &[], None),
        ];
        let out = parse_sysfs(&devs);
        assert_eq!(out.len(), 3, "interfaces and id-less entries skipped");
        let hub = out.iter().find(|d| d.product_id == "0002").unwrap();
        assert_eq!(hub.usability, UsbUsability::Unsupported);
        let rx = out.iter().find(|d| d.vendor_id == "046d").unwrap();
        assert_eq!(
            (rx.bus, rx.address, rx.port.as_deref()),
            (Some(1), Some(4), Some("2"))
        );
        assert_eq!(rx.drivers, ["usbhid"]);
        assert_eq!(rx.usability, UsbUsability::Ready);
        assert_eq!(rx.label(), "Logitech USB Receiver");
        let stick = out.iter().find(|d| d.vendor_id == "0781").unwrap();
        assert_eq!(stick.port.as_deref(), Some("3.1"));
        assert_eq!(stick.usability, UsbUsability::NeedsSetup);
        assert!(stick.note.as_deref().unwrap().contains("udev"));
    }

    #[test]
    fn windows_pnp() {
        let out = parse_windows_pnp(WIN).unwrap();
        assert_eq!(out.len(), 4, "{out:#?}");
        // Composite keyboard: parent usbccgp + two HID interfaces, merged.
        let kb = out.iter().find(|d| d.product_id == "0002").unwrap();
        assert_eq!(kb.drivers, ["usbccgp", "HidUsb"]);
        assert_eq!(kb.usability, UsbUsability::NeedsSetup);
        assert!(
            kb.note.as_deref().unwrap().contains("HidUsb"),
            "names the real driver, not usbccgp"
        );
        // A device already on WinUSB is ready.
        let ready = out.iter().find(|d| d.product_id == "0001").unwrap();
        assert_eq!(ready.usability, UsbUsability::Ready);
        assert_eq!(ready.product.as_deref(), Some("Test Gadget"));
        // Bluetooth radio keeps its own name and manufacturer.
        let bt = out.iter().find(|d| d.product_id == "0003").unwrap();
        assert_eq!(bt.manufacturer.as_deref(), Some("Example Corp"));
        // Hubs are unsupported.
        let hub = out.iter().find(|d| d.product_id == "0004").unwrap();
        assert_eq!(hub.usability, UsbUsability::Unsupported);
        // Windows exposes no bus/port in this query.
        assert!(out.iter().all(|d| d.bus.is_none() && d.port.is_none()));
    }

    #[test]
    fn windows_single_object_and_empty() {
        let one = r#"{"PNPDeviceID":"USB\\VID_1209&PID_0001\\X","Name":"Thing","Manufacturer":"Maker","Service":"WINUSB","PNPClass":"USBDevice","Status":"OK"}"#;
        assert_eq!(parse_windows_pnp(one).unwrap().len(), 1);
        assert!(parse_windows_pnp("").is_err());
        assert!(parse_windows_pnp("null").unwrap().is_empty());
    }

    #[test]
    fn matching_rules() {
        let d = UsbDevice {
            vendor_id: "046d".into(),
            product_id: "c52b".into(),
            bus: Some(1),
            address: Some(4),
            port: Some("2".into()),
            manufacturer: None,
            product: None,
            drivers: vec![],
            usability: UsbUsability::Ready,
            note: None,
        };
        let mut r = UsbPassthrough {
            vendor_id: "046D".into(),
            product_id: "C52B".into(),
            host_bus: None,
            host_port: None,
            label: None,
        };
        assert!(d.matches(&r));
        r.host_port = Some("3".into());
        assert!(!d.matches(&r));
        r.host_port = Some("2".into());
        r.host_bus = Some(1);
        assert!(d.matches(&r));
    }
}
