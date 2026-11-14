//! Port forwards on NAT NICs (plan.md §10.2, M3-02).
//!
//! A forward is part of the NIC's spec, so a cold start passes it as
//! `-netdev user,hostfwd=…`. On a running VM it is applied live through the
//! HMP `hostfwd_add` / `hostfwd_remove` commands (slirp has no QMP
//! equivalent) and recorded in both the run's spec and the saved spec, the
//! same way disk hotplug is (see `disks.rs`), so it survives a restart and
//! `restart_pending` stays meaningful.
//!
//! A VM's own NAT (`user`) and NAT network objects are both a slirp netdev
//! `qui-net<i>`, so both change live. passt networks take forwards as
//! sidecar arguments fixed at start: they change only while the VM is
//! stopped. Isolated networks have no forwards (network validation says so).
//!
//! HMP reports results as text in a successful reply: `hostfwd_add` is
//! silent on success, `hostfwd_remove` says "… removed"; anything else is
//! QEMU's error message.

use super::*;
use qemu_ui_core::compile::{forwards_overlap, hostfwd_host, hostfwd_rule};
use qemu_ui_core::network::{Network, NetworkConfig};
use qemu_ui_core::spec::{NicBackend, PortForward};
use qemu_ui_core::vm::ForwardChange;

fn invalid(field: String, rule: &str, msg: String) -> OpError {
    OpError::Invalid(vec![Diagnostic {
        severity: Severity::Error,
        field,
        rule: rule.into(),
        message: msg,
    }])
}

/// The forwards of NIC `i`.
fn nic_forwards(spec: &mut VmSpec, i: usize) -> OpResult<&mut Vec<PortForward>> {
    Ok(spec
        .nics
        .get_mut(i)
        .ok_or(OpError::NotFound)?
        .backend
        .forwards_mut())
}

/// Can NIC `i` of a running VM (its live spec) change forwards now? Only
/// slirp netdevs can.
fn live_forwardable(live: &VmSpec, i: usize, networks: &[Network]) -> OpResult<()> {
    let nic = live.nics.get(i).ok_or_else(|| {
        OpError::Conflict(
            "this adapter was added after the VM started; restart the VM to change its forwards"
                .into(),
        )
    })?;
    let NicBackend::Network { network, .. } = &nic.backend else {
        return Ok(());
    };
    match networks.iter().find(|n| &n.id == network).map(|n| &n.config) {
        Some(NetworkConfig::Nat { .. }) => Ok(()),
        Some(NetworkConfig::Passt { .. }) => Err(OpError::Conflict(
            "forwards on a passt network are set when the VM starts and can't change while it runs; \
             stop the VM to change them (restart the VM to apply)"
                .into(),
        )),
        Some(c) => Err(OpError::Conflict(c.forwards_unsupported_reason())),
        None => Err(OpError::Conflict(format!(
            "network '{network}' no longer exists; restart the VM to change its forwards"
        ))),
    }
}

/// Run an HMP command; reply text other than `ok_text` is QEMU's error.
async fn hmp(qmp: &Qmp, cmd: &str, ok_text: Option<&str>) -> OpResult<()> {
    let out = qmp
        .execute("human-monitor-command", Some(json!({"command-line": cmd})))
        .await
        .map_err(|e| OpError::Failed(e.into()))?;
    let text = out.as_str().unwrap_or_default().trim();
    if text.is_empty() || ok_text.is_some_and(|t| text.ends_with(t)) {
        Ok(())
    } else {
        Err(OpError::Conflict(text.to_string()))
    }
}

impl Supervisor {
    /// Add a forward to NIC `i`: live when the VM runs, and always saved.
    pub async fn add_forward(
        &self,
        id: &str,
        i: usize,
        fw: PortForward,
    ) -> OpResult<ForwardChange> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let caps = self.caps_for(&saved.arch).await?;
        let networks = self.networks.list().map_err(OpError::Failed)?;
        let pos = nic_forwards(&mut saved, i)?.len();
        let field = format!("nics[{i}].backend.forwards[{pos}]");

