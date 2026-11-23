//! Autostart with a crash-loop breaker (plan.md §7.2, M7-08).
//!
//! A VM with autostart on is started when the daemon starts (unless it is
//! still running from before, which reattach handles; a suspended one is
//! resumed), and restarted after it crashes. Restarts back off
//! exponentially (`base`, 2×, 4×, … up to `max_delay`). When it crashes
//! `max_crashes` times within `window`, the breaker opens: no more restarts
//! until somebody starts it by hand or turns autostart off and on again.
//!
//! A clean stop (asked for, or the guest powering off) is not a crash.
//! Crash history lives in memory: a daemon restart gives a looping VM a
//! fresh set of attempts, and the breaker still trips after `max_crashes`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qemu_ui_core::vm::{VmState, VmSummary};
use serde_json::json;

use super::{OpError, OpResult, Supervisor, now};

#[derive(Debug, Clone, Copy)]
pub struct AutostartPolicy {
    pub base: Duration,
    pub max_delay: Duration,
    pub window: Duration,
    pub max_crashes: usize,
}

impl Default for AutostartPolicy {
    fn default() -> Self {
        AutostartPolicy {
            base: Duration::from_secs(2),
            max_delay: Duration::from_secs(300),
            window: Duration::from_secs(600),
            max_crashes: 5,
        }
    }
}

impl AutostartPolicy {
    /// Delay before restart attempt `n` (1-based).
    pub fn delay(&self, n: usize) -> Duration {
        let shift = n.saturating_sub(1).min(20) as u32;
        self.base
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }
}

#[derive(Default)]
pub(super) struct Restarts {
    crashes: VecDeque<Instant>,
    /// Epoch seconds of the scheduled restart.
    retry_at: Option<u64>,
    tripped: bool,
    /// Bumped to cancel a scheduled restart.
    generation: u64,
}

/// What `crashed` decided.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Nothing,
    Restart { after: Duration, generation: u64 },
    Tripped(usize),
}

impl Supervisor {
    pub fn set_autostart_policy(&self, p: AutostartPolicy) {
        *self.autostart_policy.lock().unwrap() = p;
    }

    /// (scheduled restart, breaker open) for the summary.
    pub(super) fn restart_info(&self, id: &str) -> (Option<u64>, bool) {
        self.restarts
            .lock()
            .unwrap()
            .get(id)
            .map_or((None, false), |r| (r.retry_at, r.tripped))
    }

    /// Forget crash history and cancel a scheduled restart.
    pub(super) fn reset_restarts(&self, id: &str) {
        let mut all = self.restarts.lock().unwrap();
        let r = all.entry(id.to_string()).or_default();
        r.crashes.clear();
        r.retry_at = None;
        r.tripped = false;
        r.generation += 1;
    }

    fn crash_count(&self, id: &str) -> usize {
        self.restarts
            .lock()
            .unwrap()
            .get(id)
            .map_or(0, |r| r.crashes.len())
    }

    pub async fn set_autostart(&self, id: &str, on: bool) -> OpResult<VmSummary> {
        self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        self.store.set_autostart(id, on)?;
        self.reset_restarts(id);
        self.emit(id, "autostart", json!({"enabled": on}));
        self.summary_of(id).await
    }

    /// Restart autostart VMs after crashes from now on. Call before
    /// `reattach_all`, so a VM that died while the daemon was down is
    /// restarted too.
    pub fn enable_restarts(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
    }

    /// Start the autostart VMs that are not running (after `reattach_all`:
    /// VMs with an open run are left to reattach).
    pub async fn autostart_all(self: Arc<Self>) {
        let runs = self.store.open_runs().unwrap_or_default();
        for row in self.store.list_vms().unwrap_or_default() {
            if !row.autostart || runs.iter().any(|r| r.vm_id == row.id) {
                continue;
            }
            let state = VmState::parse(&row.state);
            if state.is_active() {
                continue;
            }
            tracing::info!(vm = %row.name, "autostart");
            self.attempt(&row.id).await;
        }
    }

    /// One (re)start attempt. A failure before QEMU ran counts as a crash
    /// here; one after it ran was already counted by `finish`.
    async fn attempt(self: &Arc<Self>, id: &str) {
        let before = self.crash_count(id);
        if let Err(e) = self.start_inner(id).await {
            let msg = match &e {
                OpError::Invalid(d) => format!("{} problem(s) in the configuration", d.len()),
                // Already running (e.g. reattached meanwhile), or deleted.
                OpError::NotFound | OpError::Conflict(_) => return,
                OpError::Failed(e) => format!("{e:#}"),
            };
            tracing::warn!(vm = %id, "autostart failed: {msg}");
            if self.crash_count(id) == before {
                self.crashed(id);
            }
        }
    }

