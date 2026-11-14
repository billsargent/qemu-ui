//! Taps for NICs on tap-backed networks (M3-05/06).
//!
//! Linux bridged/host-only: on an actual start the privileged helper
//! creates one tap per NIC, owned by the daemon user, and attaches it to the
//! bridge; the taps are recorded and deleted when the run ends (or fails to
//! start). Windows TAP: the TAP-Windows adapter itself is the tap. Previews
//! and dry lints never ask the helper for anything.

use std::collections::BTreeMap;

use qemu_ui_core::compile::{Diagnostic, Severity, TapSource};
use qemu_ui_core::helper::TapNetwork;
use qemu_ui_core::network::{Network, NetworkConfig};
use qemu_ui_core::spec::{NicBackend, VmSpec};

use super::{BuildMode, Supervisor};

const HOST_TAPS_KEY: &str = "host-taps:";

fn error(i: usize, rule: &str, message: String) -> Vec<Diagnostic> {
    vec![Diagnostic {
        severity: Severity::Error,
        field: format!("nics[{i}].backend.network"),
        rule: rule.into(),
        message,
    }]
}

impl Supervisor {
    /// The tap of every NIC on a tap-backed network this host can run;
    /// other cases are left to the compiler's diagnostics.
    pub(super) async fn resolve_taps(
        &self,
        vm_id: &str,
        spec: &VmSpec,
        networks: &[Network],
        mode: &BuildMode,
    ) -> Result<BTreeMap<usize, TapSource>, Vec<Diagnostic>> {
        let os = std::env::consts::OS;
        let mut out = BTreeMap::new();
        let mut adapters: Option<Vec<qemu_ui_core::hostnet::HostNetAdapter>> = None;
        for (i, nic) in spec.nics.iter().enumerate() {
            let NicBackend::Network { network, .. } = &nic.backend else {
                continue;
            };
            let Some(net) = networks.iter().find(|n| &n.id == network) else {
                continue;
            };
            if !net.config.is_tap() || net.config.host_unsupported_reason(os).is_some() {
                continue;
            }
            let tap = match &net.config {
                NetworkConfig::Tap { adapter } => {
                    if matches!(mode, BuildMode::Run) {
                        if adapters.is_none() {
                            adapters = Some(crate::hostnet::adapters().await.unwrap_or_default());
                        }
                        let known = adapters.as_deref().unwrap_or_default();
                        if !known.iter().any(|a| &a.name == adapter) {
                            let taps: Vec<&str> = known
                                .iter()
                                .filter(|a| a.is_tap)
                                .map(|a| a.name.as_str())
                                .collect();
                            return Err(error(
                                i,
                                "nic.tap-adapter",
                                if taps.is_empty() {
                                    crate::hostnet::TAP_WINDOWS_SETUP.to_string()
                                } else {
                                    format!(
                                        "no network adapter named '{adapter}'; TAP adapters on this host: {}",
                                        taps.join(", ")
                                    )
                                },
                            ));
                        }
                    }
                    TapSource {
                        ifname: adapter.clone(),
                        vhost: false,
                    }
                }
                NetworkConfig::Bridged { .. } | NetworkConfig::HostOnly {} => {
                    let placeholder = || {
                        qemu_ui_core::hostnet::tap_name(vm_id, i as u32)
                            .unwrap_or_else(|| format!("qui-tap{i}"))
                    };
                    let ifname = match mode {
                        BuildMode::Preview | BuildMode::Lint(_) => placeholder(),
                        BuildMode::Run => {
                            let req = match &net.config {
                                NetworkConfig::Bridged { bridge } => TapNetwork::Bridge {
                                    bridge: bridge.clone(),
                                },
                                _ => TapNetwork::HostOnly,
                            };
                            let mq = nic.queues.is_some_and(|q| q > 1);
                            match self.helper().create_tap(vm_id, i as u32, req, mq).await {
                                Ok(t) => {
                                    self.record_tap(vm_id, &t.ifname);
                                    t.ifname
                                }
                                Err(e) => {
                                    self.release_taps(vm_id);
                                    return Err(error(
                                        i,
                                        "nic.tap-create",
                                        format!(
                                            "{} network '{}': {e:#}",
                                            net.config.kind_label(),
                                            net.name
                                        ),
                                    ));
                                }
                            }
                        }
                    };
                    TapSource {
                        ifname,
                        vhost: crate::hostnet::vhost_net_usable(),
                    }
                }
                // Not tap-backed (filtered above).
                NetworkConfig::Nat { .. } | NetworkConfig::Passt { .. } => continue,
            };
            out.insert(i, tap);
        }
        Ok(out)
    }

