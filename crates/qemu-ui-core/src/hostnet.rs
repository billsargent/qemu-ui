//! Host side of tap-backed networks (plan.md §10.1, M3-05/M3-06): tap
//! names, the `ip` commands the privileged helper runs on Linux, and the
//! TAP-Windows adapter list on Windows. All pure; the helper and daemon run
//! the commands.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::network::valid_ifname;

/// Tap names the helper creates: `qui-<vm id>-<nic>` (at most 15 bytes,
/// Linux's IFNAMSIZ less the NUL). VM ids are 8 lowercase hex digits.
pub fn tap_name(vm_id: &str, nic: u32) -> Option<String> {
    let ok = vm_id.len() == 8
        && vm_id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    (ok && nic < 64).then(|| format!("qui-{vm_id}-{nic}"))
}

/// Is `name` one the helper could have created (`tap_name`'s shape)?
pub fn is_tap_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("qui-") else {
        return false;
    };
    let Some((vm, nic)) = rest.split_once('-') else {
        return false;
    };
    nic.parse::<u32>()
        .ok()
        .and_then(|n| tap_name(vm, n))
        .is_some_and(|t| t == name)
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Create a persistent tap owned by `owner_uid` (so the unprivileged QEMU
/// can open it), enslave it to `bridge` and bring it up. `multi_queue` must
/// match how QEMU opens it (it does with `queues` > 1).
pub fn create_tap_commands(
    name: &str,
    owner_uid: u32,
    multi_queue: bool,
    bridge: &str,
) -> Vec<Vec<String>> {
    let uid = owner_uid.to_string();
    let mut add = argv(&[
        "ip", "tuntap", "add", "dev", name, "mode", "tap", "user", &uid,
    ]);
    if multi_queue {
        add.push("multi_queue".into());
    }
    vec![
        add,
        argv(&["ip", "link", "set", "dev", name, "master", bridge]),
        argv(&["ip", "link", "set", "dev", name, "up"]),
    ]
}

/// Remove a tap the helper created.
pub fn delete_tap_commands(name: &str) -> Vec<Vec<String>> {
    vec![argv(&["ip", "link", "delete", "dev", name])]
}

/// Create the host-only bridge: no uplink, the host at `address` (CIDR).
pub fn host_only_bridge_commands(bridge: &str, address: &str) -> Vec<Vec<String>> {
    vec![
        argv(&["ip", "link", "add", "name", bridge, "type", "bridge"]),
        argv(&["ip", "addr", "add", address, "dev", bridge]),
        argv(&["ip", "link", "set", "dev", bridge, "up"]),
    ]
}

/// The host's address on a host-only bridge, `a.b.c.d/n` with host bits.
pub fn valid_host_address(s: &str) -> bool {
    let Some((addr, prefix)) = crate::network::parse_ipv4_cidr(s) else {
        return false;
    };
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let host = u32::from(addr) & !mask;
    (8..=30).contains(&prefix)
        && host != 0
        && host != !mask
        && !addr.is_loopback()
        && !addr.is_multicast()
}

/// Bridge names come from helper.toml; check them anyway.
pub fn valid_bridge(name: &str) -> bool {
    valid_ifname(name)
}

// ---------------------------------------------------------------- Windows

/// Lists network adapters as JSON (no elevation needed).
pub const WINDOWS_ADAPTERS_PS: &str = "Get-NetAdapter -IncludeHidden -ErrorAction SilentlyContinue | \
Select-Object Name,InterfaceDescription,MacAddress,@{n='Status';e={\"$($_.Status)\"}} | ConvertTo-Json -Compress";

/// A host network adapter (Windows) as offered for TAP networks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HostNetAdapter {
    /// The name QEMU's `ifname=` takes, e.g. `Ethernet 3`.
    pub name: String,
    pub description: String,
    /// `Up`, `Disconnected`, `Disabled`, …
    pub status: String,
    /// A TAP-Windows (tap-windows6 / OpenVPN TAP) adapter.
    pub is_tap: bool,
}

/// Is this adapter description a TAP-Windows driver?
pub fn is_tap_windows(description: &str) -> bool {
    let d = description.to_ascii_lowercase();
    d.contains("tap-windows") || d.contains("tap-win32") || d.contains("openvpn tap")
}

