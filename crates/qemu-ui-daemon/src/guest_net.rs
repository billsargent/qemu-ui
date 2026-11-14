//! Guest IP detection (M3-08): asks each source the running VM offers and
//! ranks the answers per NIC (see `qemu_ui_core::guest_net`).
//!
//! * Guest agent: when the VM has a `qui-ga0` chardev (the device arrives
//!   with M7-02), `guest-network-get-interfaces` over its socket.
//! * slirp NAT: `info usernet` per `qui-net<i>` netdev.
//! * Host neighbour table: for NICs on bridged, host-only and TAP networks
//!   (`hostnet::neighbors`; parsing in core).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::guest_net::normalize_mac;
use qemu_ui_core::guest_net::{
    GuestNetInfo, GuestNic, UsernetHub, agent_addr, parse_agent_interfaces, parse_usernet, rank,
};
use qemu_ui_core::network::NetworkConfig;
use qemu_ui_core::spec::NicBackend;
use qemu_ui_core::vm::VmState;
use qemu_ui_qmp::Qmp;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::supervisor::{OpResult, Supervisor};

/// The guest-agent chardev id the compiler reserves (M7-02).
pub const AGENT_CHARDEV: &str = "qui-ga0";
const AGENT_TIMEOUT: Duration = Duration::from_secs(3);

/// Everything known about a VM's guest addresses right now.
pub async fn guest_net(sv: &Supervisor, id: &str) -> OpResult<GuestNetInfo> {
    let detail = sv.detail(id)?;
    let running = matches!(detail.summary.state, VmState::Running | VmState::Paused);
    let spec = detail.live_spec.clone().unwrap_or(detail.spec);
    let qmp = if running { sv.live_qmp(id) } else { None };
    let Some(qmp) = qmp else {
        return Ok(GuestNetInfo {
            running: false,
            agent_channel: false,
            agent_error: None,
            nics: spec
                .nics
                .iter()
                .enumerate()
                .map(|(i, n)| GuestNic {
                    nic: i,
                    mac: n.mac.clone(),
                    addresses: vec![],
                    active_forwards: vec![],
                })
                .collect(),
        });
    };

    let networks = sv.networks.list().unwrap_or_default();
    // The neighbour table, only when some NIC is on a tap-backed network.
    let tap_backed = spec.nics.iter().any(|n| {
        matches!(&n.backend, NicBackend::Network { network, .. }
            if networks.iter().any(|x| &x.id == network && x.config.is_tap()))
    });
    let neighbors = if tap_backed {
        crate::hostnet::neighbors().await.unwrap_or_else(|e| {
            tracing::debug!(vm = %id, "neighbour table: {e:#}");
            vec![]
        })
    } else {
        vec![]
    };
    let hubs = usernet(&qmp).await.unwrap_or_else(|e| {
        tracing::debug!(vm = %id, "info usernet: {e:#}");
        vec![]
    });
    let agent_sock = agent_socket(&qmp).await;
    let (agent, agent_error) = match &agent_sock {
        Some(p) => match agent_interfaces(p).await {
            Ok(v) => (parse_agent_interfaces(&v), None),
            Err(e) => (vec![], Some(format!("{e:#}"))),
        },
        None => (vec![], None),
    };

    let nics = spec
        .nics
        .iter()
        .enumerate()
        .map(|(i, nic)| {
            let mac = nic.mac.as_deref().and_then(normalize_mac);
            let mut addrs: Vec<_> = agent
                .iter()
                .filter(|(m, _)| Some(m) == mac.as_ref())
                .flat_map(|(_, ips)| ips.iter().cloned().map(agent_addr))
                .collect();
            let mut active_forwards = vec![];
            // NAT (a per-VM `user` NIC or a NAT network) is slirp either way.
            let nat_subnet: Option<Option<String>> = match &nic.backend {
                NicBackend::User { .. } => Some(None),
                NicBackend::Network { network, .. } => networks
                    .iter()
                    .find(|n| &n.id == network)
                    .and_then(|n| match &n.config {
                        NetworkConfig::Nat { subnet } => Some(subnet.clone()),
                        // Tap-backed: the guest is on a host bridge/adapter.
                        NetworkConfig::Bridged { .. }
                        | NetworkConfig::HostOnly {}
                        | NetworkConfig::Tap { .. } => None,
                    }),
            };
            if nat_subnet.is_none()
                && let Some(mac) = &nic.mac
            {
                addrs.extend(qemu_ui_core::guest_net::lookup_neighbors(&neighbors, mac));
            }
            if let Some(subnet) = nat_subnet {
                let netdev = format!("qui-net{i}");
                let hub = hubs.iter().find(|h| h.netdev == netdev);
                addrs.push(hub.map_or_else(
                    || {
                        UsernetHub {
                            netdev: netdev.clone(),
                            conns: vec![],
                        }
                        .guest_addr_in(subnet.as_deref())
                    },
                    |h| h.guest_addr_in(subnet.as_deref()),
                ));
                active_forwards = hub.map(UsernetHub::forwards).unwrap_or_default();
            }
            GuestNic {
                nic: i,
                mac: nic.mac.clone(),
                addresses: rank(addrs),
                active_forwards,
            }
        })
        .collect();

    Ok(GuestNetInfo {
        running: true,
        agent_channel: agent_sock.is_some(),
        agent_error,
        nics,
    })
}