        // The same checks as a save, for this forward and this NIC's network.
        let mut check = saved.clone();
        nic_forwards(&mut check, i)?.push(fw.clone());
        let nic_fwds = format!("nics[{i}].backend.forwards");
        let errs: Vec<Diagnostic> = compile::validate(&check, &caps)
            .into_iter()
            .filter(|d| d.field.starts_with(&field))
            .chain(
                compile::validate_networks(&check, &networks)
                    .into_iter()
                    .filter(|d| d.field.starts_with(&nic_fwds)),
            )
            .filter(|d| d.severity == Severity::Error)
            .collect();
        if !errs.is_empty() {
            return Err(OpError::Invalid(errs));
        }

        let Some(run) = self.active_run(id) else {
            nic_forwards(&mut saved, i)?.push(fw);
            self.save_spec(id, &saved)?;
            return Ok(ForwardChange {
                live: false,
                message: "Forward saved; it applies at the next start".into(),
                vm: self.detail(id)?,
            });
        };

        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        live_forwardable(&live, i, &networks)?;
        let live_fwds = nic_forwards(&mut live, i)?;
        if live_fwds.iter().any(|g| forwards_overlap(&fw, g)) {
            return Err(invalid(
                field,
                "forward.duplicate",
                format!(
                    "host {} port {} is already forwarded on the running VM",
                    fw.proto, fw.host_port
                ),
            ));
        }
        let qmp = self.qmp_of(id)?;
        hmp(
            &qmp,
            &format!("hostfwd_add qui-net{i} {}", hostfwd_rule(&fw)),
            None,
        )
        .await
        .map_err(|e| match e {
            OpError::Conflict(m) => invalid(
                field.clone(),
                "forward.bind",
                format!(
                    "QEMU could not listen on {}:{} (is the port in use?): {m}",
                    fw.host_addr, fw.host_port
                ),
            ),
            e => e,
        })?;

        live_fwds.push(fw.clone());
        nic_forwards(&mut saved, i)?.push(fw.clone());
        let live_json = serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_run_spec(run.id, &live_json)?;
        self.save_spec(id, &saved)?;
        self.emit(id, "forward-added", json!({"nic": i, "forward": fw}));
        Ok(ForwardChange {
            live: true,
            message: format!(
                "{} {}:{} now forwards to guest port {}",
                fw.proto, fw.host_addr, fw.host_port, fw.guest_port
            ),
            vm: self.detail(id)?,
        })
    }

    /// Remove forward `j` of NIC `i` (positions in the saved spec). On a
    /// running VM the matching live forward is removed too.
    pub async fn remove_forward(&self, id: &str, i: usize, j: usize) -> OpResult<ForwardChange> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let fwds = nic_forwards(&mut saved, i)?;
        if j >= fwds.len() {
            return Err(OpError::NotFound);
        }
        let fw = fwds.remove(j);

        let mut live_applied = false;
        if let Some(run) = self.active_run(id) {
            let mut live: VmSpec =
                serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
            let k = live
                .nics
                .get(i)
                .and_then(|n| n.backend.forwards().iter().position(|g| *g == fw));
            if let Some(k) = k {
                let networks = self.networks.list().map_err(OpError::Failed)?;
                live_forwardable(&live, i, &networks)?;
                let qmp = self.qmp_of(id)?;
                hmp(
                    &qmp,
                    &format!("hostfwd_remove qui-net{i} {}", hostfwd_host(&fw)),
                    Some("removed"),
                )
                .await?;
                nic_forwards(&mut live, i)?.remove(k);
                let live_json =
                    serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
                self.store.update_run_spec(run.id, &live_json)?;
                live_applied = true;
            }
        }
        self.save_spec(id, &saved)?;
        self.emit(id, "forward-removed", json!({"nic": i, "forward": fw}));
        Ok(ForwardChange {
            live: live_applied,
            message: if live_applied {
                format!("{} port {} is no longer forwarded", fw.proto, fw.host_port)
            } else {
                "Forward removed".into()
            },
            vm: self.detail(id)?,
        })
    }

    fn save_spec(&self, id: &str, spec: &VmSpec) -> OpResult<()> {
        let json = serde_json::to_string(spec).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_vm_spec(id, &spec.name, &json, now())?;
        self.emit(id, "updated", json!({}));
        Ok(())
    }

    /// Warnings for host ports this spec forwards that another running VM
    /// already holds: the start would fail to bind them.
    pub(super) fn forward_conflicts(&self, id: Option<&str>, spec: &VmSpec) -> Vec<Diagnostic> {
        let Ok(rows) = self.store.list_vms() else {
            return vec![];
        };
        let others: Vec<(String, VmSpec)> = rows
            .into_iter()
            .filter(|r| Some(r.id.as_str()) != id)
            .filter_map(|r| {
                let run = self.active_run(&r.id)?;
                let s = serde_json::from_str(&run.spec_json).ok()?;
                Some((r.name, s))
            })
            .collect();
        let mut d = Vec::new();
        for (i, nic) in spec.nics.iter().enumerate() {
            for (j, f) in nic.backend.forwards().iter().enumerate() {
                let holder = others.iter().find(|(_, s)| {
                    s.nics
                        .iter()
                        .any(|n| n.backend.forwards().iter().any(|g| forwards_overlap(f, g)))
                });
                if let Some((name, _)) = holder {
                    d.push(Diagnostic {
                        severity: Severity::Warning,
                        field: format!("nics[{i}].backend.forwards[{j}]"),
                        rule: "forward.in-use".into(),
                        message: format!(
                            "host {} port {} is in use by the running VM '{name}'",
                            f.proto, f.host_port
                        ),
                    });
                }
            }
        }
        d
    }
}

