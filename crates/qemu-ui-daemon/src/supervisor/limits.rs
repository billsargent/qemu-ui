//! Resource limits for running VMs (M6-05/06/07).
//!
//! * Disk IO: the throttle group the compiler adds; changed live with
//!   `qom-set` on `/objects/qui-throttle0`.
//! * CPU and memory: a named job object on Windows (never kill-on-close),
//!   a cgroup v2 group through the privileged helper on Linux. Neither is
//!   required: without them the limits are saved but not enforced, and the
//!   start event and API say so.

use qemu_ui_core::limits::{self, LimitsChange, LimitsSupport, ResourceLimits, THROTTLE_GROUP};
use serde_json::json;

use super::*;

/// The active Linux security module, if any.
fn lsm() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if std::path::Path::new("/sys/kernel/security/apparmor").exists() {
        Some("apparmor".into())
    } else if std::path::Path::new("/sys/fs/selinux/enforce").exists() {
        Some("selinux".into())
    } else {
        None
    }
}

/// Windows: each running VM's job object, kept open so its limits can be
/// changed live (a job's name disappears with its last handle). Dropped when
/// the run ends; after a daemon restart a live change nests a new job, and
/// nested limits only ever tighten.
static JOBS: std::sync::LazyLock<Mutex<HashMap<String, qemu_ui_winsec::Job>>> =
    std::sync::LazyLock::new(Mutex::default);

fn win_limits(l: &ResourceLimits) -> qemu_ui_winsec::JobLimits {
    let jl = limits::job_limits(l, host_cpus());
    qemu_ui_winsec::JobLimits {
        memory_bytes: jl.memory_bytes,
        cpu_rate: jl.cpu_rate,
    }
}

/// Create (or re-limit) the VM's job.
fn set_job(vm_id: &str, pid: u32, l: &ResourceLimits) -> std::io::Result<()> {
    let mut jobs = JOBS.lock().unwrap();
    if let Some(j) = jobs.get(vm_id) {
        return j.set_limits(win_limits(l));
    }
    let j = qemu_ui_winsec::Job::assign(&qemu_ui_winsec::job_name(vm_id), pid, win_limits(l))?;
    jobs.insert(vm_id.to_string(), j);
    Ok(())
}

fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

impl Supervisor {
    pub fn limits_support(&self) -> LimitsSupport {
        if cfg!(windows) {
            LimitsSupport {
                cpu_memory: "job-object".into(),
                reason: None,
                io: true,
                lsm: lsm(),
            }
        } else if cfg!(target_os = "linux") {
            let h = self.helper();
            LimitsSupport {
                cpu_memory: if h.present() { "cgroup" } else { "none" }.into(),
                reason: (!h.present()).then(|| {
                    "CPU and memory limits need the qemu-ui helper with [isolation] cgroups = true".into()
                }),
                io: true,
                lsm: lsm(),
            }
        } else {
            LimitsSupport {
                cpu_memory: "none".into(),
                reason: Some("CPU and memory limits are not supported on this OS".into()),
                io: true,
                lsm: lsm(),
            }
        }
    }

