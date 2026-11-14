//! Privileged-helper protocol and the pure parts of disk handling
//! (docs/design/privileged-helper.md, M2-10/M2-11).
//!
//! Requests are a closed enum: there is deliberately no way to ask the helper
//! to run a command or open an arbitrary path.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Protocol version; bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        secret: Option<String>,
    },
    ListDisks,
    ExportDisk {
        disk_id: String,
        writable: bool,
    },
    Unexport {
        export_id: String,
    },
    ListExports,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope<T> {
    pub id: u64,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Hello { version: u32 },
    Disks { disks: Vec<HostDisk> },
    Exported { export: DiskExport },
    Unexported,
    Exports { exports: Vec<DiskExport> },
    Error { message: String },
}

/// A host disk as the helper sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HostDisk {
    /// Stable id: Linux `/dev/disk/by-id` name, Windows `UniqueId`.
    pub id: String,
    /// Human label: model / friendly name.
    pub label: String,
    #[ts(type = "number")]
    pub size_bytes: u64,
    /// `usb`, `nvme`, `sata`, `iscsi`, …
    pub bus: String,
    pub removable: bool,
    /// Holds the running OS (root, boot or system partition).
    pub system: bool,
    pub read_only: bool,
    /// Mounted partitions (Linux) or online volumes with access paths (Windows).
    pub in_use: Vec<String>,
    /// Windows: the disk is offline (its volumes are not mounted).
    pub offline: bool,
    /// Device path the helper opens (never supplied by the daemon).
    pub device_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct DiskExport {
    pub export_id: String,
    pub disk_id: String,
    pub writable: bool,
    /// AF_UNIX socket of the NBD server.
    pub socket: String,
    /// NBD export name.
    pub export_name: String,
}

/// Administrator-owned helper configuration (`helper.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelperConfig {
    /// Linux: only this user may connect.
    #[serde(default)]
    pub daemon_user: Option<String>,
    #[serde(default)]
    pub disks: DiskPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskPolicy {
    /// Allow any removable disk (USB sticks, card readers).
    #[serde(default)]
    pub allow_removable: bool,
    /// Allow writable exports at all (default: read-only only).
    #[serde(default)]
    pub allow_writable: bool,
    /// Explicitly allowed disk ids.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// Why a disk may not be exported, or `Ok` if it may.
pub fn check_export(
    disk: &HostDisk,
    policy: &DiskPolicy,
    writable: bool,
    windows: bool,
) -> Result<(), String> {
    // Never configurable: the running OS's own disk.
    if disk.system {
        return Err(format!(
            "'{}' holds the running operating system and can never be attached",
            disk.label
        ));
    }
    if !(policy.allow.iter().any(|a| a == &disk.id) || (policy.allow_removable && disk.removable)) {
        return Err(format!(
            "'{}' is not in the helper's allowlist; an administrator must add its id ({}) to helper.toml",
            disk.label, disk.id
        ));
    }
    if writable && !policy.allow_writable {
        return Err("writable disk exports are disabled in helper.toml (allow_writable)".into());
    }
    if writable && disk.read_only {
        return Err(format!("'{}' is read-only", disk.label));
    }
    if windows {
        if !disk.offline {
            return Err(format!(
                "'{}' is online; Windows locks online volumes. Take it offline first (elevated PowerShell: \
                 Set-Disk -UniqueId '{}' -IsOffline $true)",
                disk.label, disk.id
            ));
        }
    } else if !disk.in_use.is_empty() {
        return Err(format!(
            "'{}' is in use ({}); unmount it first",
            disk.label,
            disk.in_use.join(", ")
        ));
    }
    Ok(())
}

// --------------------------------------------------------------- parsers

/// Parse Windows `Get-Disk | ConvertTo-Json` plus `Get-Partition` output.
/// `ConvertTo-Json` emits an object instead of an array for a single item.
pub fn parse_windows_disks(
    disks_json: &str,
    partitions_json: &str,
) -> Result<Vec<HostDisk>, String> {
    fn items(s: &str) -> Result<Vec<serde_json::Value>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(vec![]);
        }
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        Ok(match v {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        })
    }
    let parts = items(partitions_json)?;
    let mut out = Vec::new();
    for d in items(disks_json)? {
        let number = d["Number"].as_u64().ok_or("disk without Number")?;
        let bus = d["BusType"]
            .as_str()
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        let in_use: Vec<String> = parts
            .iter()
            .filter(|p| p["DiskNumber"].as_u64() == Some(number))
            .filter_map(|p| {
                let letter = p["DriveLetter"]
                    .as_str()
                    .filter(|l| !l.trim().is_empty() && *l != "\u{0}");
                letter.map(|l| format!("{l}:"))
            })
            .collect();
        out.push(HostDisk {
            id: d["UniqueId"].as_str().unwrap_or_default().to_string(),
            label: d["FriendlyName"]
                .as_str()
                .unwrap_or("disk")
                .trim()
                .to_string(),
            size_bytes: d["Size"].as_u64().unwrap_or(0),
            removable: matches!(bus.as_str(), "usb" | "sd" | "mmc"),
            bus,
            system: d["IsSystem"].as_bool().unwrap_or(false)
                || d["IsBoot"].as_bool().unwrap_or(false),
            read_only: d["IsReadOnly"].as_bool().unwrap_or(false),
            in_use,
            offline: d["IsOffline"].as_bool().unwrap_or(false),
            device_path: format!(r"\\.\PhysicalDrive{number}"),
        });
    }
    out.retain(|d| !d.id.is_empty());
    Ok(out)
}

