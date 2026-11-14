//! passt NAT sidecars (plan.md §10.1, M3-03; Linux only).
//!
//! Each NIC on a passt network gets its own `passt` process listening on a
//! unix socket in the run directory; QEMU connects to it with
//! `-netdev stream`. passt runs in the foreground with `--one-off`, so it
//! exits when QEMU disconnects; the supervisor records it as a run sidecar
//! and terminates any leftover when the run ends, like virtiofsd.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use qemu_ui_core::compile::passt_socket;
use qemu_ui_core::network::{Network, NetworkConfig, passt_args};
use qemu_ui_core::spec::{NicBackend, NicSpec};

use crate::procs::{self, ProcId};

/// Locate a `passt` binary (Linux): `QEMU_UI_PASST`, then `PATH`, then the
/// usual install locations.
pub fn find_passt() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if let Some(p) = std::env::var_os("QEMU_UI_PASST").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("passt")).collect())
        .unwrap_or_default();
    candidates.push(PathBuf::from("/usr/bin/passt"));
    candidates.push(PathBuf::from("/usr/local/bin/passt"));
    candidates.into_iter().find(|p| p.is_file())
}

/// One passt sidecar to start: NIC index, socket, arguments.
#[derive(Debug, PartialEq)]
pub struct PasstPlan {
    pub nic: usize,
    pub socket: String,
    pub args: Vec<String>,
}

/// The passt sidecars a VM's NICs need; `qmp` is the run's QMP socket
/// (the sockets sit next to it, where the compiled argv points QEMU).
pub fn plan(nics: &[NicSpec], networks: &[Network], qmp: &str) -> Vec<PasstPlan> {
    nics.iter()
        .enumerate()
        .filter_map(|(i, nic)| {
            let NicBackend::Network { network, forwards } = &nic.backend else {
                return None;
            };
            let n = networks.iter().find(|n| &n.id == network)?;
            let NetworkConfig::Passt { mtu } = &n.config else {
                return None;
            };
            let socket = passt_socket(qmp, i);
            Some(PasstPlan {
                nic: i,
                args: passt_args(&socket, forwards, *mtu),
                socket,
            })
        })
        .collect()
}

/// Start the planned sidecars and wait for their sockets. On failure,
/// everything started so far is terminated.
pub async fn start(
    bin: Option<&Path>,
    plans: &[PasstPlan],
    logs: &Path,
    stamp: u64,
) -> Result<Vec<ProcId>> {
    let mut started: Vec<ProcId> = Vec::new();
    let stop_all = |started: &[ProcId]| {
        for p in started {
            procs::terminate(*p, true);
        }
    };
    for p in plans {
        let Some(bin) = bin else {
            stop_all(&started);
            bail!(
                "passt is not installed; install the passt package (or set QEMU_UI_PASST) to use passt networks"
            );
        };
        let _ = std::fs::remove_file(&p.socket);
        let log = logs.join(format!("run-{stamp}.passt{}.log", p.nic));
        let (child, id) = match procs::spawn_detached(&bin.display().to_string(), &p.args, &log) {
            Ok(x) => x,
            Err(e) => {
                stop_all(&started);
                return Err(e);
            }
        };
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        started.push(id);
        let sock = PathBuf::from(&p.socket);
        let mut ready = false;
        for _ in 0..100 {
            if sock.exists() {
                ready = true;
                break;
            }
            if !procs::alive(id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !ready {
            stop_all(&started);
            bail!(
                "passt for NIC {} did not start: {}",
                p.nic,
                procs::log_tail(&log, 5)
            );
        }
    }
    Ok(started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qemu_ui_core::spec::PortForward;

    #[test]
    fn plans_one_sidecar_per_passt_nic() {
        let nets = [
            Network {
                id: "p".into(),
                name: "fast".into(),
                config: NetworkConfig::Passt { mtu: None },
            },
            Network {
                id: "n".into(),
                name: "nat".into(),
                config: NetworkConfig::Nat { subnet: None },
            },
        ];
        let nic = |backend| NicSpec {
            backend,
            model: "virtio-net-pci".into(),
            mac: None,
            queues: None,
            virtio: None,
        };
        let fw = PortForward {
            proto: "tcp".into(),
            host_addr: "127.0.0.1".into(),
            host_port: 2222,
            guest_port: 22,
        };
        let nics = [
            nic(NicBackend::User { forwards: vec![] }),
            nic(NicBackend::Network {
                network: "n".into(),
                forwards: vec![],
            }),
            nic(NicBackend::Network {
                network: "p".into(),
                forwards: vec![fw],
            }),
        ];
        let qmp = "/run/q/vm/qmp.sock";
        let p = plan(&nics, &nets, qmp);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].nic, 2);
        assert_eq!(p[0].socket, passt_socket(qmp, 2));
        assert!(
            p[0].args
                .ends_with(&["-t".to_string(), "127.0.0.1/2222:22".to_string()])
        );
    }
}

/// A VM on a passt network runs with its passt sidecar, which is recorded
/// for the run and gone once the VM stops (Linux with passt installed).
#[cfg(all(test, target_os = "linux"))]
mod real {
    use super::*;
    use crate::supervisor::test_support::*;
    use qemu_ui_core::spec::PortForward;
    use qemu_ui_core::vm::VmState;

    #[test]
    fn vm_runs_with_a_passt_sidecar_that_ends_with_it() {
        let (Some(qemu), Some(img), Some(_)) =
            (find("qemu-system-x86_64"), find("qemu-img"), find_passt())
        else {
            eprintln!("skipping: QEMU or passt not found");
            return;
        };
        let data = short_data_dir("passt");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "passty").await;
            let net = sv
                .networks
                .create("fast", NetworkConfig::Passt { mtu: None })
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.nics[0].backend = NicBackend::Network {
                network: net.id.clone(),
                forwards: vec![PortForward {
                    proto: "tcp".into(),
                    host_addr: "127.0.0.1".into(),
                    host_port: port,
                    guest_port: 22,
                }],
            };
            sv.update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            let s = sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert_eq!(s.state, VmState::Running);
            let run = sv.store.last_run(&id).unwrap().unwrap();
            let sidecars = sv.store.sidecars(run.id).unwrap();
            let (_, pid, start) = sidecars
                .iter()
                .find(|(k, _, _)| k == "passt")
                .cloned()
                .expect("passt sidecar recorded");
            let proc_id = ProcId {
                pid,
                start_time: start,
            };
            assert!(procs::alive(proc_id), "passt runs alongside QEMU");
            // passt is listening on the forward.
            assert!(
                std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
                "forward bound by passt"
            );

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            let mut gone = false;
            for _ in 0..50 {
                if !procs::alive(proc_id) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(gone, "passt exits with the run");
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