async fn usernet(qmp: &Qmp) -> Result<Vec<UsernetHub>> {
    let out = qmp
        .execute(
            "human-monitor-command",
            Some(json!({"command-line": "info usernet"})),
        )
        .await?;
    Ok(parse_usernet(out.as_str().unwrap_or_default()))
}

/// The guest agent's socket, when the VM has a `qui-ga0` unix chardev.
async fn agent_socket(qmp: &Qmp) -> Option<PathBuf> {
    let v = qmp.execute("query-chardev", None).await.ok()?;
    let dev = v.as_array()?.iter().find(|c| c["label"] == AGENT_CHARDEV)?;
    // e.g. "unix:/run/qui/vm/ga.sock,server=on"
    let f = dev["filename"].as_str()?.strip_prefix("unix:")?;
    Some(PathBuf::from(f.split(",server").next().unwrap_or(f)))
}

/// `guest-network-get-interfaces` through the agent socket. The agent has
/// no greeting; `guest-sync` first flushes any stale reply.
async fn agent_interfaces(sock: &std::path::Path) -> Result<Value> {
    let fut = async {
        let stream = qemu_ui_qmp::transport::connect_unix(sock)
            .await
            .context("connecting to the guest agent channel")?;
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        let token: u32 = rand::random::<u32>() & 0x7fff_ffff;
        let sync = json!({"execute": "guest-sync", "arguments": {"id": token}});
        w.write_all(format!("{sync}\n").as_bytes()).await?;
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("the guest agent closed the channel"))?;
            if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["return"] == token) {
                break;
            }
        }
        let req = json!({"execute": "guest-network-get-interfaces"});
        w.write_all(format!("{req}\n").as_bytes()).await?;
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("the guest agent closed the channel"))?;
        let v: Value = serde_json::from_str(&line)?;
        if let Some(e) = v.get("error") {
            bail!("guest agent: {}", e["desc"].as_str().unwrap_or("error"));
        }
        Ok(v["return"].clone())
    };
    tokio::time::timeout(AGENT_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("the guest agent did not answer (is it running in the guest?)"))?
}

#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use qemu_ui_core::guest_net::{AddrSource, GuestNetInfo};
    use qemu_ui_core::spec::PortForward;
    use qemu_ui_core::vm::VmState;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn get(r: &axum::Router, uri: &str) -> (u16, Option<GuestNetInfo>) {
        let req = Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"))
            .body(Body::empty())
            .unwrap();
        let res = r.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (
            status,
            serde_json::from_value(body.clone())
                .map_err(|e| format!("{e}: {body}"))
                .ok(),
        )
    }

    #[test]
    fn nat_address_and_forwards_for_a_running_vm() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("gnet");
        let _guard = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "gnet").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            assert!(!spec.nics.is_empty(), "default spec has a NIC");
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            spec.nics[0].backend.forwards_mut().push(PortForward {
                proto: "tcp".into(),
                host_addr: "127.0.0.1".into(),
                host_port: port,
                guest_port: 22,
            });
            sv.update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);
            let uri = format!("/api/v1/vms/{id}/guest-net");

            // Stopped: nothing detected.
            let (st, info) = get(&r, &uri).await;
            assert_eq!(st, 200);
            let info = info.unwrap();
            assert!(!info.running);
            assert!(info.nics.iter().all(|n| n.addresses.is_empty()));

            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let (st, info) = get(&r, &uri).await;
            assert_eq!(st, 200);
            let info = info.unwrap();
            assert!(info.running);
            assert!(!info.agent_channel);
            let nic = &info.nics[0];
            let a = &nic.addresses[0];
            assert_eq!(a.source, AddrSource::Nat);
            assert!(a.ip.starts_with("10.0.2."), "{a:?}");
            assert!(!a.reachable_from_host);
            assert!(
                nic.active_forwards
                    .iter()
                    .any(|f| f.host_port == port && f.guest_port == 22 && f.proto == "tcp"),
                "the forward is parsed from info usernet: {:?}",
                nic.active_forwards
            );

            let (st, _) = get(&r, "/api/v1/vms/nope/guest-net").await;
            assert_eq!(st, 404);

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