/// Parse `lsblk --json --bytes -o NAME,PATH,SIZE,TYPE,TRAN,RM,RO,MODEL,MOUNTPOINTS`
/// and map disks to their `/dev/disk/by-id` names (`by_id`: (link name, target path)).
pub fn parse_lsblk(json: &str, by_id: &[(String, String)]) -> Result<Vec<HostDisk>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    fn mounts(dev: &serde_json::Value, acc: &mut Vec<String>) {
        if let Some(m) = dev["mountpoints"].as_array() {
            acc.extend(
                m.iter()
                    .filter_map(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
        }
        if let Some(m) = dev["mountpoint"].as_str() {
            acc.push(m.to_string());
        }
        for c in dev["children"].as_array().into_iter().flatten() {
            mounts(c, acc);
        }
    }
    let truthy = |x: &serde_json::Value| {
        x.as_bool().unwrap_or(false) || x.as_str() == Some("1") || x.as_u64() == Some(1)
    };
    let mut out = Vec::new();
    for dev in v["blockdevices"].as_array().into_iter().flatten() {
        if dev["type"].as_str() != Some("disk") {
            continue;
        }
        let path = dev["path"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("/dev/{}", dev["name"].as_str().unwrap_or_default()));
        // Prefer a wwn-/nvme-eui ids? Any stable id works; take the shortest
        // non-partition link for readability, preferring usb-/ata-/nvme- names.
        let mut ids: Vec<&String> = by_id
            .iter()
            .filter(|(name, target)| target == &path && !name.contains("-part"))
            .map(|(name, _)| name)
            .collect();
        ids.sort_by_key(|n| (n.starts_with("wwn-") || n.contains("eui."), n.len()));
        let Some(id) = ids.first().map(|s| s.to_string()) else {
            continue;
        };
        let mut in_use = Vec::new();
        mounts(dev, &mut in_use);
        let system = in_use
            .iter()
            .any(|m| matches!(m.as_str(), "/" | "/boot" | "/boot/efi" | "/usr" | "[SWAP]"));
        out.push(HostDisk {
            label: dev["model"].as_str().unwrap_or("disk").trim().to_string(),
            size_bytes: dev["size"].as_u64().unwrap_or(0),
            bus: dev["tran"].as_str().unwrap_or("unknown").to_string(),
            removable: truthy(&dev["rm"]),
            system,
            read_only: truthy(&dev["ro"]),
            in_use,
            offline: false,
            device_path: format!("/dev/disk/by-id/{id}"),
            id,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fixtures shaped like real tool output (no real ids or serials).
    const WIN_DISKS: &str = r#"[
      {"Number":0,"UniqueId":"eui.0000AAAA0000BBBB","FriendlyName":"Example NVMe SSD","BusType":"NVMe",
       "IsSystem":true,"IsBoot":true,"IsOffline":false,"IsReadOnly":false,"Size":500107862016},
      {"Number":2,"UniqueId":"USBSTOR\\DISK&VEN_EXAMPLE&PROD_STICK\\0001","FriendlyName":"Example USB Stick",
       "BusType":"USB","IsSystem":false,"IsBoot":false,"IsOffline":false,"IsReadOnly":false,"Size":16008609792},
      {"Number":3,"UniqueId":"6002248000000000000000000000AAAA","FriendlyName":"Spare SATA","BusType":"SATA",
       "IsSystem":false,"IsBoot":false,"IsOffline":true,"IsReadOnly":false,"Size":1000204886016}
    ]"#;
    const WIN_PARTS: &str = r#"[
      {"DiskNumber":0,"PartitionNumber":3,"DriveLetter":"C"},
      {"DiskNumber":2,"PartitionNumber":1,"DriveLetter":"E"},
      {"DiskNumber":3,"PartitionNumber":1,"DriveLetter":"\u0000"}
    ]"#;

    const LSBLK: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","path":"/dev/nvme0n1","size":512110190592,"type":"disk","tran":"nvme","rm":false,"ro":false,
       "model":"Example NVMe","mountpoints":[null],
       "children":[{"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part","mountpoints":["/boot/efi"]},
                   {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part","mountpoints":["/"]}]},
      {"name":"sdb","path":"/dev/sdb","size":16008609792,"type":"disk","tran":"usb","rm":true,"ro":false,
       "model":"Example Stick","mountpoints":[null],
       "children":[{"name":"sdb1","path":"/dev/sdb1","type":"part","mountpoints":["/media/stick"]}]},
      {"name":"sdc","path":"/dev/sdc","size":1000204886016,"type":"disk","tran":"sata","rm":false,"ro":false,
       "model":"Spare SATA","mountpoints":[null]},
      {"name":"loop0","path":"/dev/loop0","size":1,"type":"loop","mountpoints":["/snap/x"]}
    ]}"#;

    fn by_id() -> Vec<(String, String)> {
        [
            ("nvme-Example_NVMe_1234", "/dev/nvme0n1"),
            ("nvme-eui.0000aaaa", "/dev/nvme0n1"),
            ("nvme-Example_NVMe_1234-part2", "/dev/nvme0n1p2"),
            ("usb-Example_Stick_0001-0:0", "/dev/sdb"),
            ("ata-Spare_SATA_XYZ", "/dev/sdc"),
            ("wwn-0x5000c500aaaa", "/dev/sdc"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
    }

    #[test]
    fn windows_parsing() {
        let d = parse_windows_disks(WIN_DISKS, WIN_PARTS).unwrap();
        assert_eq!(d.len(), 3);
        assert!(d[0].system);
        assert_eq!(d[0].device_path, r"\\.\PhysicalDrive0");
        assert!(d[1].removable && !d[1].offline);
        assert_eq!(d[1].in_use, ["E:"]);
        assert!(
            d[2].offline && d[2].in_use.is_empty(),
            "NUL drive letter is no letter"
        );
        // A single disk serialises as an object, not an array.
        let one = parse_windows_disks(
            r#"{"Number":5,"UniqueId":"X","FriendlyName":"Solo","BusType":"USB","Size":1}"#,
            "",
        )
        .unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn lsblk_parsing() {
        let d = parse_lsblk(LSBLK, &by_id()).unwrap();
        assert_eq!(d.len(), 3, "loop devices are skipped");
        assert!(d[0].system, "root is mounted from it");
        assert_eq!(
            d[0].id, "nvme-Example_NVMe_1234",
            "readable id preferred over eui"
        );
        assert!(d[1].removable);
        assert_eq!(d[1].in_use, ["/media/stick"]);
        assert_eq!(d[2].id, "ata-Spare_SATA_XYZ");
        assert_eq!(d[2].device_path, "/dev/disk/by-id/ata-Spare_SATA_XYZ");
    }

    #[test]
    fn export_policy() {
        let win = parse_windows_disks(WIN_DISKS, WIN_PARTS).unwrap();
        let lin = parse_lsblk(LSBLK, &by_id()).unwrap();
        let none = DiskPolicy::default();
        let removable = DiskPolicy {
            allow_removable: true,
            ..Default::default()
        };
        let spare = DiskPolicy {
            allow: vec![win[2].id.clone(), lin[2].id.clone()],
            ..Default::default()
        };

        // Nothing by default; the system disk never, even if listed.
        assert!(
            check_export(&win[2], &none, false, true)
                .unwrap_err()
                .contains("allowlist")
        );
        let sys = DiskPolicy {
            allow: vec![win[0].id.clone()],
            allow_removable: true,
            allow_writable: true,
        };
        assert!(
            check_export(&win[0], &sys, false, true)
                .unwrap_err()
                .contains("operating system")
        );

        // Windows: online removable stick refused until offline; offline spare allowed read-only.
        assert!(
            check_export(&win[1], &removable, false, true)
                .unwrap_err()
                .contains("Set-Disk")
        );
        assert!(check_export(&win[2], &spare, false, true).is_ok());
        assert!(
            check_export(&win[2], &spare, true, true)
                .unwrap_err()
                .contains("allow_writable")
        );

        // Linux: mounted stick refused; unmounted spare allowed.
        assert!(
            check_export(&lin[1], &removable, false, false)
                .unwrap_err()
                .contains("unmount")
        );
        assert!(check_export(&lin[2], &spare, false, false).is_ok());
    }

    #[test]
    fn protocol_is_tagged_json() {
        let r = Envelope {
            id: 7,
            body: Request::ExportDisk {
                disk_id: "x".into(),
                writable: false,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"id":7,"op":"export_disk","disk_id":"x","writable":false}"#
        );
        let back: Envelope<Request> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
        // Unknown operations are rejected by deserialisation.
        assert!(
            serde_json::from_str::<Envelope<Request>>(r#"{"id":1,"op":"run","cmd":"sh"}"#).is_err()
        );
    }
}
