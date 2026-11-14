//! Tap devices for bridged and host-only networks (M3-05, Linux).
//!
//! The commands are built in `qemu_ui_core::hostnet` (pure, tested); this
//! module runs them through a `Runner`, which tests replace with a fake so
//! no test ever touches the host's interfaces. Taps the helper created are
//! recorded in its state directory: only those can be deleted, and a stale
//! one left by a crash is replaced rather than refused.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::helper::{NetworkPolicy, TapInfo, TapNetwork, check_tap};
use qemu_ui_core::hostnet::{
    create_tap_commands, delete_tap_commands, host_only_bridge_commands, is_tap_name, tap_name,
};

/// Runs host commands and inspects interfaces.
pub trait Runner: Send + Sync {
    fn run(&self, argv: &[String]) -> Result<()>;
    fn exists(&self, ifname: &str) -> bool;
}

/// The real thing: `ip` with a null stdin, interfaces from `/sys/class/net`.
pub struct HostRunner;

impl Runner for HostRunner {
    fn run(&self, argv: &[String]) -> Result<()> {
        let (prog, args) = argv.split_first().context("empty command")?;
        let out = std::process::Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("running {prog}"))?;
        if !out.status.success() {
            bail!(
                "{}: {}",
                argv.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn exists(&self, ifname: &str) -> bool {
        std::path::Path::new("/sys/class/net").join(ifname).exists()
    }
}

pub struct Taps {
    runner: Box<dyn Runner>,
    state: PathBuf,
    created: Mutex<BTreeSet<String>>,
    /// Taps are a Linux feature; tests override this to exercise the logic.
    linux: bool,
}

impl Taps {
    pub fn open(state: PathBuf, runner: Box<dyn Runner>) -> Taps {
        let created = std::fs::read_to_string(&state)
            .ok()
            .and_then(|s| serde_json::from_str::<BTreeSet<String>>(&s).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|n| is_tap_name(n))
            .collect();
        Taps {
            runner,
            state,
            created: Mutex::new(created),
            linux: cfg!(target_os = "linux"),
        }
    }

    #[cfg(test)]
    pub fn pretend_linux(mut self) -> Self {
        self.linux = true;
        self
    }

    fn save(&self, set: &BTreeSet<String>) {
        if let Ok(s) = serde_json::to_string(set) {
            let _ = std::fs::write(&self.state, s);
        }
    }

    fn run_all(&self, cmds: Vec<Vec<String>>) -> Result<()> {
        for c in cmds {
            self.runner.run(&c)?;
        }
        Ok(())
    }

    pub fn create(
        &self,
        policy: &NetworkPolicy,
        vm_id: &str,
        nic: u32,
        network: &TapNetwork,
        multi_queue: bool,
        owner_uid: Option<u32>,
    ) -> Result<TapInfo> {
        if !self.linux {
            bail!(
                "tap devices are created by the helper on Linux only; on Windows, attach a TAP-Windows \
                 adapter (TAP network) instead"
            );
        }
        let bridge = check_tap(policy, network).map_err(|e| anyhow!(e))?;
        let ifname = tap_name(vm_id, nic).ok_or_else(|| anyhow!("invalid VM id or NIC index"))?;
        let uid = owner_uid.ok_or_else(|| {
            anyhow!("the helper does not know the daemon's user; set daemon_user in helper.toml")
        })?;
        let mut created = self.created.lock().unwrap();
        if self.runner.exists(&ifname) {
            if !created.contains(&ifname) {
                bail!("interface '{ifname}' exists and was not created by the helper");
            }
            // Left over from a run that ended without cleanup.
            self.run_all(delete_tap_commands(&ifname))?;
        }
        match network {
            TapNetwork::HostOnly => {
                if !self.runner.exists(&bridge) {
                    let addr = policy
                        .host_only
                        .as_ref()
                        .map(|h| h.address.clone())
                        .unwrap_or_default();
                    self.run_all(host_only_bridge_commands(&bridge, &addr))
                        .context("creating the host-only bridge")?;
                }
            }
            TapNetwork::Bridge { .. } => {
                if !self.runner.exists(&bridge) {
                    bail!("bridge '{bridge}' does not exist on this host");
                }
            }
        }
        if let Err(e) = self.run_all(create_tap_commands(&ifname, uid, multi_queue, &bridge)) {
            let _ = self.run_all(delete_tap_commands(&ifname));
            return Err(e);
        }
        created.insert(ifname.clone());
        self.save(&created);
        Ok(TapInfo { ifname, bridge })
    }

    pub fn delete(&self, ifname: &str) -> Result<()> {
        let mut created = self.created.lock().unwrap();
        if !created.contains(ifname) {
            bail!("'{ifname}' is not a tap this helper created");
        }
        if self.runner.exists(ifname) {
            self.run_all(delete_tap_commands(ifname))?;
        }
        created.remove(ifname);
        self.save(&created);
        Ok(())
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Arc;

    /// Records commands and keeps a set of interfaces in memory.
    #[derive(Default, Clone)]
    pub struct FakeRunner {
        pub log: Arc<Mutex<Vec<String>>>,
        pub ifaces: Arc<Mutex<BTreeSet<String>>>,
        pub fail_on: Option<String>,
    }

    impl Runner for FakeRunner {
        fn run(&self, argv: &[String]) -> Result<()> {
            let line = argv.join(" ");
            self.log.lock().unwrap().push(line.clone());
            if self.fail_on.as_ref().is_some_and(|f| line.contains(f)) {
                bail!("{line}: simulated failure");
            }
            let mut ifs = self.ifaces.lock().unwrap();
            match argv
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["ip", "tuntap", "add", "dev", name, ..]
                | ["ip", "link", "add", "name", name, ..] => {
                    ifs.insert((*name).into());
                }
                ["ip", "link", "delete", "dev", name] => {
                    ifs.remove(*name);
                }
                _ => {}
            }
            Ok(())
        }

        fn exists(&self, ifname: &str) -> bool {
            self.ifaces.lock().unwrap().contains(ifname)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeRunner;
    use super::*;
    use qemu_ui_core::helper::HostOnlyPolicy;

    fn policy() -> NetworkPolicy {
        NetworkPolicy {
            bridges: vec!["br0".into()],
            host_only: Some(HostOnlyPolicy {
                bridge: "qui-hostonly".into(),
                address: "10.213.0.1/24".into(),
            }),
        }
    }

    fn taps(r: &FakeRunner, dir: &std::path::Path) -> Taps {
        Taps::open(dir.join("taps.json"), Box::new(r.clone())).pretend_linux()
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "quit-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bridged_tap_lifecycle() {
        let dir = tmp();
        let r = FakeRunner::default();
        r.ifaces.lock().unwrap().insert("br0".into());
        let t = taps(&r, &dir);
        let br = TapNetwork::Bridge {
            bridge: "br0".into(),
        };
        let info = t
            .create(&policy(), "0a1b2c3d", 1, &br, false, Some(1000))
            .unwrap();
        assert_eq!(
            info,
            TapInfo {
                ifname: "qui-0a1b2c3d-1".into(),
                bridge: "br0".into()
            }
        );
        assert_eq!(
            *r.log.lock().unwrap(),
            [
                "ip tuntap add dev qui-0a1b2c3d-1 mode tap user 1000",
                "ip link set dev qui-0a1b2c3d-1 master br0",
                "ip link set dev qui-0a1b2c3d-1 up",
            ]
        );
        // Recorded across restarts; deletion only of our own taps.
        let t2 = taps(&r, &dir);
        assert!(
            t2.delete("br0")
                .unwrap_err()
                .to_string()
                .contains("not a tap")
        );
        t2.delete("qui-0a1b2c3d-1").unwrap();
        assert!(!r.exists("qui-0a1b2c3d-1"));
        assert!(t2.delete("qui-0a1b2c3d-1").is_err(), "gone from the record");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refusals_and_cleanup() {
        let dir = tmp();
        let r = FakeRunner::default();
        let t = taps(&r, &dir);
        let br = TapNetwork::Bridge {
            bridge: "br0".into(),
        };
        // Allowlisted but missing on the host.
        assert!(
            t.create(&policy(), "0a1b2c3d", 0, &br, false, Some(1))
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        // Not allowlisted.
        let other = TapNetwork::Bridge {
            bridge: "br9".into(),
        };
        assert!(
            t.create(&policy(), "0a1b2c3d", 0, &other, false, Some(1))
                .is_err()
        );
        // No owner known.
        r.ifaces.lock().unwrap().insert("br0".into());
        assert!(
            t.create(&policy(), "0a1b2c3d", 0, &br, false, None)
                .is_err()
        );
        // Bad VM id cannot smuggle a name.
        assert!(
            t.create(&policy(), "../../x", 0, &br, false, Some(1))
                .is_err()
        );
        // A foreign interface with our name is never touched.
        r.ifaces.lock().unwrap().insert("qui-0a1b2c3d-0".into());
        assert!(
            t.create(&policy(), "0a1b2c3d", 0, &br, false, Some(1))
                .unwrap_err()
                .to_string()
                .contains("not created by the helper")
        );
        assert!(r.log.lock().unwrap().is_empty(), "nothing ran");

        // A failing step rolls the tap back.
        let r = FakeRunner {
            fail_on: Some("master".into()),
            ..Default::default()
        };
        r.ifaces.lock().unwrap().insert("br0".into());
        let t = taps(&r, &dir);
        assert!(
            t.create(&policy(), "0a1b2c3d", 2, &br, true, Some(1))
                .is_err()
        );
        assert!(!r.exists("qui-0a1b2c3d-2"));
        assert!(r.log.lock().unwrap()[0].ends_with("multi_queue"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn host_only_bridge_is_created_once() {
        let dir = tmp();
        let r = FakeRunner::default();
        let t = taps(&r, &dir);
        t.create(
            &policy(),
            "0a1b2c3d",
            0,
            &TapNetwork::HostOnly,
            false,
            Some(5),
        )
        .unwrap();
        t.create(
            &policy(),
            "0a1b2c3e",
            0,
            &TapNetwork::HostOnly,
            false,
            Some(5),
        )
        .unwrap();
        let log = r.log.lock().unwrap();
        assert_eq!(log.iter().filter(|l| l.contains("type bridge")).count(), 1);
        assert!(log.contains(&"ip addr add 10.213.0.1/24 dev qui-hostonly".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_own_tap_is_replaced() {
        let dir = tmp();
        let r = FakeRunner::default();
        r.ifaces.lock().unwrap().insert("br0".into());
        let t = taps(&r, &dir);
        let br = TapNetwork::Bridge {
            bridge: "br0".into(),
        };
        t.create(&policy(), "0a1b2c3d", 0, &br, false, Some(1))
            .unwrap();
        t.create(&policy(), "0a1b2c3d", 0, &br, false, Some(1))
            .unwrap();
        assert!(
            r.log
                .lock()
                .unwrap()
                .contains(&"ip link delete dev qui-0a1b2c3d-0".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