/// Parse `WINDOWS_ADAPTERS_PS` output (an object or an array of them).
pub fn parse_net_adapters(json: &str) -> Result<Vec<HostNetAdapter>, String> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(vec![]);
    }
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let items = match v {
        serde_json::Value::Array(a) => a,
        o => vec![o],
    };
    let mut out: Vec<HostNetAdapter> = items
        .iter()
        .filter_map(|o| {
            let name = o["Name"].as_str()?.trim().to_string();
            let description = o["InterfaceDescription"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            Some(HostNetAdapter {
                is_tap: is_tap_windows(&description),
                status: o["Status"].as_str().unwrap_or_default().to_string(),
                name,
                description,
            })
        })
        .filter(|a| !a.name.is_empty())
        .collect();
    out.sort_by(|a, b| b.is_tap.cmp(&a.is_tap).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// What the host offers for tap-backed networks (`GET /host/net-adapters`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct HostNetInfo {
    /// `linux`, `windows`, …
    pub os: String,
    /// Windows: adapters, TAP-Windows ones first.
    pub adapters: Vec<HostNetAdapter>,
    /// Linux: the helper is reachable (bridged/host-only need it).
    pub helper: bool,
    /// Linux: bridges the helper's configuration allows.
    pub bridges: Vec<String>,
    /// Linux: the helper offers a host-only bridge.
    pub host_only: bool,
    /// Why tap networks are unavailable or need setup, if they do.
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_names() {
        assert_eq!(tap_name("0a1b2c3d", 0).as_deref(), Some("qui-0a1b2c3d-0"));
        let longest = tap_name("ffffffff", 63).unwrap();
        assert!(longest.len() <= 15 && valid_ifname(&longest));
        assert!(tap_name("0A1B2C3D", 0).is_none(), "lowercase ids only");
        assert!(tap_name("../x", 0).is_none());
        assert!(tap_name("0a1b2c3d", 64).is_none());
        assert!(is_tap_name("qui-0a1b2c3d-7"));
        for bad in [
            "eth0",
            "qui-0a1b2c3d",
            "qui-0a1b2c3d-07",
            "qui-zzzzzzzz-1",
            "qui-0a1b2c3d-1x",
        ] {
            assert!(!is_tap_name(bad), "{bad}");
        }
    }

    #[test]
    fn commands() {
        let c = create_tap_commands("qui-0a1b2c3d-0", 1000, false, "br0");
        assert_eq!(
            c[0].join(" "),
            "ip tuntap add dev qui-0a1b2c3d-0 mode tap user 1000"
        );
        assert_eq!(c[1].join(" "), "ip link set dev qui-0a1b2c3d-0 master br0");
        assert_eq!(c[2].join(" "), "ip link set dev qui-0a1b2c3d-0 up");
        let mq = create_tap_commands("qui-0a1b2c3d-0", 1000, true, "br0");
        assert_eq!(mq[0].last().unwrap(), "multi_queue");
        assert_eq!(
            delete_tap_commands("t")[0].join(" "),
            "ip link delete dev t"
        );
        let b = host_only_bridge_commands("qui-hostonly", "10.213.0.1/24");
        assert_eq!(b[1].join(" "), "ip addr add 10.213.0.1/24 dev qui-hostonly");
    }

    #[test]
    fn host_addresses() {
        assert!(valid_host_address("10.213.0.1/24"));
        assert!(!valid_host_address("10.213.0.0/24"), "network address");
        assert!(!valid_host_address("10.213.0.255/24"), "broadcast");
        assert!(!valid_host_address("127.0.0.1/8"));
        assert!(!valid_host_address("10.0.0.1"));
    }

    // Synthetic, shaped like `Get-NetAdapter | ConvertTo-Json` output.
    const ADAPTERS: &str = r#"[
      {"Name":"Ethernet","InterfaceDescription":"Example Gigabit Network Connection","MacAddress":"00-11-22-33-44-55","Status":"Up"},
      {"Name":"Ethernet 3","InterfaceDescription":"TAP-Windows Adapter V9","MacAddress":"00-FF-AA-BB-CC-DD","Status":"Disconnected"},
      {"Name":"Wi-Fi","InterfaceDescription":"Example Wireless Adapter","MacAddress":"00-11-22-33-44-66","Status":"Disabled"}
    ]"#;

    #[test]
    fn adapters() {
        let a = parse_net_adapters(ADAPTERS).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].name, "Ethernet 3", "TAP adapters first");
        assert!(a[0].is_tap && !a[1].is_tap);
        let one = parse_net_adapters(
            r#"{"Name":"OpenVPN TAP","InterfaceDescription":"TAP-Win32 Adapter OAS","Status":"Up"}"#,
        )
        .unwrap();
        assert!(one[0].is_tap);
        assert!(parse_net_adapters("").unwrap().is_empty());
        assert!(parse_net_adapters("nope").is_err());
    }
}
