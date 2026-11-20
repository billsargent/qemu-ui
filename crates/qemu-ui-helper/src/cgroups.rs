//! Per-VM cgroup v2 groups (M6-05): CPU and memory limits for a running
//! QEMU, applied by the helper because writing cgroup files needs root (or
//! a delegated subtree).
//!
//! Safety rules: the request names a VM id and a pid, never a file. The pid
//! must be a `qemu-system-*` process owned by the daemon user; the group is
//! `<mount>/<parent>/vm-<id>` and only the allowlisted interface files from
//! `qemu_ui_core::limits::cgroup_v2_settings` are written.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qemu_ui_core::helper::{IsolationPolicy, valid_vm_id};
use qemu_ui_core::limits::{ResourceLimits, cgroup_v2_settings};

/// The file operations cgroups need; faked in tests.
pub trait Fs: Send + Sync {
    fn create_dir_all(&self, p: &Path) -> Result<()>;
    fn write(&self, p: &Path, contents: &str) -> Result<()>;
    fn read_to_string(&self, p: &Path) -> Result<String>;
    fn read_link(&self, p: &Path) -> Result<PathBuf>;
    fn remove_dir(&self, p: &Path) -> Result<()>;
}

pub struct HostFs;

impl Fs for HostFs {
    fn create_dir_all(&self, p: &Path) -> Result<()> {
        std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
    }
    fn write(&self, p: &Path, contents: &str) -> Result<()> {
        std::fs::write(p, contents).with_context(|| format!("writing {}", p.display()))
    }
    fn read_to_string(&self, p: &Path) -> Result<String> {
        std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
    }
    fn read_link(&self, p: &Path) -> Result<PathBuf> {
        std::fs::read_link(p).with_context(|| format!("reading {}", p.display()))
    }
    fn remove_dir(&self, p: &Path) -> Result<()> {
        std::fs::remove_dir(p).with_context(|| format!("removing {}", p.display()))
    }
}

pub struct Cgroups {
    fs: Box<dyn Fs>,
    /// The cgroup v2 mount, normally `/sys/fs/cgroup`.
    mount: PathBuf,
    /// `/proc`, overridable in tests.
    proc: PathBuf,
    linux: bool,
}

impl Cgroups {
    pub fn new(fs: Box<dyn Fs>) -> Cgroups {
        Cgroups {
            fs,
            mount: PathBuf::from("/sys/fs/cgroup"),
            proc: PathBuf::from("/proc"),
            linux: cfg!(target_os = "linux"),
        }
    }

    #[cfg(test)]
    fn pretend_linux(mut self) -> Self {
        self.linux = true;
        self
    }

    fn group(&self, policy: &IsolationPolicy, vm_id: &str) -> Result<PathBuf> {
        if !self.linux {
            bail!("per-VM cgroups are Linux-only");
        }
        if !policy.cgroups {
            bail!("per-VM cgroups are disabled; set [isolation] cgroups = true in helper.toml");
        }
        if !valid_vm_id(vm_id) {
            bail!("invalid VM id");
        }
        let parent = policy.cgroup_parent.as_deref().unwrap_or("qemu-ui");
        if parent.is_empty()
            || !parent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            || parent.starts_with('.')
        {
            bail!("invalid cgroup_parent in helper.toml");
        }
        Ok(self.mount.join(parent).join(format!("vm-{vm_id}")))
    }

