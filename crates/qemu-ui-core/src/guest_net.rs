//! Guest network addresses (M3-08): pure parsing for every source the
//! daemon consults, and the rule that picks the best answer per NIC.
//!
//! Sources, best first:
//! 1. **Guest agent** — `guest-network-get-interfaces`, matched by MAC.
//! 2. **slirp NAT** — the guest address is deterministic on QEMU's default
//!    user network; `info usernet` (HMP) confirms it when the guest has
//!    traffic of its own. Only reachable from the host through forwards.
//! 3. **Host neighbour table** — for bridged/host-only NICs (M3-05): the
//!    host's ARP/NDP cache looked up by the NIC's MAC.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The guest address QEMU's user network hands out first (DHCP start).
pub const SLIRP_GUEST_IP: &str = "10.0.2.15";

/// Where an address came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum AddrSource {
    GuestAgent,
    Nat,
    Neighbor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct GuestAddr {
    pub ip: String,
    pub source: AddrSource,
    /// Observed (agent, traffic, neighbour entry) rather than inferred.
    pub confirmed: bool,
    /// Can the host connect to this address directly? NAT addresses are
    /// only reachable through port forwards.
    pub reachable_from_host: bool,
    /// One line for the UI, e.g. "NAT (internal)".
    pub label: String,
}

/// A port forward QEMU reports as active (`info usernet`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ActiveForward {
    pub proto: String,
    pub host_addr: String,
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct GuestNic {
    /// Index into the spec's `nics`.
    pub nic: usize,
    pub mac: Option<String>,
    /// Best source first.
    pub addresses: Vec<GuestAddr>,
    /// Forwards the running QEMU actually has (NAT NICs only).
    pub active_forwards: Vec<ActiveForward>,
}

/// `GET /vms/{id}/guest-net`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct GuestNetInfo {
    pub running: bool,
    /// The VM has a guest-agent channel (`qui-ga0`).
    pub agent_channel: bool,
    /// Why the agent gave no answer, when it has a channel.
    pub agent_error: Option<String>,
    pub nics: Vec<GuestNic>,
}

// ----------------------------------------------------------- info usernet

/// One socket from `info usernet`.
#[derive(Debug, Clone, PartialEq)]
pub struct UsernetConn {
    /// `TCP`, `UDP`, `ICMP`.
    pub proto: String,
    /// `HOST_FORWARD`, `ESTABLISHED`, `236 sec`, …
    pub state: String,
    pub src: String,
    pub sport: u16,
    pub dst: String,
    pub dport: u16,
}

/// One user-network stack from `info usernet`.
#[derive(Debug, Clone, PartialEq)]
pub struct UsernetHub {
    /// The netdev id, e.g. `qui-net0`.
    pub netdev: String,
    pub conns: Vec<UsernetConn>,
}

/// Parse HMP `info usernet`:
///
/// ```text
/// Hub -1 (qui-net0):
///   Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
///   TCP[HOST_FORWARD]  13       127.0.0.1  2222       10.0.2.15    22     0     0
///   UDP[236 sec]       26       10.0.2.15    68 255.255.255.255    67     0     0
/// ```
///
/// For host forwards the destination is the guest; for everything else
/// the source is (libslirp prints the guest side as "local").
pub fn parse_usernet(text: &str) -> Vec<UsernetHub> {
    let mut hubs: Vec<UsernetHub> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Hub ").or_else(|| t.strip_prefix("VLAN ")) {
            if let (Some(a), Some(b)) = (rest.find('('), rest.rfind(')'))
                && a < b
            {
                hubs.push(UsernetHub {
                    netdev: rest[a + 1..b].to_string(),
                    conns: vec![],
                });
            }
            continue;
        }
        let Some(hub) = hubs.last_mut() else { continue };
        let (Some(open), Some(close)) = (t.find('['), t.find(']')) else {
            continue;
        };
        if open == 0 || close < open || t.starts_with("Protocol") {
            continue;
        }
        let f: Vec<&str> = t[close + 1..].split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let (Ok(sport), Ok(dport)) = (f[2].parse(), f[4].parse()) else {
            continue;
        };
        hub.conns.push(UsernetConn {
            proto: t[..open].to_string(),
            state: t[open + 1..close].to_string(),
            src: f[1].to_string(),
            sport,
            dst: f[3].to_string(),
            dport,
        });
    }
    hubs
}

impl UsernetHub {
    /// Host forwards QEMU has set up for this netdev.
    pub fn forwards(&self) -> Vec<ActiveForward> {
        self.conns
            .iter()
            .filter(|c| c.state == "HOST_FORWARD")
            .map(|c| ActiveForward {
                proto: c.proto.to_ascii_lowercase(),
                host_addr: c.src.clone(),
                host_port: c.sport,
                guest_port: c.dport,
            })
            .collect()
    }