#[cfg(test)]
mod rules {
    use super::*;

    fn net(id: &str, config: NetworkConfig) -> Network {
        Network {
            id: id.into(),
            name: id.into(),
            config,
        }
    }

    fn spec_on(backend: serde_json::Value) -> VmSpec {
        serde_json::from_value(json!({
            "spec_version": qemu_ui_core::spec::SPEC_VERSION, "name": "t", "arch": "x86_64",
            "machine": {"type": "pc"}, "cpu": {"count": 1}, "memory": {"size_mib": 128},
            "nics": [{"backend": backend, "model": "e1000", "mac": "52:54:00:00:00:01"}],
        }))
        .unwrap()
    }

    #[test]
    fn which_nics_change_forwards_live() {
        let nets = [
            net("nat", NetworkConfig::Nat { subnet: None }),
            net("passt", NetworkConfig::Passt { mtu: None }),
            net("iso", NetworkConfig::Isolated {}),
            net("host-only", NetworkConfig::HostOnly {}),
        ];
        // Bridged and Tap carry host-specific settings; build them from JSON.
        let nets: Vec<Network> = nets
            .into_iter()
            .chain([
                net(
                    "bridged",
                    serde_json::from_value(json!({"kind": "bridged", "bridge": "br0"})).unwrap(),
                ),
                net(
                    "tap",
                    serde_json::from_value(json!({"kind": "tap", "adapter": "TAP0"})).unwrap(),
                ),
            ])
            .collect();
        let on = |n: &str| spec_on(json!({"kind": "network", "network": n}));
        assert!(live_forwardable(&spec_on(json!({"kind": "user"})), 0, &nets).is_ok());
        assert!(live_forwardable(&on("nat"), 0, &nets).is_ok());
        let passt = format!("{:?}", live_forwardable(&on("passt"), 0, &nets));
        assert!(passt.contains("restart the VM to apply"), "{passt}");
        for kind in ["iso", "bridged", "host-only", "tap"] {
            assert!(
                matches!(
                    live_forwardable(&on(kind), 0, &nets),
                    Err(OpError::Conflict(_))
                ),
                "{kind} has no forwards"
            );
        }
        assert!(matches!(
            live_forwardable(&on("gone"), 0, &nets),
            Err(OpError::Conflict(_))
        ));
        assert!(
            matches!(live_forwardable(&on("nat"), 1, &nets), Err(OpError::Conflict(m)) if m.contains("added after")),
            "a NIC the run does not have"
        );
    }
}