    /// Is `pid` a QEMU system emulator run by `uid`?
    fn check_pid(&self, pid: u32, uid: Option<u32>) -> Result<()> {
        let dir = self.proc.join(pid.to_string());
        let exe = self
            .fs
            .read_link(&dir.join("exe"))
            .context("no such process")?;
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.starts_with("qemu-system-") {
            bail!("process {pid} is not a QEMU system emulator");
        }
        let Some(want) = uid else {
            bail!("the daemon user is unknown; set daemon_user in helper.toml");
        };
        let status = self.fs.read_to_string(&dir.join("status"))?;
        let real_uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<u32>().ok());
        if real_uid != Some(want) {
            bail!("process {pid} does not belong to the daemon user");
        }
        Ok(())
    }

    /// Create or update the VM's group and move `pid` into it.
    pub fn apply(
        &self,
        policy: &IsolationPolicy,
        vm_id: &str,
        pid: u32,
        uid: Option<u32>,
        limits: &ResourceLimits,
    ) -> Result<String> {
        let group = self.group(policy, vm_id)?;
        self.check_pid(pid, uid)?;
        let parent = group.parent().expect("group has a parent");
        self.fs.create_dir_all(&group)?;
        // Hand the cpu and memory controllers down to the VM groups.
        for dir in [parent.parent().unwrap_or(parent), parent] {
            self.fs
                .write(&dir.join("cgroup.subtree_control"), "+cpu +memory")?;
        }
        // Clear limits that were removed, then set the current ones.
        for (file, default) in [
            ("cpu.max", "max 100000"),
            ("cpu.weight", "100"),
            ("memory.max", "max"),
        ] {
            self.fs.write(&group.join(file), default)?;
        }
        for (file, value) in cgroup_v2_settings(limits) {
            self.fs.write(&group.join(file), &value)?;
        }
        self.fs
            .write(&group.join("cgroup.procs"), &pid.to_string())?;
        Ok(group.display().to_string())
    }

    /// Remove the VM's group (its processes have exited).
    pub fn release(&self, policy: &IsolationPolicy, vm_id: &str) -> Result<()> {
        let group = self.group(policy, vm_id)?;
        match self.fs.remove_dir(&group) {
            Ok(()) => Ok(()),
            // Never created, or already gone.
            Err(_) if self.fs.read_to_string(&group.join("cgroup.procs")).is_err() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// In-memory files; `links` play the role of `/proc/<pid>/exe`.
    #[derive(Default, Clone)]
    pub struct FakeFs {
        pub files: Arc<Mutex<BTreeMap<PathBuf, String>>>,
        pub dirs: Arc<Mutex<Vec<PathBuf>>>,
        pub links: Arc<Mutex<BTreeMap<PathBuf, PathBuf>>>,
    }

    impl Fs for FakeFs {
        fn create_dir_all(&self, p: &Path) -> Result<()> {
            self.dirs.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }
        fn write(&self, p: &Path, c: &str) -> Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(p.to_path_buf(), c.to_string());
            Ok(())
        }
        fn read_to_string(&self, p: &Path) -> Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(p)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no file"))
        }
        fn read_link(&self, p: &Path) -> Result<PathBuf> {
            self.links
                .lock()
                .unwrap()
                .get(p)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no link"))
        }
        fn remove_dir(&self, p: &Path) -> Result<()> {
            let mut d = self.dirs.lock().unwrap();
            let before = d.len();
            d.retain(|x| x != p);
            if d.len() == before {
                bail!("no such directory");
            }
            // Like the kernel: the group's interface files go with it.
            self.files.lock().unwrap().retain(|k, _| !k.starts_with(p));
            Ok(())
        }
    }

    fn process(fs: &FakeFs, pid: u32, exe: &str, uid: u32) {
        let dir = PathBuf::from("/proc").join(pid.to_string());
        fs.links
            .lock()
            .unwrap()
            .insert(dir.join("exe"), PathBuf::from(exe));
        fs.files.lock().unwrap().insert(
            dir.join("status"),
            format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        );
    }

    fn on() -> IsolationPolicy {
        IsolationPolicy {
            cgroups: true,
            cgroup_parent: None,
        }
    }

    fn limits() -> ResourceLimits {
        ResourceLimits {
            cpu_max_percent: Some(150),
            cpu_weight: None,
            memory_max_mib: Some(1024),
            ..ResourceLimits::default()
        }
    }

    #[test]
    fn applies_limits_to_a_qemu_of_the_daemon_user() {
        let fs = FakeFs::default();
        process(&fs, 42, "/usr/bin/qemu-system-x86_64", 1000);
        let cg = Cgroups::new(Box::new(fs.clone())).pretend_linux();
        let path = cg
            .apply(&on(), "abc123", 42, Some(1000), &limits())
            .unwrap();
        assert_eq!(
            PathBuf::from(&path),
            PathBuf::from("/sys/fs/cgroup")
                .join("qemu-ui")
                .join("vm-abc123")
        );
        let files = fs.files.lock().unwrap();
        let g = PathBuf::from(&path);
        assert_eq!(files[&g.join("cpu.max")], "150000 100000");
        assert_eq!(files[&g.join("memory.max")], (1024u64 << 20).to_string());
        assert_eq!(
            files[&g.join("cpu.weight")],
            "100",
            "unset limits are reset"
        );
        assert_eq!(files[&g.join("cgroup.procs")], "42");
        assert_eq!(
            files[&PathBuf::from("/sys/fs/cgroup")
                .join("qemu-ui")
                .join("cgroup.subtree_control")],
            "+cpu +memory"
        );
        drop(files);
        cg.release(&on(), "abc123").unwrap();
        cg.release(&on(), "abc123").unwrap();
    }

    #[test]
    fn refuses_what_it_must() {
        let fs = FakeFs::default();
        process(&fs, 7, "/usr/bin/bash", 1000);
        process(&fs, 8, "/usr/bin/qemu-system-x86_64", 0);
        process(&fs, 9, "/usr/bin/qemu-system-x86_64", 1000);
        let cg = Cgroups::new(Box::new(fs.clone())).pretend_linux();
        let err = |r: Result<String>| format!("{:#}", r.unwrap_err());
        assert!(err(cg.apply(&on(), "a", 7, Some(1000), &limits())).contains("not a QEMU"));
        assert!(err(cg.apply(&on(), "a", 8, Some(1000), &limits())).contains("daemon user"));
        assert!(err(cg.apply(&on(), "a", 99, Some(1000), &limits())).contains("no such process"));
        assert!(err(cg.apply(&on(), "../x", 9, Some(1000), &limits())).contains("invalid VM id"));
        assert!(
            err(cg.apply(&IsolationPolicy::default(), "a", 9, Some(1000), &limits()))
                .contains("disabled")
        );
        let bad = IsolationPolicy {
            cgroups: true,
            cgroup_parent: Some("../../etc".into()),
        };
        assert!(err(cg.apply(&bad, "a", 9, Some(1000), &limits())).contains("cgroup_parent"));
        assert!(
            fs.files
                .lock()
                .unwrap()
                .keys()
                .all(|k| !k.starts_with("/sys")),
            "nothing written"
        );
        let not_linux = Cgroups::new(Box::new(fs));
        if !cfg!(target_os = "linux") {
            assert!(
                err(not_linux.apply(&on(), "a", 9, Some(1000), &limits())).contains("Linux-only")
            );
        }
    }
}