    /// The guest's NAT address on QEMU's default 10.0.2.0/24.
    pub fn guest_addr(&self) -> GuestAddr {
        self.guest_addr_in(None)
    }

    /// The guest's NAT address: observed from guest-originated traffic when
    /// there is any (confirmed), else the forward target or the DHCP default.
    /// `subnet` is a NAT network's `a.b.c.d/n` (`None`: QEMU's default);
    /// slirp puts its gateway at .2, DNS at .3 and the first lease at .15.
    pub fn guest_addr_in(&self, subnet: Option<&str>) -> GuestAddr {
        let net = subnet.and_then(crate::network::parse_ipv4_cidr);
        let (base, prefix) = net.map_or(
            (u32::from(std::net::Ipv4Addr::new(10, 0, 2, 0)), 24),
            |(a, p)| (u32::from(a), p),
        );
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        let at = |n: u32| std::net::Ipv4Addr::from(base + n).to_string();
        let (gateway, dns, default_guest) = (at(2), at(3), at(15));
        let is_guest = |ip: &str| {
            ip.parse::<std::net::Ipv4Addr>()
                .is_ok_and(|a| u32::from(a) & mask == base && ip != gateway && ip != dns)
        };
        let observed = self
            .conns
            .iter()
            .find(|c| c.state != "HOST_FORWARD" && is_guest(&c.src))
            .map(|c| c.src.clone());
        let (ip, confirmed) = match observed {
            Some(ip) => (ip, true),
            None => (
                self.conns
                    .iter()
                    .find(|c| c.state == "HOST_FORWARD" && is_guest(&c.dst))
                    .map(|c| c.dst.clone())
                    .unwrap_or(default_guest),
                false,
            ),
        };
        nat_addr(ip, confirmed)
    }
}

/// A NAT address entry.
pub fn nat_addr(ip: String, confirmed: bool) -> GuestAddr {
    GuestAddr {
        ip,
        source: AddrSource::Nat,
        confirmed,
        reachable_from_host: false,
        label: if confirmed {
            "NAT (internal) — reach it through port forwards".into()
        } else {
            "NAT (internal, expected) — reach it through port forwards".into()
        },
    }
}

// ------------------------------------------------------------- guest agent

/// Addresses per MAC from qemu-guest-agent's `guest-network-get-interfaces`
/// reply (the `return` value). Loopback and link-local addresses are skipped.
pub fn parse_agent_interfaces(v: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for iface in v.as_array().into_iter().flatten() {
        let Some(mac) = iface["hardware-address"].as_str().and_then(normalize_mac) else {
            continue;
        };
        if mac == "00:00:00:00:00:00" {
            continue;
        }
        let ips: Vec<String> = iface["ip-addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["ip-address"].as_str())
            .filter(|ip| useful_ip(ip))
            .map(String::from)
            .collect();
        out.push((mac, ips));
    }
    out
}

/// Not loopback, link-local or unspecified.
fn useful_ip(ip: &str) -> bool {
    match ip
        .split('%')
        .next()
        .unwrap_or(ip)
        .parse::<std::net::IpAddr>()
    {
        Ok(std::net::IpAddr::V4(a)) => {
            !(a.is_loopback() || a.is_link_local() || a.is_unspecified())
        }
        Ok(std::net::IpAddr::V6(a)) => {
            !(a.is_loopback() || a.is_unspecified() || (a.segments()[0] & 0xffc0) == 0xfe80)
        }
        Err(_) => false,
    }
}

pub fn agent_addr(ip: String) -> GuestAddr {
    GuestAddr {
        ip,
        source: AddrSource::GuestAgent,
        confirmed: true,
        reachable_from_host: false,
        label: "reported by the guest agent".into(),
    }
}

// ----------------------------------------------------- host neighbour table

/// The command that lists the host's neighbour table on Linux.
pub const LINUX_NEIGH_CMD: &[&str] = &["ip", "neigh", "show"];

/// The PowerShell script that lists the neighbour table on Windows as JSON.
pub const WINDOWS_NEIGH_PS: &str = "Get-NetNeighbor -ErrorAction SilentlyContinue | \
Select-Object IPAddress,LinkLayerAddress,@{n='State';e={\"$($_.State)\"}} | ConvertTo-Json -Compress";

#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub ip: String,
    /// Normalised `aa:bb:cc:dd:ee:ff`.
    pub mac: String,
    pub state: String,
}

