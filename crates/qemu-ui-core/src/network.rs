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
}

impl NetworkConfig {
    /// Short label for lists.
    pub fn kind_label(&self) -> &'static str {
        match self {
            NetworkConfig::Nat { .. } => "NAT",
        }
    }

    /// Do NICs on this network take port forwards?
    pub fn supports_forwards(&self) -> bool {
        match self {
            NetworkConfig::Nat { .. } => true,
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
        NetworkConfig::Nat { subnet: None } => {}
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