    fn record_tap(&self, vm_id: &str, ifname: &str) {
        let key = format!("{HOST_TAPS_KEY}{vm_id}");
        let mut names: Vec<String> = self
            .store
            .get_kv(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        names.push(ifname.into());
        if let Ok(s) = serde_json::to_string(&names) {
            let _ = self.store.put_kv(&key, &s);
        }
    }

    /// Delete a VM's helper-created taps (its run ended or failed to start).
    pub(super) fn release_taps(&self, vm_id: &str) {
        let key = format!("{HOST_TAPS_KEY}{vm_id}");
        let names: Vec<String> = self
            .store
            .get_kv(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if names.is_empty() {
            return;
        }
        let _ = self.store.put_kv(&key, "[]");
        let helper = self.helper();
        let vm = vm_id.to_string();
        tokio::spawn(async move {
            for n in names {
                if let Err(e) = helper.delete_tap(&n).await {
                    tracing::warn!(vm = %vm, "deleting tap {n}: {e:#}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use qemu_ui_core::compile::Compiled;
    use qemu_ui_core::spec::NicBackend;

    fn netdev(c: &Compiled) -> serde_json::Value {
        let a = c
            .args
            .iter()
            .find(|a| a.contains("\"id\":\"qui-net0\""))
            .unwrap();
        serde_json::from_str(a).unwrap()
    }

    async fn on_network(
        sv: &Supervisor,
        id: &str,
        config: NetworkConfig,
    ) -> (VmSpec, qemu_ui_core::caps::BinaryCaps) {
        let net = sv
            .networks
            .create("lan", config)
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        let mut spec = sv.detail(id).ok().unwrap().spec;
        spec.nics[0].backend = NicBackend::Network {
            network: net.id,
            forwards: vec![],
        };
        sv.update(id, spec.clone())
            .await
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        let caps = sv.caps_for(&spec.arch).await.unwrap();
        (spec, caps)
    }

    /// The preview of a TAP (Windows) or bridged (Linux) NIC is a tap
    /// netdev and asks no helper; on other hosts the kind is refused.
    #[test]
    fn preview_shows_a_tap_netdev_without_side_effects() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("tapp");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            // No helper anywhere: a preview must not need one.
            sv.set_helper(crate::helper_client::HelperClient {
                socket: data.join("none.sock"),
                secret_file: None,
            });
            let id = small_vm(&sv, &caps, "tapped").await;
            let config = if cfg!(windows) {
                NetworkConfig::Tap {
                    adapter: "Example TAP".into(),
                }
            } else {
                NetworkConfig::Bridged {
                    bridge: "br0".into(),
                }
            };
            let (spec, bcaps) = on_network(&sv, &id, config).await;
            let r = sv.build(&id, &spec, &bcaps, &BuildMode::Preview).await;
            if cfg!(any(windows, target_os = "linux")) {
                let c = r.map_err(|d| format!("{d:?}")).unwrap();
                let nd = netdev(&c.0);
                assert_eq!(nd["type"], "tap");
                let want = if cfg!(windows) {
                    "Example TAP".to_string()
                } else {
                    format!("qui-{id}-0")
                };
                assert_eq!(nd["ifname"], want.as_str());
            } else {
                assert!(r.is_err());
            }
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Linux: a start asks the helper for a tap per bridged NIC, compiles a
    /// `-netdev tap` for it, and deletes it when the run is released. The
    /// stand-in helper only answers the protocol; no interface is created.
    #[cfg(target_os = "linux")]
    #[test]
    fn helper_taps_are_requested_compiled_and_released() {
        use qemu_ui_core::helper::{Envelope, Request, Response, TapInfo};
        use std::io::{BufRead, BufReader, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("tapr");
        let sock = data.join("helper.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let deleted = Arc::new(AtomicUsize::new(0));
        let count = deleted.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let mut w = conn.try_clone().unwrap();
                for line in BufReader::new(conn).lines() {
                    let Ok(line) = line else { break };
                    let env: Envelope<Request> = serde_json::from_str(&line).unwrap();
                    let body = match env.body {
                        Request::Hello { version, .. } => Response::Hello { version },
                        Request::CreateTap {
                            vm_id,
                            nic,
                            network: TapNetwork::Bridge { bridge },
                            multi_queue: false,
                        } => Response::TapCreated {
                            tap: TapInfo {
                                ifname: format!("qui-{vm_id}-{nic}"),
                                bridge,
                            },
                        },
                        Request::DeleteTap { .. } => {
                            count.fetch_add(1, Ordering::SeqCst);
                            Response::TapDeleted
                        }
                        other => Response::Error {
                            message: format!("unexpected {other:?}"),
                        },
                    };
                    let mut out = serde_json::to_vec(&Envelope { id: env.id, body }).unwrap();
                    out.push(b'\n');
                    if w.write_all(&out).is_err() {
                        break;
                    }
                }
            }
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            sv.set_helper(crate::helper_client::HelperClient {
                socket: sock.clone(),
                secret_file: None,
            });
            let id = small_vm(&sv, &caps, "bridged").await;
            let (spec, bcaps) = on_network(
                &sv,
                &id,
                NetworkConfig::Bridged {
                    bridge: "br0".into(),
                },
            )
            .await;
            let (c, _) = sv
                .build(&id, &spec, &bcaps, &BuildMode::Run)
                .await
                .map_err(|d| format!("{d:?}"))
                .unwrap();
            let nd = netdev(&c);
            assert_eq!(nd["type"], "tap");
            assert_eq!(nd["ifname"], format!("qui-{id}-0").as_str());
            assert_eq!(nd["script"], "no");
            sv.release_taps(&id);
            for _ in 0..50 {
                if deleted.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert_eq!(deleted.load(Ordering::SeqCst), 1, "released");
            sv.release_taps(&id);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert_eq!(deleted.load(Ordering::SeqCst), 1, "released once");
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