    /// The VM crashed (or failed to start). Schedule a restart if autostart
    /// is on, or open the breaker.
    pub(super) fn crashed(&self, id: &str) {
        let Some(row) = self.store.get_vm(id).ok().flatten() else {
            return;
        };
        if !row.autostart {
            return;
        }
        let policy = *self.autostart_policy.lock().unwrap();
        let next = {
            let mut all = self.restarts.lock().unwrap();
            let r = all.entry(id.to_string()).or_default();
            if r.tripped {
                Next::Nothing
            } else {
                let t = Instant::now();
                while r
                    .crashes
                    .front()
                    .is_some_and(|c| t.duration_since(*c) > policy.window)
                {
                    r.crashes.pop_front();
                }
                r.crashes.push_back(t);
                let n = r.crashes.len();
                r.generation += 1;
                if n >= policy.max_crashes {
                    r.tripped = true;
                    r.retry_at = None;
                    Next::Tripped(n)
                } else {
                    let after = policy.delay(n);
                    r.retry_at = Some(now() + after.as_secs().max(1));
                    Next::Restart {
                        after,
                        generation: r.generation,
                    }
                }
            }
        };
        match next {
            Next::Nothing => {}
            Next::Tripped(n) => {
                let msg = format!(
                    "crash loop: crashed {n} times within {} minutes; autostart gave up. \
                     Start it by hand to try again.",
                    policy.window.as_secs().div_ceil(60)
                );
                tracing::warn!(vm = %row.name, "{msg}");
                let _ = self.store.set_vm_state(id, "crashed", None, Some(&msg));
                self.emit(id, "crash-loop", json!({"crashes": n, "message": msg}));
            }
            Next::Restart { after, generation } => {
                self.emit(
                    id,
                    "restart-scheduled",
                    json!({"in_ms": after.as_millis() as u64}),
                );
                let Some(me) = self.me.get().and_then(|w| w.upgrade()) else {
                    return; // background work not started (tests, tools)
                };
                let id = id.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let current = {
                        let mut all = me.restarts.lock().unwrap();
                        let r = all.entry(id.clone()).or_default();
                        let current = r.generation == generation && !r.tripped;
                        if current {
                            r.retry_at = None;
                        }
                        current
                    };
                    let still = me.store.get_vm(&id).ok().flatten().is_some_and(|v| {
                        v.autostart && VmState::parse(&v.state) == VmState::Crashed
                    });
                    if current && still {
                        tracing::info!(vm = %id, "restarting after a crash");
                        me.attempt(&id).await;
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let p = AutostartPolicy::default();
        assert_eq!(p.delay(1), Duration::from_secs(2));
        assert_eq!(p.delay(2), Duration::from_secs(4));
        assert_eq!(p.delay(4), Duration::from_secs(16));
        assert_eq!(p.delay(9), Duration::from_secs(300));
        assert_eq!(p.delay(100), Duration::from_secs(300));
    }
}

/// Autostart, crash restarts and the breaker against a real QEMU.
#[cfg(test)]
mod real {
    use super::super::test_support::*;
    use super::*;
    use crate::procs::{self, ProcId};

    fn pid_of(sv: &Supervisor, id: &str) -> ProcId {
        let r = sv.store.last_run(id).unwrap().unwrap();
        assert!(r.ended_at.is_none());
        ProcId {
            pid: r.pid,
            start_time: r.proc_start,
        }
    }

    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..300 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn crashes_restart_with_backoff_until_the_breaker_opens() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("auto");
        let _kill = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            sv.set_autostart_policy(AutostartPolicy {
                base: Duration::from_millis(200),
                max_delay: Duration::from_secs(1),
                window: Duration::from_secs(120),
                max_crashes: 3,
            });
            sv.enable_restarts();
            let id = small_vm(&sv, &caps, "phoenix").await;
            let err = |e: OpError| format!("{e:?}");

            // Daemon start: an autostart VM that is not running is started.
            let s = sv.set_autostart(&id, true).await.map_err(err).unwrap();
            assert!(s.autostart);
            sv.clone().autostart_all().await;
            wait_state(&sv, &id, VmState::Running).await;

            // Two crashes: each is followed by a restart.
            for n in 1..=2 {
                let p = pid_of(&sv, &id);
                procs::terminate(p, true);
                wait_until(&format!("restart {n}"), || {
                    sv.store
                        .last_run(&id)
                        .ok()
                        .flatten()
                        .is_some_and(|r| r.pid != p.pid && r.ended_at.is_none())
                })
                .await;
                wait_state(&sv, &id, VmState::Running).await;
            }

            // The third crash within the window opens the breaker.
            procs::terminate(pid_of(&sv, &id), true);
            wait_until("the breaker", || {
                sv.detail(&id).ok().is_some_and(|d| d.summary.crash_loop)
            })
            .await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let s = sv.detail(&id).ok().unwrap().summary;
            assert_eq!(s.state, VmState::Crashed, "no restart after the breaker");
            assert!(s.last_error.unwrap_or_default().contains("crash loop"));
            assert_eq!(s.restart_at, None);

            // A start by hand clears it; a clean stop is not a crash.
            sv.start(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Running).await;
            assert!(!sv.detail(&id).ok().unwrap().summary.crash_loop);
            sv.force_stop(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            assert_eq!(
                sv.detail(&id).ok().unwrap().summary.state,
                VmState::Stopped,
                "stopped on purpose: no restart"
            );

            // Autostart off: a crash stays a crash.
            sv.set_autostart(&id, false).await.map_err(err).unwrap();
            sv.start(&id).await.map_err(err).unwrap();
            wait_state(&sv, &id, VmState::Running).await;
            procs::terminate(pid_of(&sv, &id), true);
            wait_state(&sv, &id, VmState::Crashed).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            assert_eq!(sv.detail(&id).ok().unwrap().summary.state, VmState::Crashed);
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
