//! Network objects (plan.md §10.1, M3-01).
//!
//! A network is a named, reusable definition that NICs attach to by id
//! (`NicBackend::Network`). The spec never holds host interface names or
//! socket paths; the daemon resolves those when it builds the command line.
//! Kinds arrive milestone by milestone; each says which hosts can run it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct Network {
    /// Stable id; NICs reference it.
    pub id: String,
    /// Unique display name.
    pub name: String,
    pub config: NetworkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum NetworkConfig {
    /// User-mode NAT (slirp). Every attached VM gets its own private NAT
    /// with this addressing; guests on it cannot see each other.
    Nat {
        /// IPv4 subnet in CIDR form; `None` uses QEMU's 10.0.2.0/24.
        #[serde(default)]
        subnet: Option<String>,
    },
    /// Linux: a tap per NIC, attached by the privileged helper to an
    /// existing host bridge the administrator allowlisted (M3-05). Guests
    /// are on the bridge's LAN and reachable directly.
    Bridged {
        /// Host bridge interface, e.g. `br0`.
        bridge: String,
    },
    /// Linux: a tap per NIC on a bridge the helper manages, with no uplink.
    /// The host has an address on it (set in helper.toml); guests reach the
    /// host and each other, not the outside world (M3-05).
    HostOnly {},
    /// Windows: a TAP-Windows adapter, bridged to the LAN (or not) in
    /// Network Connections by an administrator (M3-06).
    Tap {
        /// The adapter's name (`Get-NetAdapter` Name), e.g. `Ethernet 3`.
        adapter: String,
    },
}

impl NetworkConfig {
    /// Short label for lists.
    pub fn kind_label(&self) -> &'static str {
        match self {
            NetworkConfig::Nat { .. } => "NAT",
            NetworkConfig::Bridged { .. } => "Bridged",
            NetworkConfig::HostOnly {} => "Host-only",
            NetworkConfig::Tap { .. } => "TAP",
        }
    }

    /// Backed by a host tap device: guests are reachable directly and
    /// virtio-net can use several queues (Linux taps only).
    pub fn is_tap(&self) -> bool {
        matches!(
            self,
            NetworkConfig::Bridged { .. } | NetworkConfig::HostOnly {} | NetworkConfig::Tap { .. }
        )
    }

    /// Does the backend give a NIC more than one queue? Linux taps do
    /// (multi_queue); TAP-Windows and slirp have a single queue.
    pub fn supports_multiqueue(&self) -> bool {
        matches!(
            self,
            NetworkConfig::Bridged { .. } | NetworkConfig::HostOnly {}
        )
    }

    /// Why this kind cannot run on host `os` (`linux`, `windows`, …).
    pub fn unsupported_on(&self, os: &str) -> Option<String> {
        match self {
            NetworkConfig::Bridged { .. } | NetworkConfig::HostOnly {} if os != "linux" => Some(format!(
                "{} networks need a Linux host (tap devices and bridges through the qemu-ui helper);                  on Windows use a TAP network",
                self.kind_label()
            )),
            NetworkConfig::Tap { .. } if os != "windows" => Some(
                "TAP networks use a TAP-Windows adapter and need a Windows host; on Linux use a                  bridged or host-only network"
                    .into(),
            ),
            _ => None,
        }
    }

    /// Do NICs on this network take port forwards?
    pub fn supports_forwards(&self) -> bool {
        match self {
            NetworkConfig::Nat { .. } => true,
            NetworkConfig::Bridged { .. }
            | NetworkConfig::HostOnly {}
            | NetworkConfig::Tap { .. } => false,
        }
    }
}

/// A network as listed by the API: the definition plus the VMs using it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[ts(export)]
pub struct NetworkInfo {
    #[serde(flatten)]
    pub network: Network,
    /// Names of VMs with a NIC attached.
    pub used_by: Vec<String>,
}

/// Parse `a.b.c.d/n` (IPv4). Returns the network address and prefix.
pub fn parse_ipv4_cidr(s: &str) -> Option<(std::net::Ipv4Addr, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let addr: std::net::Ipv4Addr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    (prefix <= 32).then_some((addr, prefix))
}

/// A Linux interface name: 1-15 characters, no `/`, `:`, whitespace or
/// control characters, and not `.` or `..`.
pub fn valid_ifname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == ':' || c.is_whitespace() || c.is_control() || !c.is_ascii())
}

/// A Windows adapter name: printable, up to 256 characters.
pub fn valid_adapter_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 256
        && name.trim() == name
        && !name.chars().any(|c| c.is_control() || c == '"')
}

/// Network names: like VM names, short and printable.
pub fn valid_network_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= 64
        && name.trim() == name
        && !name.chars().any(|c| c.is_control())
}