    /// Apply CPU/memory limits to a freshly started QEMU. Never fails the
    /// start: returns what happened, for the start event.
    pub(super) async fn enforce_limits(
        &self,
        vm_id: &str,
        pid: u32,
        l: &ResourceLimits,
    ) -> Option<String> {
        if !l.needs_host_enforcement() {
            return None;
        }
        let r: Result<String, String> = if cfg!(windows) {
            JOBS.lock().unwrap().remove(vm_id); // a stale job from an earlier run
            set_job(vm_id, pid, l)
                .map(|_| "CPU and memory limits applied (job object)".to_string())
                .map_err(|e| e.to_string())
        } else if cfg!(target_os = "linux") {
            self.helper()
                .apply_vm_limits(vm_id, pid, l)
                .await
                .map(|g| format!("CPU and memory limits applied (cgroup {g})"))
                .map_err(|e| format!("{e:#}"))
        } else {
            Err("not supported on this OS".into())
        };
        Some(match r {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(vm = %vm_id, "CPU/memory limits not enforced: {e}");
                format!("CPU and memory limits are saved but not enforced: {e}")
            }
        })
    }

    /// The run ended: give back the VM's cgroup (Linux). Job objects go
    /// away on their own when their last process exits.
    pub(super) fn release_limits(&self, vm_id: &str) {
        JOBS.lock().unwrap().remove(vm_id);
        if !cfg!(target_os = "linux") {
            return;
        }
        let helper = self.helper();
        if !helper.present() {
            return;
        }
        let vm = vm_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = helper.release_vm_limits(&vm).await {
                tracing::debug!(vm = %vm, "releasing cgroup: {e:#}");
            }
        });
    }

    /// Change a VM's limits: saved always, and live where possible.
    pub async fn set_limits(&self, id: &str, new: ResourceLimits) -> OpResult<LimitsChange> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let mut saved: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let errs = limits::validate(&new, saved.cpu.count, saved.memory.size_mib);
        if !errs.is_empty() {
            return Err(OpError::Invalid(errs));
        }
        saved.limits = new.clone();
        let saved_json = serde_json::to_string(&saved).map_err(|e| OpError::Failed(e.into()))?;

        let Some(run) = self.active_run(id) else {
            self.store
                .update_vm_spec(id, &saved.name, &saved_json, now())?;
            return Ok(LimitsChange {
                live: false,
                message: "Limits saved; they apply at the next start".into(),
                vm: self.detail(id)?,
            });
        };

        let mut live: VmSpec =
            serde_json::from_str(&run.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let mut notes = Vec::new();
        let mut all_live = true;

        // Disk IO: only when the run has a throttle group to change.
        if new.io != live.limits.io {
            if live.limits.io.is_set() {
                let qmp = self
                    .live_qmp(id)
                    .ok_or_else(|| OpError::Conflict("the VM is not running".into()))?;
                qmp.execute(
                    "qom-set",
                    Some(json!({
                        "path": format!("/objects/{THROTTLE_GROUP}"),
                        "property": "limits",
                        "value": new.io.qemu_limits(),
                    })),
                )
                .await
                .map_err(|e| OpError::Failed(anyhow!("updating the IO limits: {e}")))?;
                live.limits.io = new.io.clone();
                notes.push("disk IO limits updated".to_string());
            } else {
                all_live = false;
                notes.push("disk IO limits apply at the next start".to_string());
            }
        }

        // CPU and memory.
        let cpu_mem_changed = (new.cpu_max_percent, new.cpu_weight, new.memory_max_mib)
            != (
                live.limits.cpu_max_percent,
                live.limits.cpu_weight,
                live.limits.memory_max_mib,
            );
        if cpu_mem_changed {
            let r: Result<(), String> = if cfg!(windows) {
                set_job(id, run.pid, &new).map_err(|e| e.to_string())
            } else if cfg!(target_os = "linux") {
                self.helper()
                    .apply_vm_limits(id, run.pid, &new)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            } else {
                Err("not supported on this OS".into())
            };
            match r {
                Ok(()) => {
                    live.limits.cpu_max_percent = new.cpu_max_percent;
                    live.limits.cpu_weight = new.cpu_weight;
                    live.limits.memory_max_mib = new.memory_max_mib;
                    notes.push("CPU and memory limits updated".into());
                }
                Err(e) => {
                    all_live = false;
                    notes.push(format!("CPU and memory limits saved but not enforced: {e}"));
                }
            }
        }

        let live_json = serde_json::to_string(&live).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_run_spec(run.id, &live_json)?;
        self.store
            .update_vm_spec(id, &saved.name, &saved_json, now())?;
        self.emit(
            id,
            "limits-changed",
            json!({"limits": new, "live": all_live}),
        );
        Ok(LimitsChange {
            live: all_live,
            message: if notes.is_empty() {
                "No change".into()
            } else {
                notes.join("; ")
            },
            vm: self.detail(id)?,
        })
    }
}