/// `ip neigh show` lines: `192.0.2.10 dev br0 lladdr 52:54:00:12:34:56 REACHABLE`.
pub fn parse_ip_neigh(text: &str) -> Vec<Neighbor> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(ip) = f.first() else { continue };
        let Some(i) = f.iter().position(|w| *w == "lladdr") else {
            continue;
        };
        let Some(mac) = f.get(i + 1).and_then(|m| normalize_mac(m)) else {
            continue;
        };
        out.push(Neighbor {
            ip: ip.to_string(),
            mac,
            state: f.last().unwrap_or(&"").to_string(),
        });
    }
    out
}

/// `Get-NetNeighbor … | ConvertTo-Json` (an object, or an array of them).
pub fn parse_net_neighbor_json(text: &str) -> Vec<Neighbor> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return vec![];
    };
    let items = match v {
        serde_json::Value::Array(a) => a,
        o @ serde_json::Value::Object(_) => vec![o],
        _ => vec![],
    };
    items
        .iter()
        .filter_map(|o| {
            Some(Neighbor {
                ip: o["IPAddress"].as_str()?.to_string(),
                mac: normalize_mac(o["LinkLayerAddress"].as_str()?)?,
                state: o["State"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Usable addresses for `mac` in a neighbour table, as guest addresses.
pub fn lookup_neighbors(table: &[Neighbor], mac: &str) -> Vec<GuestAddr> {
    let Some(mac) = normalize_mac(mac) else {
        return vec![];
    };
    table
        .iter()
        .filter(|n| n.mac == mac)
        .filter(|n| {
            !matches!(
                n.state.to_ascii_lowercase().as_str(),
                "failed" | "incomplete" | "unreachable"
            )
        })
        .filter(|n| useful_ip(&n.ip))
        .map(|n| GuestAddr {
            ip: n.ip.clone(),
            source: AddrSource::Neighbor,
            confirmed: true,
            reachable_from_host: true,
            label: "seen in the host's neighbour table".into(),
        })
        .collect()
}

/// `52-54-00-AB-CD-EF`, `5254.00ab.cdef` or `52:54:00:ab:cd:ef` →
/// `52:54:00:ab:cd:ef`.
pub fn normalize_mac(s: &str) -> Option<String> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let seps = s
        .chars()
        .filter(|c| !c.is_ascii_hexdigit())
        .all(|c| matches!(c, ':' | '-' | '.'));
    if hex.len() != 12 || !seps {
        return None;
    }
    let lower = hex.to_ascii_lowercase();
    Some(
        (0..6)
            .map(|i| &lower[i * 2..i * 2 + 2])
            .collect::<Vec<_>>()
            .join(":"),
    )
}

// --------------------------------------------------------------- selection

/// Order a NIC's candidate addresses: agent, then neighbour, then NAT;
/// confirmed before inferred; duplicates (same IP) keep the better one.
pub fn rank(mut addrs: Vec<GuestAddr>) -> Vec<GuestAddr> {
    let score = |a: &GuestAddr| {
        let s = match a.source {
            AddrSource::GuestAgent => 0,
            AddrSource::Neighbor => 1,
            AddrSource::Nat => 2,
        };
        (s, !a.confirmed)
    };
    addrs.sort_by_key(score);
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(a.ip.clone()));
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    const USERNET: &str = "\
Hub -1 (qui-net0):
  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
  TCP[HOST_FORWARD]  13       127.0.0.1  2222       10.0.2.15    22     0     0
  UDP[236 sec]       26       10.0.2.15    68 255.255.255.255    67     0     0
  TCP[ESTABLISHED]   27       10.0.2.15 51000     198.51.100.7   443     0     0
Hub -1 (qui-net1):
  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ
";

    #[test]
    fn parses_usernet_hubs_and_forwards() {
        let hubs = parse_usernet(USERNET);
        assert_eq!(hubs.len(), 2);
        assert_eq!(hubs[0].netdev, "qui-net0");
        assert_eq!(hubs[0].conns.len(), 3);
        assert_eq!(hubs[0].conns[1].state, "236 sec");
        assert_eq!(
            hubs[0].forwards(),
            vec![ActiveForward {
                proto: "tcp".into(),
                host_addr: "127.0.0.1".into(),
                host_port: 2222,
                guest_port: 22
            }]
        );
        assert!(hubs[1].conns.is_empty());
    }

    #[test]
    fn nat_address_confirmed_by_guest_traffic() {
        let hubs = parse_usernet(USERNET);
        let a = hubs[0].guest_addr();
        assert_eq!(
            (a.ip.as_str(), a.confirmed, a.reachable_from_host),
            ("10.0.2.15", true, false)
        );
        // Only a forward: inferred from its target.
        let only_fwd = UsernetHub {
            netdev: "n".into(),
            conns: vec![hubs[0].conns[0].clone()],
        };
        let a = only_fwd.guest_addr();
        assert_eq!((a.ip.as_str(), a.confirmed), ("10.0.2.15", false));
        // Nothing at all: the DHCP default.
        assert_eq!(hubs[1].guest_addr().ip, SLIRP_GUEST_IP);
        assert!(!hubs[1].guest_addr().confirmed);
        // On a NAT network with its own subnet the default lease moves too.
        assert_eq!(hubs[1].guest_addr_in(Some("10.77.0.0/24")).ip, "10.77.0.15");
    }

    #[test]
    fn guest_static_address_is_observed() {
        let text = "Hub -1 (qui-net0):\n  TCP[SYN_SENT] 9 10.0.2.40 40000 10.0.2.2 80 0 0\n  UDP[10 sec] 10 10.0.2.3 53 10.0.2.40 5353 0 0\n";
        assert_eq!(parse_usernet(text)[0].guest_addr().ip, "10.0.2.40");
    }

    #[test]
    fn agent_interfaces_by_mac() {
        let v = serde_json::json!([
            {"name": "lo", "hardware-address": "00:00:00:00:00:00",
             "ip-addresses": [{"ip-address-type": "ipv4", "ip-address": "127.0.0.1", "prefix": 8}]},
            {"name": "eth0", "hardware-address": "52:54:00:AB:CD:EF",
             "ip-addresses": [
                {"ip-address-type": "ipv4", "ip-address": "10.0.2.15", "prefix": 24},
                {"ip-address-type": "ipv6", "ip-address": "fe80::5054:ff:feab:cdef%eth0", "prefix": 64},
                {"ip-address-type": "ipv6", "ip-address": "2001:db8::15", "prefix": 64}]},
            {"name": "Ethernet 2", "hardware-address": "52-54-00-11-22-33"}
        ]);
        assert_eq!(
            parse_agent_interfaces(&v),
            vec![
                (
                    "52:54:00:ab:cd:ef".into(),
                    vec!["10.0.2.15".into(), "2001:db8::15".into()]
                ),
                ("52:54:00:11:22:33".into(), vec![]),
            ]
        );
    }

    #[test]
    fn linux_neighbors() {
        let text = "\
192.0.2.10 dev br0 lladdr 52:54:00:12:34:56 REACHABLE
192.0.2.11 dev br0  FAILED
192.0.2.12 dev br0 lladdr 52:54:00:12:34:56 STALE
fe80::1 dev br0 lladdr 52:54:00:12:34:56 router STALE
2001:db8::9 dev br0 lladdr 52:54:00:aa:bb:cc DELAY
";
        let t = parse_ip_neigh(text);
        assert_eq!(t.len(), 4);
        let hits = lookup_neighbors(&t, "52-54-00-12-34-56");
        let ips: Vec<&str> = hits.iter().map(|a| a.ip.as_str()).collect();
        assert_eq!(ips, ["192.0.2.10", "192.0.2.12"], "link-local skipped");
        assert!(
            hits.iter()
                .all(|a| a.reachable_from_host && a.source == AddrSource::Neighbor)
        );
    }

    #[test]
    fn windows_neighbors() {
        let arr = r#"[{"IPAddress":"192.0.2.20","LinkLayerAddress":"52-54-00-AA-BB-CC","State":"Reachable"},
                     {"IPAddress":"192.0.2.21","LinkLayerAddress":"52-54-00-AA-BB-CC","State":"Unreachable"},
                     {"IPAddress":"192.0.2.22","LinkLayerAddress":"","State":"Incomplete"}]"#;
        let t = parse_net_neighbor_json(arr);
        assert_eq!(t.len(), 2);
        let hits = lookup_neighbors(&t, "52:54:00:aa:bb:cc");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ip, "192.0.2.20");
        // A single neighbour is serialised as an object, not an array.
        let one =
            r#"{"IPAddress":"192.0.2.30","LinkLayerAddress":"52-54-00-01-02-03","State":"Stale"}"#;
        assert_eq!(parse_net_neighbor_json(one).len(), 1);
        assert!(parse_net_neighbor_json("").is_empty());
    }

    #[test]
    fn macs_normalise() {
        assert_eq!(
            normalize_mac("52-54-00-AB-CD-EF").as_deref(),
            Some("52:54:00:ab:cd:ef")
        );
        assert_eq!(
            normalize_mac("5254.00ab.cdef").as_deref(),
            Some("52:54:00:ab:cd:ef")
        );
        assert_eq!(normalize_mac("52:54:00:ab:cd"), None);
        assert_eq!(normalize_mac("52 54 00 ab cd ef"), None);
    }

    #[test]
    fn ranking_prefers_agent_then_confirmed() {
        let r = rank(vec![
            nat_addr("10.0.2.15".into(), false),
            agent_addr("10.0.2.15".into()),
            agent_addr("2001:db8::15".into()),
        ]);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].source, AddrSource::GuestAgent);
        assert_eq!(r[0].ip, "10.0.2.15");
    }
}
