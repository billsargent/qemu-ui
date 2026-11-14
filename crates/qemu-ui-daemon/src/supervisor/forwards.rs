//! Port forwards on slirp NICs (plan.md §10.2, M3-02).
//!
//! A forward is part of the NIC's spec, so a cold start passes it as
//! `-netdev user,hostfwd=…`. On a running VM it is applied live through the
//! HMP `hostfwd_add` / `hostfwd_remove` commands (slirp has no QMP
//! equivalent) and recorded in both the run's spec and the saved spec, the
//! same way disk hotplug is (see `disks.rs`), so it survives a restart and
//! `restart_pending` stays meaningful.
//!
//! HMP reports results as text in a successful reply: `hostfwd_add` is
//! silent on success, `hostfwd_remove` says "… removed"; anything else is
//! QEMU's error message.

use super::*;
use qemu_ui_core::compile::{forwards_overlap, hostfwd_host, hostfwd_rule};
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

/// The slirp forwards of NIC `i`, or why it has none.
fn user_forwards(spec: &mut VmSpec, i: usize) -> OpResult<&mut Vec<PortForward>> {
    let nic = spec.nics.get_mut(i).ok_or(OpError::NotFound)?;
    match &mut nic.backend {
        NicBackend::User { forwards } => Ok(forwards),
        #[allow(unreachable_patterns)]
        _ => Err(OpError::Conflict(
            "port forwards apply to NAT (user-mode) network adapters only".into(),
        )),
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
        let pos = user_forwards(&mut saved, i)?.len();
        let field = format!("nics[{i}].backend.forwards[{pos}]");

        // The same checks as a save, for this forward only.
        let mut check = saved.clone();
        user_forwards(&mut check, i)?.push(fw.clone());
        let errs: Vec<Diagnostic> = compile::validate(&check, &caps)
            .into_iter()
            .filter(|d| d.severity == Severity::Error && d.field.starts_with(&field))
            .collect();
        if !errs.is_empty() {
            return Err(OpError::Invalid(errs));
        }

        let Some(run) = self.active_run(id) else {
            user_forwards(&mut saved, i)?.push(fw);
            self.save_spec(id, &saved)?;
            return Ok(ForwardChange {
                live: false,
                message: "Forward saved; it applies at the next start".into(),
                vm: self.detail(id)?,
            });
        };

        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let live_fwds = user_forwards(&mut live, i).map_err(|_| {
            OpError::Conflict(
                "this adapter changed since the VM started; restart it to add forwards live".into(),
            )
        })?;
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
        user_forwards(&mut saved, i)?.push(fw.clone());
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
        let fwds = user_forwards(&mut saved, i)?;
        if j >= fwds.len() {
            return Err(OpError::NotFound);
        }
        let fw = fwds.remove(j);

        let mut live_applied = false;
        if let Some(run) = self.active_run(id) {
            let mut live: VmSpec =
                serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
            if let Ok(live_fwds) = user_forwards(&mut live, i)
                && let Some(k) = live_fwds.iter().position(|g| *g == fw)
            {
                let qmp = self.qmp_of(id)?;
                hmp(
                    &qmp,
                    &format!("hostfwd_remove qui-net{i} {}", hostfwd_host(&fw)),
                    Some("removed"),
                )
                .await?;
                live_fwds.remove(k);
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
            #[allow(irrefutable_let_patterns)]
            let NicBackend::User { forwards } = &nic.backend else {
                continue;
            };
            for (j, f) in forwards.iter().enumerate() {
                let holder = others.iter().find(|(_, s)| {
                    s.nics.iter().any(|n| {
                        #[allow(irrefutable_let_patterns)]
                        let NicBackend::User { forwards } = &n.backend else {
                            return false;
                        };
                        forwards.iter().any(|g| forwards_overlap(f, g))
                    })
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
            .join("
")
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
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
            // Run the body as a task so a failure still stops QEMU.
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
        {
            let base = sv.detail(&id).ok().unwrap().spec.nics[0].clone();
            let NicBackend::User { forwards } = &base.backend;
            let before = forwards.len();
            let port = free_port();
            let fw = PortForward {
                proto: "tcp".into(),
                host_addr: "127.0.0.1".into(),
                host_port: port,
                guest_port: 8080,
            };

            // Stopped: saved only.
            let c = sv
                .add_forward(&id, 0, fw.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(!c.live);
            assert!(
                matches!(sv.add_forward(&id, 0, fw.clone()).await, Err(OpError::Invalid(d)) if d[0].rule == "forward.duplicate"),
                "duplicates are refused"
            );
            sv.remove_forward(&id, 0, before)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            // Running: applied live.
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let qmp = sv.live_qmp(&id).unwrap();
            assert!(!usernet(&qmp).await.contains(&port.to_string()));
            let c = sv
                .add_forward(&id, 0, fw.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live, "{}", c.message);
            let un = usernet(&qmp).await;
            assert!(un.contains(&port.to_string()), "{un}");
            // The host port really listens.
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("forward listens");
            let d = sv.detail(&id).ok().unwrap();
            assert!(!d.summary.restart_pending, "live and saved stay in step");
            let NicBackend::User { forwards } = &d.spec.nics[0].backend;
            assert_eq!(forwards.last(), Some(&fw));

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
            let c = sv
                .remove_forward(&id, 0, before)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(c.live, "{}", c.message);
            assert!(!usernet(&qmp).await.contains(&port.to_string()));

            // Persistence: re-added, it is in the next start's argv.
            sv.add_forward(&id, 0, fw.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
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

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
        }
    }
}