#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;

    /// The `HOST_FORWARD` lines of `info usernet` (other lines list
    /// connections, which outlive a removed forward).
    async fn usernet(qmp: &Qmp) -> String {
        let out = qmp
            .execute(
                "human-monitor-command",
                Some(json!({"command-line": "info usernet"})),
            )
            .await
            .unwrap();
        out.as_str()
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("HOST_FORWARD"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn ok<T>(r: OpResult<T>) -> T {
        r.map_err(|e| format!("{e:?}")).unwrap()
    }

    #[test]
    fn forwards_live_and_saved() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("fw");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "fwd").await;
            // Run the body as a task with a deadline, so a failure or a hang
            // still stops QEMU.
            let body = tokio::spawn(tokio::time::timeout(
                Duration::from_secs(180),
                scenario(sv.clone(), id.clone()),
            ));
            let res = body.await;
            let _ = sv.force_stop(&id).await;
            match res {
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => panic!("{e}"),
                Ok(Err(_)) => panic!("the forwards scenario timed out"),
                Ok(Ok(())) => {}
            }
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }

    async fn scenario(sv: Arc<Supervisor>, id: String) {
        let before = sv.detail(&id).ok().unwrap().spec.nics[0]
            .backend
            .forwards()
            .len();
        let port = free_port();
        let fw = PortForward {
            proto: "tcp".into(),
            host_addr: "127.0.0.1".into(),
            host_port: port,
            guest_port: 8080,
        };

        // Stopped: saved only.
        let c = ok(sv.add_forward(&id, 0, fw.clone()).await);
        assert!(!c.live);
        assert!(
            matches!(sv.add_forward(&id, 0, fw.clone()).await, Err(OpError::Invalid(d)) if d[0].rule == "forward.duplicate"),
            "duplicates are refused"
        );
        ok(sv.remove_forward(&id, 0, before).await);

        // Running: applied live.
        ok(sv.start(&id).await);
        let qmp = sv.live_qmp(&id).unwrap();
        assert!(!usernet(&qmp).await.contains(&port.to_string()));
        let c = ok(sv.add_forward(&id, 0, fw.clone()).await);
        assert!(c.live, "{}", c.message);
        let un = usernet(&qmp).await;
        assert!(un.contains(&port.to_string()), "{un}");
        // The host port really listens.
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("forward listens");
        let d = sv.detail(&id).ok().unwrap();
        assert!(!d.summary.restart_pending, "live and saved stay in step");
        assert_eq!(d.spec.nics[0].backend.forwards().last(), Some(&fw));

        // A port already bound on the host is a clear error.
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = PortForward {
            host_port: held.local_addr().unwrap().port(),
            ..fw.clone()
        };
        let e = sv
            .add_forward(&id, 0, busy)
            .await
            .err()
            .map(|e| format!("{e:?}"))
            .unwrap_or_default();
        assert!(e.contains("forward.bind"), "{e}");
        drop(held);

        // Remove live.
        let c = ok(sv.remove_forward(&id, 0, before).await);
        assert!(c.live, "{}", c.message);
        assert!(!usernet(&qmp).await.contains(&port.to_string()));

        // Persistence: re-added, it is in the next start's argv.
        ok(sv.add_forward(&id, 0, fw.clone()).await);
        ok(sv.force_stop(&id).await);
        wait_state(&sv, &id, VmState::Stopped).await;
        ok(sv.start(&id).await);
        let qmp = sv.live_qmp(&id).unwrap();
        assert!(usernet(&qmp).await.contains(&port.to_string()));

        // Another VM forwarding the same port gets a warning.
        let mut other = sv.detail(&id).ok().unwrap().spec;
        other.name = "other".into();
        let w = sv.forward_conflicts(None, &other);
        assert!(w.iter().any(|d| d.rule == "forward.in-use"), "{w:?}");
        assert!(
            sv.forward_conflicts(Some(&id), &other).is_empty(),
            "own forwards are fine"
        );
        ok(sv.force_stop(&id).await);
        wait_state(&sv, &id, VmState::Stopped).await;

        // A NIC on a NAT network object is a slirp netdev too: live as well.
        let nat = sv
            .networks
            .create("lan", NetworkConfig::Nat { subnet: None })
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        let mut spec = sv.detail(&id).ok().unwrap().spec;
        spec.nics[0].backend = NicBackend::Network {
            network: nat.id.clone(),
            forwards: vec![],
        };
        ok(sv.update(&id, spec).await);
        ok(sv.start(&id).await);
        let qmp = sv.live_qmp(&id).unwrap();
        let c = ok(sv.add_forward(&id, 0, fw.clone()).await);
        assert!(c.live, "{}", c.message);
        assert!(usernet(&qmp).await.contains(&port.to_string()));
        let c = ok(sv.remove_forward(&id, 0, 0).await);
        assert!(c.live, "{}", c.message);
        assert!(!usernet(&qmp).await.contains(&port.to_string()));
        ok(sv.force_stop(&id).await);
        wait_state(&sv, &id, VmState::Stopped).await;
    }
}