#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;
    use qemu_ui_core::limits::IoLimits;

    async fn throttle_limits(sv: &Supervisor, id: &str) -> serde_json::Value {
        sv.live_qmp(id)
            .unwrap()
            .execute(
                "qom-get",
                Some(json!({"path": format!("/objects/{THROTTLE_GROUP}"), "property": "limits"})),
            )
            .await
            .unwrap()
    }

    #[test]
    fn io_limits_are_applied_at_start_and_changed_live() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("lim");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "limited").await;
            let mut l = ResourceLimits {
                io: IoLimits {
                    read_bytes_per_sec: Some(8 << 20),
                    ..IoLimits::default()
                },
                ..ResourceLimits::default()
            };
            // Stopped: saved only.
            let c = sv
                .set_limits(&id, l.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(!c.live);

            let scenario = async {
                sv.start(&id).await.map_err(|e| format!("{e:?}"))?;
                let v = throttle_limits(&sv, &id).await;
                if v["bps-read"] != 8 << 20 {
                    return Err(format!("at start: {v}"));
                }
                l.io.read_bytes_per_sec = Some(2 << 20);
                l.io.write_iops = Some(300);
                let c = sv
                    .set_limits(&id, l.clone())
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                if !c.live {
                    return Err(format!("not live: {}", c.message));
                }
                let v = throttle_limits(&sv, &id).await;
                if v["bps-read"] != 2 << 20 || v["iops-write"] != 300 {
                    return Err(format!("after update: {v}"));
                }
                // Invalid limits are refused and change nothing.
                let bad = ResourceLimits {
                    memory_max_mib: Some(1),
                    ..l.clone()
                };
                if sv.set_limits(&id, bad).await.is_ok() {
                    return Err("accepted a memory limit below guest RAM".into());
                }
                Ok(())
            };
            let out = tokio::time::timeout(Duration::from_secs(180), scenario).await;
            let _ = sv.force_stop(&id).await;
            wait_state(&sv, &id, VmState::Stopped).await;
            out.expect("timed out").unwrap();
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[cfg(windows)]
    fn job_limits_of(id: &str) -> Result<qemu_ui_winsec::JobLimits, String> {
        JOBS.lock()
            .unwrap()
            .get(id)
            .ok_or("no job for the VM")?
            .limits()
            .map_err(|e| e.to_string())
    }

    /// Windows: CPU/memory limits put QEMU in a job; the VM keeps running.
    #[cfg(windows)]
    #[test]
    fn cpu_memory_limits_use_a_job_object() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("job");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "jobbed").await;
            let l = ResourceLimits {
                memory_max_mib: Some(2048),
                cpu_max_percent: Some(100),
                ..ResourceLimits::default()
            };
            sv.set_limits(&id, l.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let scenario = async {
                sv.start(&id).await.map_err(|e| format!("{e:?}"))?;
                let got = job_limits_of(&id)?;
                if got.memory_bytes != Some(2048 << 20) || got.cpu_rate.is_none() {
                    return Err(format!("job limits {got:?}"));
                }
                let lower = ResourceLimits {
                    memory_max_mib: Some(1024),
                    ..l.clone()
                };
                let c = sv
                    .set_limits(&id, lower)
                    .await
                    .map_err(|e| format!("{e:?}"))?;
                let got = job_limits_of(&id)?;
                if !c.live || got.memory_bytes != Some(1024 << 20) {
                    return Err(format!("live update: {} {got:?}", c.message));
                }
                Ok(())
            };
            let out = tokio::time::timeout(Duration::from_secs(180), scenario).await;
            let _ = sv.force_stop(&id).await;
            wait_state(&sv, &id, VmState::Stopped).await;
            out.expect("timed out").unwrap();
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