/// Check a network definition; the message is shown to the user.
pub fn validate_network(n: &Network) -> Result<(), String> {
    if !valid_network_name(&n.name) {
        return Err(
            "network names are 1-64 printable characters without surrounding spaces".into(),
        );
    }
    match &n.config {
        NetworkConfig::Nat { subnet: Some(s) } => {
            let (addr, prefix) = parse_ipv4_cidr(s)
                .ok_or_else(|| format!("'{s}' is not an IPv4 subnet like 10.0.2.0/24"))?;
            // slirp puts the gateway at .2, DNS at .3 and the guest from .15.
            if !(8..=28).contains(&prefix) {
                return Err("the subnet prefix must be between /8 and /28 (NAT needs room for its gateway, DNS and DHCP range)".into());
            }
            let mask = u32::MAX << (32 - prefix);
            if u32::from(addr) & !mask != 0 {
                return Err(format!(
                    "'{s}' has host bits set; did you mean {}/{prefix}?",
                    std::net::Ipv4Addr::from(u32::from(addr) & mask)
                ));
            }
            if addr.is_loopback()
                || addr.is_multicast()
                || addr.is_unspecified()
                || addr.is_broadcast()
            {
                return Err(format!("'{s}' is not a usable private subnet"));
            }
        }
        NetworkConfig::Nat { subnet: None } | NetworkConfig::HostOnly {} => {}
        NetworkConfig::Bridged { bridge } => {
            if !valid_ifname(bridge) {
                return Err(format!(
                    "'{bridge}' is not a Linux interface name (1-15 characters, no spaces, '/' or ':')"
                ));
            }
        }
        NetworkConfig::Tap { adapter } => {
            if !valid_adapter_name(adapter) {
                return Err(format!("'{adapter}' is not a valid adapter name"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nat(subnet: Option<&str>) -> Network {
        Network {
            id: "n1".into(),
            name: "lab".into(),
            config: NetworkConfig::Nat {
                subnet: subnet.map(String::from),
            },
        }
    }

    #[test]
    fn subnets() {
        assert!(validate_network(&nat(None)).is_ok());
        assert!(validate_network(&nat(Some("10.5.0.0/24"))).is_ok());
        assert!(
            validate_network(&nat(Some("192.168.77.0/16")))
                .unwrap_err()
                .contains("host bits")
        );
        assert!(
            validate_network(&nat(Some("10.0.0.0/30"))).is_err(),
            "too small"
        );
        assert!(validate_network(&nat(Some("127.0.0.0/8"))).is_err());
        assert!(validate_network(&nat(Some("nonsense"))).is_err());
        assert!(validate_network(&nat(Some("10.0.0.0/33"))).is_err());
    }

    #[test]
    fn tap_kinds() {
        let mk = |config| Network {
            id: "n".into(),
            name: "x".into(),
            config,
        };
        assert!(
            validate_network(&mk(NetworkConfig::Bridged {
                bridge: "br0".into()
            }))
            .is_ok()
        );
        for bad in ["", "a/b", "br 0", "eth0:1", "way-too-long-bridge0"] {
            assert!(
                validate_network(&mk(NetworkConfig::Bridged { bridge: bad.into() })).is_err(),
                "{bad}"
            );
        }
        assert!(validate_network(&mk(NetworkConfig::HostOnly {})).is_ok());
        assert!(
            validate_network(&mk(NetworkConfig::Tap {
                adapter: "Ethernet 3".into()
            }))
            .is_ok()
        );
        assert!(
            validate_network(&mk(NetworkConfig::Tap {
                adapter: " x".into()
            }))
            .is_err()
        );

        let br = NetworkConfig::Bridged {
            bridge: "br0".into(),
        };
        let tap = NetworkConfig::Tap {
            adapter: "T".into(),
        };
        assert!(br.unsupported_on("linux").is_none() && br.unsupported_on("windows").is_some());
        assert!(tap.unsupported_on("windows").is_none() && tap.unsupported_on("linux").is_some());
        assert!(br.is_tap() && br.supports_multiqueue() && !br.supports_forwards());
        assert!(tap.is_tap() && !tap.supports_multiqueue());
        let v = serde_json::to_value(NetworkConfig::HostOnly {}).unwrap();
        assert_eq!(v, serde_json::json!({"kind": "host-only"}));
    }

    #[test]
    fn names() {
        let mut n = nat(None);
        n.name = " lab".into();
        assert!(validate_network(&n).is_err());
        n.name = String::new();
        assert!(validate_network(&n).is_err());
    }

    #[test]
    fn json_shape() {
        let v = serde_json::to_value(nat(Some("10.5.0.0/24"))).unwrap();
        assert_eq!(v["config"]["kind"], "nat");
        assert_eq!(v["config"]["subnet"], "10.5.0.0/24");
    }
}
