//! VM supervisor (plan.md §7, M1-05 … M1-09).
//!
//! Owns every QEMU process: compiles the spec, spawns QEMU detached with
//! `-S`, connects to its QMP monitor over AF_UNIX, checks it, sends `cont`,
//! then follows its events until it exits. On daemon start, open runs are
//! re-attached: identity is checked (pid + start time), the monitor is
//! reconnected, and state is reconciled with `query-status`.
//!
//! Killing or restarting the daemon never kills a VM.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::buses::MachineBuses;
use qemu_ui_core::caps::BinaryCaps;
use qemu_ui_core::compile::{
    self, Compiled, Diagnostic, ResolvedFirmware, ResolvedImage, RunPaths, Severity, VncListen,
};
use qemu_ui_core::lint::{self, LintResult};
use qemu_ui_core::spec::{
    FirmwareSpec, ImageRef, Media, NicBackend, VmSpec, disk_node, valid_name,
};
use qemu_ui_core::vm::{VmDetail, VmEvent, VmState, VmSummary, mac_from_bytes, state_from_qmp};
use qemu_ui_qmp::{Qmp, transport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::images::Pools;
use crate::procs::{self, ProcId};
use crate::scan::Scanner;
use crate::store::{NewRun, RunRow, Store};

mod autostart;
mod disks;
mod forwards;
mod hardware;
mod limits;
mod nics;
mod ops;
mod rules;
mod snapshots;
mod suspend;
mod taps;

pub use autostart::AutostartPolicy;

const QMP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// How a run's VNC server is reached; stored with the run for the proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum VncInfo {
    Unix { path: String },
    Tcp { port: u16, password: String },
}

struct Live {
    run_id: i64,
    qmp: Option<Qmp>,
    proc_id: ProcId,
    /// A stop was requested (graceful or forced): exit is expected.
    stop_requested: bool,
    /// QEMU reported SHUTDOWN (guest powered off): exit is expected.
    shutdown_seen: bool,
    /// The state was saved to disk: the exit ends in `suspended` (M7-01).
    suspending: bool,
}

pub struct Supervisor {
    pub store: Store,
    pub scanner: Arc<Scanner>,
    pub pools: Arc<Pools>,
    pub jobs: Arc<crate::jobs::Jobs>,
    /// Registered share roots for shared folders (M2-13).
    pub shares: crate::shares::ShareRoots,
    /// Network objects NICs attach to (M3-01).
    pub networks: crate::networks::Networks,
    /// Switches of isolated networks, started with their first VM (M3-04).
    pub switches: crate::netswitch::Switches,
    /// `virtiofsd`, when installed: writable shares then use virtiofs.
    pub virtiofsd: Option<PathBuf>,
    /// `passt`, when installed (Linux): passt NAT networks need it (M3-03).
    pub passt: Option<PathBuf>,
    /// `swtpm`, when installed: emulated TPMs need it (M7-07).
    pub swtpm: Option<PathBuf>,
    data_dir: PathBuf,
    run_root: PathBuf,
    events: broadcast::Sender<VmEvent>,
    live: Mutex<HashMap<String, Live>>,
    /// Serialises lifecycle operations (start/stop/delete/edit).
    ops: tokio::sync::Mutex<()>,
    /// Dry-lint results by (binary, lint spec) hash.
    lint_cache: Mutex<HashMap<String, LintResult>>,
    /// The privileged helper, for physical host disks (M2-11).
    helper: std::sync::RwLock<crate::helper_client::HelperClient>,
    /// Machine buses by (binary sha256, machine), probed lazily (M2-09).
    bus_cache: Mutex<HashMap<(String, String), Option<MachineBuses>>>,
    /// Restart backoff and crash-loop breaker settings (M7-08).
    autostart_policy: Mutex<AutostartPolicy>,
    /// Crash history and scheduled restarts per VM.
    restarts: Mutex<HashMap<String, autostart::Restarts>>,
    /// Ourselves, for spawning restarts from `finish`; set by
    /// `start_background`.
    me: std::sync::OnceLock<std::sync::Weak<Supervisor>>,
}

/// How `clone_vm` copies writable disks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloneMode {
    /// qcow2 overlays on template disks: instant, shares the base.
    Linked,
    /// Independent copies, made by a job.
    Full,
}

/// A linked clone is ready at once; a full clone runs as a job.
pub enum CloneOutcome {
    Vm(Box<VmDetail>),
    Job(Box<qemu_ui_core::storage::Job>),
}

/// What `build` is compiling for.
enum BuildMode {
    /// An actual start: creates the NVRAM copy, allocates a VNC endpoint.
    Run,
    /// The live argv drawer: no side effects.
    Preview,
    /// "Check config": scratch disks, NVRAM, sockets in this directory.
    Lint(PathBuf),
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

/// Error returned to the API: either diagnostics (422) or a message.
#[derive(Debug)]
pub enum OpError {
    Invalid(Vec<Diagnostic>),
    Conflict(String),
    NotFound,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for OpError {
    fn from(e: anyhow::Error) -> Self {
        OpError::Failed(e)
    }
}

pub type OpResult<T> = std::result::Result<T, OpError>;

impl Supervisor {
    pub fn new(
        store: Store,
        scanner: Arc<Scanner>,
        data_dir: &Path,
        run_root: PathBuf,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&run_root)?;
        // Longest socket path we create: <run_root>/<8-char id>/qmp-user.sock
        let probe = run_root.join("00000000").join("qmp-user.sock");
        transport::check_unix_path(&probe).with_context(|| {
            format!(
                "run directory {} is too long for unix sockets; pass a shorter --run-dir",
                run_root.display()
            )
        })?;
        let (events, _) = broadcast::channel(1024);
        let jobs = crate::jobs::Jobs::new(store.clone(), events.clone(), data_dir.join("jobs"))?;
        Ok(Arc::new(Supervisor {
            pools: Arc::new(Pools::new(data_dir, store.clone())?),
            shares: crate::shares::ShareRoots::new(store.clone()),
            networks: crate::networks::Networks::new(store.clone()),
            switches: crate::netswitch::Switches::new(run_root.clone()),
            passt: {
                let p = crate::passt::find_passt();
                if let Some(p) = &p {
                    tracing::info!("passt found at {}; passt networks available", p.display());
                }
                p
            },
            swtpm: {
                let p = crate::swtpm::find_swtpm();
                if let Some(p) = &p {
                    tracing::info!("swtpm found at {}; emulated TPMs available", p.display());
                }
                p
            },
            virtiofsd: {
                let v = crate::shares::find_virtiofsd();
                if let Some(p) = &v {
                    tracing::info!(
                        "virtiofsd found at {}; writable shares use virtiofs",
                        p.display()
                    );
                }
                v
            },
            jobs,
            store,
            scanner,
            data_dir: data_dir.to_path_buf(),
            run_root,
            events,
            live: Mutex::default(),
            ops: tokio::sync::Mutex::new(()),
            lint_cache: Mutex::default(),
            helper: std::sync::RwLock::new(crate::helper_client::HelperClient::default_paths()),
            bus_cache: Mutex::default(),
            autostart_policy: Mutex::default(),
            restarts: Mutex::default(),
            me: std::sync::OnceLock::new(),
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<VmEvent> {
        self.events.subscribe()
    }

    /// Running VMs that have `image` open: (VM name, QMP session, format node
    /// name, attached read-only). Uses the spec the run was started with, so
    /// the node name matches the live process even if the saved spec changed.
    pub fn live_disks(&self, image: &ImageRef) -> Vec<(String, Qmp, String, bool)> {
        let mut out = Vec::new();
        for row in self.store.list_vms().unwrap_or_default() {
            let Some(run) = self
                .store
                .last_run(&row.id)
                .ok()
                .flatten()
                .filter(|r| r.ended_at.is_none())
            else {
                continue;
            };
            let Ok(spec) = serde_json::from_str::<VmSpec>(&run.spec_json) else {
                continue;
            };
            let Some(qmp) = self
                .live
                .lock()
                .unwrap()
                .get(&row.id)
                .and_then(|l| l.qmp.clone())
            else {
                continue;
            };
            for (i, d) in spec.disks.iter().enumerate() {
                if &d.image == image {
                    let ro = d.readonly || d.media == qemu_ui_core::spec::Media::Cdrom;
                    out.push((row.name.clone(), qmp.clone(), disk_node(&spec, i), ro));
                }
            }
        }
        out
    }

    /// Which VMs reference each image (by spec), and which have it open now
    /// (an active run whose spec lists it).
    pub fn image_users(
        &self,
    ) -> (
        HashMap<ImageRef, Vec<String>>,
        HashMap<ImageRef, Vec<String>>,
    ) {
        let mut used: HashMap<ImageRef, Vec<String>> = HashMap::new();
        let mut open: HashMap<ImageRef, Vec<String>> = HashMap::new();
        for row in self.store.list_vms().unwrap_or_default() {
            if let Ok(spec) = serde_json::from_str::<VmSpec>(&row.spec_json) {
                for d in &spec.disks {
                    used.entry(d.image.clone())
                        .or_default()
                        .push(row.name.clone());
                }
            }
            let run = self
                .store
                .last_run(&row.id)
                .ok()
                .flatten()
                .filter(|r| r.ended_at.is_none());
            // A suspended VM's disks belong to its saved state.
            let held = run.map(|r| r.spec_json).or_else(|| {
                (VmState::parse(&row.state) == VmState::Suspended)
                    .then(|| self.suspend_meta(&row.id).map(|m| m.spec_json))
                    .flatten()
            });
            if let Some(json) = held
                && let Ok(spec) = serde_json::from_str::<VmSpec>(&json)
            {
                for d in &spec.disks {
                    open.entry(d.image.clone())
                        .or_default()
                        .push(row.name.clone());
                }
            }
        }
        for v in used.values_mut().chain(open.values_mut()) {
            v.sort();
            v.dedup();
        }
        (used, open)
    }

    /// The daemon's data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn vm_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("vms").join(id)
    }

    pub(crate) fn run_dir(&self, id: &str) -> PathBuf {
        self.run_root.join(id)
    }

    // ---------------------------------------------------------------- events

    /// A live-only event: sent to connected browsers, not stored or replayed
    /// (e.g. console presence, which is stale a moment later).
    pub fn notify(&self, vm_id: &str, kind: &str, data: Value) {
        let _ = self.events.send(VmEvent {
            seq: 0,
            vm_id: vm_id.into(),
            at_us: now_us(),
            kind: kind.into(),
            data,
        });
    }

    fn emit(&self, vm_id: &str, kind: &str, data: Value) {
        let at_us = now_us();
        match self
            .store
            .insert_event(vm_id, at_us, kind, &data.to_string())
        {
            Ok(seq) => {
                let _ = self.events.send(VmEvent {
                    seq,
                    vm_id: vm_id.into(),
                    at_us,
                    kind: kind.into(),
                    data,
                });
            }
            Err(e) => tracing::error!("recording event: {e:#}"),
        }
    }

    fn set_state(&self, vm_id: &str, state: VmState, reason: Option<&str>, error: Option<&str>) {
        if let Err(e) = self
            .store
            .set_vm_state(vm_id, state.as_str(), reason, error)
        {
            tracing::error!("saving state: {e:#}");
        }
        self.emit(
            vm_id,
            "state",
            json!({"state": state, "pause_reason": reason, "error": error}),
        );
    }

    // --------------------------------------------------------------- queries

    async fn caps_for(&self, arch: &str) -> Result<BinaryCaps> {
        let caps = self.scanner.caps.read().await;
        let caps = caps
            .as_ref()
            .ok_or_else(|| anyhow!("capability scan has not finished yet"))?;
        caps.binaries
            .iter()
            .find(|b| b.arch == arch)
            .cloned()
            .ok_or_else(|| anyhow!("no qemu-system-{arch} binary is installed"))
    }

    pub async fn qemu_img(&self) -> Option<PathBuf> {
        let caps = self.scanner.caps.read().await;
        caps.as_ref()?
            .qemu_img
            .as_ref()
            .map(|b| PathBuf::from(&b.path))
    }

    pub fn summary(&self, row: &crate::store::VmRow) -> VmSummary {
        let spec: Option<VmSpec> = serde_json::from_str(&row.spec_json).ok();
        let last = self.store.last_run(&row.id).ok().flatten();
        let state = VmState::parse(&row.state);
        let active_run = last.as_ref().filter(|r| r.ended_at.is_none());
        let restart_pending = active_run.is_some_and(|r| {
            serde_json::from_str::<Value>(&r.spec_json).ok()
                != serde_json::from_str::<Value>(&row.spec_json).ok()
        });
        let (restart_at, crash_loop) = self.restart_info(&row.id);
        let qemu_update = active_run.and_then(|r| self.qemu_update(r.id, spec.as_ref()?));
        VmSummary {
            qemu_update,
            autostart: row.autostart,
            restart_at,
            crash_loop,
            id: row.id.clone(),
            name: row.name.clone(),
            arch: spec.as_ref().map(|s| s.arch.clone()).unwrap_or_default(),
            state,
            pause_reason: row.pause_reason.clone(),
            accel: last.as_ref().map(|r| r.accel.clone()),
            started_at: active_run.map(|r| r.started_at),
            last_error: row.last_error.clone(),
            restart_pending,
            has_vnc: active_run.is_some_and(|r| r.vnc_json.is_some()),
        }
    }

    /// The QEMU version a running VM will move to on its next start, when
    /// the binary it runs was replaced (upgraded) since it started (X-02).
    fn qemu_update(&self, run_id: i64, spec: &VmSpec) -> Option<String> {
        let (sha, version) = self.store.run_binary(run_id).ok().flatten()?;
        let caps = self.scanner.caps.try_read().ok()?;
        let now = caps
            .as_ref()?
            .binaries
            .iter()
            .find(|b| b.arch == spec.arch)?;
        (now.binary.sha256 != sha).then(|| {
            let new = now.qemu_version.to_string();
            if new == version {
                format!("{new} (rebuilt)")
            } else {
                new
            }
        })
    }

    pub fn list(&self) -> Result<Vec<VmSummary>> {
        Ok(self
            .store
            .list_vms()?
            .iter()
            .map(|r| self.summary(r))
            .collect())
    }

    pub fn detail(&self, id: &str) -> OpResult<VmDetail> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let spec = serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let last = self.store.last_run(id)?;
        let last_run_args = last
            .as_ref()
            .and_then(|r| serde_json::from_str::<Vec<String>>(&r.args_json).ok());
        let live_spec = last
            .as_ref()
            .filter(|r| r.ended_at.is_none())
            .and_then(|r| serde_json::from_str(&r.spec_json).ok());
        Ok(VmDetail {
            summary: self.summary(&row),
            spec,
            last_run_args,
            live_spec,
        })
    }

    /// VNC endpoint of the active run, for the console proxy.
    pub fn vnc_of(&self, id: &str) -> Option<VncInfo> {
        let run = self.store.last_run(id).ok().flatten()?;
        if run.ended_at.is_some() {
            return None;
        }
        serde_json::from_str(run.vnc_json.as_deref()?).ok()
    }

    /// Serial chardev socket of the active run, for the console hub.
    pub fn serial_socket(&self, id: &str) -> Option<PathBuf> {
        let run = self.store.last_run(id).ok().flatten()?;
        run.ended_at
            .is_none()
            .then(|| PathBuf::from(&run.run_dir).join("serial.sock"))
    }

    pub fn serial_log(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("serial.log")
    }

    // ------------------------------------------------------------ spec CRUD

    /// Normalise a submitted spec: pin the machine alias, assign MACs.
    async fn normalise(&self, mut spec: VmSpec) -> OpResult<(VmSpec, BinaryCaps)> {
        let caps = self.caps_for(&spec.arch).await?;
        if !spec.machine.type_name.is_empty()
            && let Ok(pinned) = compile::pin_machine(&spec.machine.type_name, &caps)
        {
            spec.machine.type_name = pinned;
        }
        self.assign_macs(&mut spec.nics);
        hash_guest_password(&mut spec)?;
        Ok((spec, caps))
    }

    /// The buses `machine` provides on this binary, probed once and cached.
    /// `None` when the probe fails (unknown: buses are then not checked).
    pub async fn machine_buses(&self, caps: &BinaryCaps, machine: &str) -> Option<MachineBuses> {
        let key = (caps.binary.sha256.clone(), machine.to_string());
        if let Some(hit) = self.bus_cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let probed = crate::probe::machine_buses(Path::new(&caps.binary.path), machine)
            .await
            .map_err(|e| tracing::warn!(machine, "probing buses: {e:#}"))
            .ok()
            .map(|buses| MachineBuses {
                machine: machine.to_string(),
                buses,
            });
        self.bus_cache.lock().unwrap().insert(key, probed.clone());
        probed
    }

    /// Disk buses that cannot exist on the spec's machine are errors.
    async fn check_buses(&self, spec: &VmSpec, caps: &BinaryCaps) -> OpResult<()> {
        let Some(m) = self.machine_buses(caps, &spec.machine.type_name).await else {
            return Ok(());
        };
        let diags = compile::validate_buses(spec, caps, &m);
        if diags.iter().any(|d| d.severity == Severity::Error) {
            return Err(OpError::Invalid(diags));
        }
        Ok(())
    }

    fn check(spec: &VmSpec, caps: &BinaryCaps) -> OpResult<Vec<Diagnostic>> {
        let diags = compile::validate(spec, caps);
        if diags.iter().any(|d| d.severity == Severity::Error) {
            return Err(OpError::Invalid(diags));
        }
        Ok(diags)
    }

    pub async fn create(&self, spec: VmSpec) -> OpResult<VmDetail> {
        let _g = self.ops.lock().await;
        let (spec, caps) = self.normalise(spec).await?;
        Self::check(&spec, &caps)?;
        self.check_buses(&spec, &caps).await?;
        if self.store.vm_name_taken(&spec.name, None)? {
            return Err(OpError::Conflict(format!(
                "a VM named '{}' already exists",
                spec.name
            )));
        }
        let id = loop {
            let id = format!("{:08x}", rand::random::<u32>());
            if self.store.get_vm(&id)?.is_none() {
                break id;
            }
        };
        std::fs::create_dir_all(self.vm_dir(&id)).map_err(|e| OpError::Failed(e.into()))?;
        let json = serde_json::to_string(&spec).map_err(|e| OpError::Failed(e.into()))?;
        self.store.insert_vm(&id, &spec.name, &json, now())?;
        self.emit(&id, "created", json!({"name": spec.name}));
        self.detail(&id)
    }

    pub async fn update(&self, id: &str, spec: VmSpec) -> OpResult<VmDetail> {
        let _g = self.ops.lock().await;
        self.refuse_if_suspended(id)?;
        let (spec, caps) = self.normalise(spec).await?;
        Self::check(&spec, &caps)?;
        self.check_buses(&spec, &caps).await?;
        if self.store.vm_name_taken(&spec.name, Some(id))? {
            return Err(OpError::Conflict(format!(
                "a VM named '{}' already exists",
                spec.name
            )));
        }
        let json = serde_json::to_string(&spec).map_err(|e| OpError::Failed(e.into()))?;
        self.store.update_vm_spec(id, &spec.name, &json, now())?;
        self.emit(id, "updated", json!({}));
        self.detail(id)
    }

    pub async fn delete(&self, id: &str) -> OpResult<()> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        if VmState::parse(&row.state).is_active() || self.live.lock().unwrap().contains_key(id) {
            return Err(OpError::Conflict("stop the VM before deleting it".into()));
        }
        self.store.delete_vm(id)?;
        // Images live in pools and are kept; per-VM state (VARS, logs) goes.
        let _ = std::fs::remove_dir_all(self.vm_dir(id));
        let _ = std::fs::remove_dir_all(self.run_dir(id));
        let _ = self.events.send(VmEvent {
            seq: 0,
            vm_id: id.into(),
            at_us: now_us(),
            kind: "deleted".into(),
            data: json!({}),
        });
        Ok(())
    }

    // -------------------------------------------------------------- compile

    /// Resolve images and firmware and compile, for one of three purposes
    /// (see [`BuildMode`]).
    async fn build(
        &self,
        id: &str,
        spec: &VmSpec,
        caps: &BinaryCaps,
        mode: &BuildMode,
    ) -> Result<(Compiled, Option<VncInfo>), Vec<Diagnostic>> {
        let run = matches!(mode, BuildMode::Run);
        let scratch = match mode {
            BuildMode::Lint(dir) => Some(dir.as_path()),
            _ => None,
        };
        let fail = |field: &str, rule: &str, msg: String| {
            vec![Diagnostic {
                severity: Severity::Error,
                field: field.into(),
                rule: rule.into(),
                message: msg,
            }]
        };
        // Templates are immutable bases for linked clones (M2-05): writing to
        // one would silently corrupt every clone built on it.
        for (i, d) in spec.disks.iter().enumerate() {
            if !d.readonly && d.media == Media::Disk && self.pools.is_template(&d.image) {
                return Err(fail(
                    &format!("disks[{i}].image"),
                    "disk.template-writable",
                    format!(
                        "'{}' is a template and is never opened writable; attach it read-only, or clone the \
                         VM or image instead",
                        d.image.name
                    ),
                ));
            }
        }
        let qemu_img = self.qemu_img().await;
        let mut images: BTreeMap<ImageRef, ResolvedImage> = BTreeMap::new();
        for (i, d) in spec.disks.iter().enumerate() {
            if qemu_ui_core::spec::is_host_disk(&d.image) {
                let resolved = self.resolve_host_disk(id, i, d, mode).await.map_err(|e| {
                    fail(
                        &format!("disks[{i}].image"),
                        "disk.host-disk",
                        format!("{e:#}"),
                    )
                })?;
                images.insert(d.image.clone(), resolved);
                continue;
            }
            let Ok(path) = self.pools.resolve(&d.image) else {
                continue;
            }; // compile reports it
            let format = match &qemu_img {
                Some(q) => self
                    .pools
                    .info(q, &path)
                    .await
                    .map(|(f, _, _)| f)
                    .unwrap_or_else(|_| "raw".into()),
                None => {
                    return Err(fail(
                        &format!("disks[{i}]"),
                        "qemu-img.missing",
                        "qemu-img is not installed".into(),
                    ));
                }
            };
            let path = match (scratch, &qemu_img) {
                // Lint: a blank image of the same format and size, so the real
                // one (perhaps in use by the running VM) is never opened.
                (Some(dir), Some(q)) => {
                    let size = self
                        .pools
                        .info(q, &path)
                        .await
                        .map(|(_, s, _)| s)
                        .unwrap_or(1 << 20);
                    let blank = dir.join(format!("disk{i}"));
                    let fmt = if format == "qcow2" || format == "raw" {
                        format.as_str()
                    } else {
                        "raw"
                    };
                    let ok = crate::probe::command(q)
                        .args(["create", "-q", "-f", fmt])
                        .arg(&blank)
                        .arg(size.max(1 << 20).to_string())
                        .status()
                        .await
                        .is_ok_and(|s| s.success());
                    if !ok {
                        return Err(fail(
                            &format!("disks[{i}]"),
                            "lint.scratch",
                            "could not create a scratch image".into(),
                        ));
                    }
                    images.insert(
                        d.image.clone(),
                        ResolvedImage {
                            path: path_str(&blank),
                            format: fmt.into(),
                            nbd: None,
                        },
                    );
                    continue;
                }
                _ => path,
            };
            images.insert(
                d.image.clone(),
                ResolvedImage {
                    path: path_str(&path),
                    format,
                    nbd: None,
                },
            );
        }

        let firmware = match &spec.firmware {
            FirmwareSpec::Default => None,
            FirmwareSpec::Descriptor { id: fw_id } => {
                let caps_all = self.scanner.caps.read().await;
                let fw = caps_all
                    .as_ref()
                    .and_then(|c| c.firmware.iter().find(|f| &f.id == fw_id))
                    .cloned();
                drop(caps_all);
                let Some(fw) = fw else {
                    return Err(fail(
                        "firmware",
                        "firmware.unknown",
                        format!("firmware '{fw_id}' is not installed"),
                    ));
                };
                if !fw.supports(&spec.arch, &spec.machine.type_name) {
                    return Err(fail(
                        "firmware",
                        "firmware.unsupported",
                        format!(
                            "firmware '{fw_id}' does not support {} / {}",
                            spec.arch, spec.machine.type_name
                        ),
                    ));
                }
                let Some(code) = fw.executable.as_ref().and_then(|f| f.resolved.clone()) else {
                    return Err(fail(
                        "firmware",
                        "firmware.missing-file",
                        "firmware code file is missing".into(),
                    ));
                };
                let vars_path = match scratch {
                    Some(dir) => dir.join("VARS.qcow2"),
                    None => self.vm_dir(id).join("VARS.qcow2"),
                };
                let vars = match fw.nvram_template.as_ref().and_then(|t| t.resolved.clone()) {
                    Some(template) => {
                        if (run || scratch.is_some()) && !vars_path.exists() {
                            let Some(q) = &qemu_img else {
                                return Err(fail(
                                    "firmware",
                                    "qemu-img.missing",
                                    "qemu-img is not installed".into(),
                                ));
                            };
                            if let Some(parent) = vars_path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let out = crate::probe::command(q)
                                .args(["convert", "-q", "-O", "qcow2"])
                                .arg(&template)
                                .arg(&vars_path)
                                .output()
                                .await;
                            if !out.as_ref().is_ok_and(|o| o.status.success()) {
                                return Err(fail(
                                    "firmware",
                                    "firmware.vars",
                                    "could not create the NVRAM file".into(),
                                ));
                            }
                        }
                        Some(path_str(&vars_path))
                    }
                    None => None,
                };
                Some(ResolvedFirmware {
                    code,
                    code_format: fw
                        .executable
                        .as_ref()
                        .and_then(|f| f.format.clone())
                        .unwrap_or_else(|| "raw".into()),
                    vars,
                    vars_format: "qcow2".into(),
                    secure_boot: fw.secure_boot(),
                    requires_smm: fw.features.iter().any(|f| f == "requires-smm"),
                })
            }
        };

        let dir = scratch
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.run_dir(id));
        let s = |n: &str| path_str(&dir.join(n));
        let serial_log = match scratch {
            Some(d) => d.join("serial.log"),
            None => self.vm_dir(id).join("serial.log"),
        };
        let vnc = if !spec.display.vnc {
            None
        } else if cfg!(windows) {
            let port = if run {
                free_vnc_port().map_err(|e| fail("display.vnc", "vnc.port", e.to_string()))?
            } else {
                // Preview: show the port of the last run so the "changes since
                // last start" diff is not polluted by port allocation.
                self.store
                    .last_run(id)
                    .ok()
                    .flatten()
                    .and_then(|r| serde_json::from_str::<VncInfo>(r.vnc_json.as_deref()?).ok())
                    .and_then(|v| match v {
                        VncInfo::Tcp { port, .. } => Some(port),
                        VncInfo::Unix { .. } => None,
                    })
                    .unwrap_or(5900)
            };
            Some(VncInfo::Tcp {
                port,
                password: random_password(),
            })
        } else {
            Some(VncInfo::Unix {
                path: s("vnc.sock"),
            })
        };
        let vnc_listen = match &vnc {
            Some(VncInfo::Tcp { port, .. }) => VncListen::Tcp {
                display: port - 5900,
            },
            Some(VncInfo::Unix { path }) => VncListen::Unix(path.clone()),
            None => VncListen::Unix(String::new()),
        };
        let machine = self.machine_buses(caps, &spec.machine.type_name).await;
        let networks = self
            .networks
            .list()
            .map_err(|e| fail("nics", "nic.networks-unreadable", format!("{e:#}")))?;
        let taps = self.resolve_taps(id, spec, &networks, mode).await?;
        // Isolated networks: a switch socket per network. Only a start
        // brings the switch up; previews just show where it will listen.
        let mut switch_sockets = std::collections::BTreeMap::new();
        for nic in &spec.nics {
            let NicBackend::Network { network, .. } = &nic.backend else {
                continue;
            };
            if !networks
                .iter()
                .any(|n| &n.id == network && n.config.needs_switch())
            {
                continue;
            }
            let path = if run {
                self.switches.acquire(network, id).map_err(|e| {
                    fail(
                        "nics",
                        "nic.switch-failed",
                        format!("starting the isolated network's switch: {e}"),
                    )
                })?
            } else {
                self.switches.socket_path(network)
            };
            switch_sockets.insert(network.clone(), path_str(&path));
        }
        // SPICE (M5-05): a loopback port and password per run. Previews show
        // the last run's port so the "changes since start" diff stays quiet.
        let spice_port = if !spec.display.spice {
            if run {
                crate::spice::clear(&self.store, id);
            }
            None
        } else if run {
            let port = crate::spice::free_port()
                .map_err(|e| fail("display.spice", "spice.port", e.to_string()))?;
            let info = crate::spice::SpiceInfo {
                port,
                password: crate::spice::password(),
            };
            crate::spice::record(&self.store, id, &info);
            Some(port)
        } else {
            Some(crate::spice::lookup(&self.store, id).map_or(6900, |s| s.port))
        };
        // cloud-init (M7-03): the seed is rebuilt at every start from the
        // spec, next to the VM's other state; previews show where it goes.
        let seed = match &spec.guest.cloud_init {
            None => None,
            Some(_) => {
                // A dry lint really starts QEMU, so its scratch copy is
                // written too.
                let path = match scratch {
                    Some(d) => d.join("seed.iso"),
                    None => self.vm_dir(id).join("cloud-init.iso"),
                };
                if run || scratch.is_some() {
                    let img = qemu_ui_core::guest::seed_image(spec, id).unwrap_or_default();
                    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))
                        .and_then(|()| std::fs::write(&path, img))
                        .map_err(|e| {
                            fail(
                                "guest.cloud_init",
                                "guest.seed-write",
                                format!("writing the cloud-init seed: {e}"),
                            )
                        })?;
                }
                Some(path_str(&path))
            }
        };
        let ctx = compile::Context {
            taps,
            switch_sockets: &switch_sockets,
            networks: &networks,
            caps,
            machine: machine.as_ref(),
            os: std::env::consts::OS,
            run: RunPaths {
                qmp: s("qmp.sock"),
                qmp_user: s("qmp-user.sock"),
                serial: s("serial.sock"),
                serial_log: path_str(&serial_log),
                pidfile: s("qemu.pid"),
                vnc: vnc_listen,
                spice_port,
                qga: s("ga.sock"),
                seed,
            },
            images: &images,
            firmware,
            shares: self.resolve_shares(spec, caps, &dir, scratch.is_some())?,
        };
        compile::compile(spec, &ctx).map(|c| (c, vnc))
    }

    /// Resolve each share to a confined host folder and pick its backend:
    /// virtiofs for writable shares when `virtiofsd` is installed and the
    /// binary supports it, else 9p. "Check config" always uses 9p (no
    /// sidecar runs during a lint). When this QEMU has no folder sharing at
    /// all, placeholders are returned and the compiler reports why.
    fn resolve_shares(
        &self,
        spec: &VmSpec,
        caps: &BinaryCaps,
        run_dir: &Path,
        lint: bool,
    ) -> Result<Vec<compile::ResolvedShare>, Vec<Diagnostic>> {
        let ninep = compile::ninep_device(caps).is_some();
        let virtiofs = !lint && self.virtiofsd.is_some() && compile::virtiofs_supported(caps);
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for (i, share) in spec.shares.iter().enumerate() {
            if !ninep && !virtiofs {
                out.push(compile::ResolvedShare {
                    path: String::new(),
                    backend: compile::ShareBackend::NineP,
                });
                continue;
            }
            let path = match self.shares.resolve(&share.root, &share.path) {
                Ok(p) => path_str(&p),
                Err(e) => {
                    errors.push(Diagnostic {
                        severity: Severity::Error,
                        field: format!("shares[{i}]"),
                        rule: "share.unresolved".into(),
                        message: format!("{e:#}"),
                    });
                    continue;
                }
            };
            // Read-only is enforced by 9p (`readonly=on`); virtiofs is used
            // for writable shares only.
            let backend = if virtiofs && !share.readonly {
                compile::ShareBackend::VirtioFs {
                    socket: path_str(&run_dir.join(format!("vfs{i}.sock"))),
                }
            } else if ninep {
                compile::ShareBackend::NineP
            } else {
                errors.push(Diagnostic {
                    severity: Severity::Error,
                    field: format!("shares[{i}].readonly"),
                    rule: "share.readonly-needs-9p".into(),
                    message: "this QEMU has no virtio-9p, and read-only shares need it".into(),
                });
                continue;
            };
            out.push(compile::ResolvedShare { path, backend });
        }
        if errors.is_empty() {
            Ok(out)
        } else {
            Err(errors)
        }
    }

    /// Start the run's sidecars: one `passt` per NIC on a passt network
    /// (M3-03) and one `virtiofsd` per virtiofs share, waiting for each
    /// socket. Returned with their kind, for the `run_sidecar` table.
    async fn start_sidecars(
        &self,
        id: &str,
        spec: &VmSpec,
        caps: &BinaryCaps,
        run_dir: &Path,
        logs: &Path,
    ) -> Result<Vec<(&'static str, ProcId)>> {
        let networks = self.networks.list()?;
        let qmp = path_str(&run_dir.join("qmp.sock"));
        let plans = crate::passt::plan(&spec.nics, &networks, &qmp);
        let passt = crate::passt::start(self.passt.as_deref(), &plans, logs, now()).await?;
        let mut all: Vec<(&'static str, ProcId)> = passt.iter().map(|p| ("passt", *p)).collect();
        let stop = |all: &[(&'static str, ProcId)]| {
            for (_, p) in all {
                procs::terminate(*p, true);
            }
        };
        if spec.tpm.is_some() {
            let socket = compile::tpm_socket(&qmp);
            let log = logs.join(format!("run-{}.swtpm.log", now()));
            match crate::swtpm::start(
                self.swtpm.as_deref(),
                &self.vm_dir(id).join("tpm"),
                &socket,
                &log,
            )
            .await
            {
                Ok(p) => all.push(("swtpm", p)),
                Err(e) => {
                    stop(&all);
                    return Err(e);
                }
            }
        }
        match self.start_virtiofsd(spec, caps, run_dir, logs).await {
            Ok(v) => all.extend(v.into_iter().map(|p| ("virtiofsd", p))),
            Err(e) => {
                stop(&all);
                return Err(e);
            }
        }
        Ok(all)
    }

    /// Why a VM on `caps` cannot have a TPM on this host, if it cannot.
    pub fn tpm_unavailable(&self, caps: &BinaryCaps) -> Option<String> {
        crate::swtpm::unavailable_reason(caps, self.swtpm.as_deref())
    }

    /// Start one `virtiofsd` per virtiofs share and wait for its socket.
    /// The sidecars are detached like QEMU: they survive a daemon restart and
    /// exit by themselves when QEMU disconnects; `finish` also terminates any
    /// that are left once the run is over.
    async fn start_virtiofsd(
        &self,
        spec: &VmSpec,
        caps: &BinaryCaps,
        run_dir: &Path,
        logs: &Path,
    ) -> Result<Vec<ProcId>> {
        let shares = self
            .resolve_shares(spec, caps, run_dir, false)
            .map_err(|d| {
                anyhow!(
                    "{}",
                    d.iter()
                        .map(|x| x.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?;
        let mut started = Vec::new();
        for (i, r) in shares.iter().enumerate() {
            let compile::ShareBackend::VirtioFs { socket } = &r.backend else {
                continue;
            };
            let Some(bin) = &self.virtiofsd else {
                bail!("virtiofsd is not installed")
            };
            let args: Vec<String> = vec![
                format!("--socket-path={socket}"),
                format!("--shared-dir={}", r.path),
                // Unprivileged daemon: no namespace/chroot sandbox available.
                "--sandbox=none".into(),
                "--cache=auto".into(),
            ];
            let log = logs.join(format!("run-{}.virtiofsd{i}.log", now()));
            let (child, id) = match procs::spawn_detached(&path_str(bin), &args, &log) {
                Ok(x) => x,
                Err(e) => {
                    for p in &started {
                        procs::terminate(*p, true);
                    }
                    return Err(e);
                }
            };
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            started.push(id);
            let sock = PathBuf::from(socket);
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
                for p in &started {
                    procs::terminate(*p, true);
                }
                bail!(
                    "virtiofsd for share {i} did not start: {}",
                    procs::log_tail(&log, 5)
                );
            }
        }
        Ok(started)
    }

    /// Compile without side effects, for the live argv drawer.
    pub async fn preview(
        &self,
        id: Option<&str>,
        spec: VmSpec,
    ) -> OpResult<(Compiled, Vec<Diagnostic>)> {
        let (spec, caps) = self.normalise(spec).await?;
        match self
            .build(id.unwrap_or("new"), &spec, &caps, &BuildMode::Preview)
            .await
        {
            Ok((c, _)) => {
                let mut w = c.warnings.clone();
                w.extend(self.forward_conflicts(id, &spec));
                w.extend(self.shared_mac_warnings(id, &spec));
                self.add_rule_diagnostics(&mut w, &spec, &caps).await;
                Ok((c, w))
            }
            Err(mut d) => {
                self.add_rule_diagnostics(&mut d, &spec, &caps).await;
                Err(OpError::Invalid(d))
            }
        }
    }

    /// "Check config" (M1-10): run the argv through QEMU with every side
    /// effect rewritten away, then quit. Never touches the VM's real disks,
    /// NVRAM, ports or sockets, so it is safe while the VM runs.
    pub async fn lint(&self, id: Option<&str>, spec: VmSpec) -> OpResult<LintResult> {
        let began = std::time::Instant::now();
        let (spec, caps) = self.normalise(spec).await?;
        // Depends on other VMs, so it is never cached.
        let conflicts = self.forward_conflicts(id, &spec);
        // Curated rules (L2) are cheap and may change with site overrides.
        let facts = self.rule_facts(&spec, &caps).await;
        let lspec = lint::lint_spec(&spec);
        let key = {
            use sha2::{Digest, Sha256};
            let json = serde_json::to_string(&lspec).unwrap_or_default();
            hex::encode(Sha256::digest(
                format!("{}\n{json}", caps.binary.sha256).as_bytes(),
            ))
        };
        if let Some(mut hit) = self.lint_cache.lock().unwrap().get(&key).cloned() {
            hit.cached = true;
            hit.diagnostics.extend(conflicts);
            self.rules().extend(&mut hit.diagnostics, &spec, &facts);
            return Ok(hit);
        }

        let tmp = self
            .run_root
            .join(format!("lint-{:06x}", rand::random::<u32>() & 0xff_ffff));
        std::fs::create_dir_all(&tmp).map_err(|e| OpError::Failed(e.into()))?;
        let result = self.lint_in(id.unwrap_or("new"), &lspec, &caps, &tmp).await;
        let _ = std::fs::remove_dir_all(&tmp);
        let (ok, diagnostics) = result?;
        let res = LintResult {
            ok,
            diagnostics,
            qemu_version: caps.qemu_version.to_string(),
            duration_ms: began.elapsed().as_millis() as u64,
            cached: false,
        };
        let mut cache = self.lint_cache.lock().unwrap();
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key, res.clone());
        let mut res = res;
        res.diagnostics.extend(conflicts);
        self.rules().extend(&mut res.diagnostics, &spec, &facts);
        Ok(res)
    }

    async fn lint_in(
        &self,
        id: &str,
        spec: &VmSpec,
        caps: &BinaryCaps,
        tmp: &Path,
    ) -> OpResult<(bool, Vec<Diagnostic>)> {
        let compiled = match self
            .build(id, spec, caps, &BuildMode::Lint(tmp.to_path_buf()))
            .await
        {
            Ok((c, _)) => c,
            Err(d) => return Ok((false, d)),
        };
        let mut child = crate::probe::command(Path::new(&compiled.program))
            .args(&compiled.args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| OpError::Failed(e.into()))?;
        let sock = tmp.join("qmp.sock");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let qmp = loop {
            if let Ok(q) = Qmp::connect_unix(&sock).await {
                break Some(q);
            }
            if matches!(child.try_wait(), Ok(Some(_))) || tokio::time::Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let mut warnings = compiled.warnings.clone();
        let ok = match qmp {
            Some(q) => {
                let ok = q
                    .execute("query-status", None)
                    .await
                    .is_ok_and(|v| v["status"] == "prelaunch");
                let _ = q
                    .execute_with_timeout("quit", None, Duration::from_secs(5))
                    .await;
                ok
            }
            None => false,
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        let _ = child.kill().await;
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ =
                tokio::time::timeout(Duration::from_secs(2), e.read_to_string(&mut stderr)).await;
        }
        let mut found = lint::diagnostics_from_stderr(&stderr, spec);
        if !ok && !found.iter().any(|d| d.severity == Severity::Error) {
            found.push(Diagnostic {
                severity: Severity::Error,
                field: String::new(),
                rule: "qemu.lint".into(),
                message: "QEMU did not start with this configuration".into(),
            });
        }
        warnings.extend(found);
        Ok((ok, warnings))
    }

    // ------------------------------------------------------------ lifecycle

    /// Start the VM (or resume it, if it is suspended to disk). A start by
    /// hand clears the crash-loop breaker (M7-08).
    pub async fn start(self: &Arc<Self>, id: &str) -> OpResult<VmSummary> {
        self.reset_restarts(id);
        self.start_inner(id).await
    }

    async fn start_inner(self: &Arc<Self>, id: &str) -> OpResult<VmSummary> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        if VmState::parse(&row.state).is_active() || self.live.lock().unwrap().contains_key(id) {
            return Err(OpError::Conflict("the VM is already running".into()));
        }
        // Resuming builds the machine the process had when it was suspended.
        let resume = if VmState::parse(&row.state) == VmState::Suspended {
            Some(self.suspend_meta(id).ok_or_else(|| {
                OpError::Conflict(
                    "the saved state of this suspended VM is missing or incomplete; discard it"
                        .into(),
                )
            })?)
        } else {
            None
        };
        let spec_json = resume
            .as_ref()
            .map_or(row.spec_json.clone(), |m| m.spec_json.clone());
        let spec: VmSpec =
            serde_json::from_str(&spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let caps = self.caps_for(&spec.arch).await?;
        // Gives back what `build` sets up on the host (helper disk exports,
        // taps, isolated-network switches) on every path where no run gets
        // recorded; once it is, `finish` releases them.
        let mut lease = StartLease {
            sv: self,
            vm: id,
            keep: false,
        };
        let (mut compiled, vnc) = self
            .build(id, &spec, &caps, &BuildMode::Run)
            .await
            .map_err(OpError::Invalid)?;
        if resume.is_some() {
            compiled
                .args
                .extend(["-incoming".to_string(), "defer".to_string()]);
        }

        let run_dir = self.run_dir(id);
        let _ = std::fs::remove_dir_all(&run_dir); // stale sockets from a crashed run
        std::fs::create_dir_all(&run_dir).map_err(|e| OpError::Failed(e.into()))?;
        let logs = self.vm_dir(id).join("logs");
        std::fs::create_dir_all(&logs).map_err(|e| OpError::Failed(e.into()))?;
        self.rotate_logs(id);
        let log = logs.join(format!("run-{}.log", now()));

        // Hold the image locks until the run is recorded, so an image
        // operation (e.g. compaction) cannot replace a disk this VM is about
        // to open; see `image_ops::lock_for_start`.
        let images: Vec<ImageRef> = spec.disks.iter().map(|d| d.image.clone()).collect();
        let start_locks = crate::image_ops::lock_for_start(&images).map_err(OpError::Conflict)?;

        self.store.clear_vm_error(id)?;
        self.set_state(id, VmState::Starting, None, None);
        tracing::info!(vm = %row.name, "starting: {}", compile::display_command(&compiled.program, &compiled.args));

        let sidecars = match self.start_sidecars(id, &spec, &caps, &run_dir, &logs).await {
            Ok(s) => s,
            Err(e) => {
                self.set_state(id, VmState::Crashed, None, Some(&format!("{e:#}")));
                return Err(OpError::Failed(e));
            }
        };
        let (child, proc_id) = match procs::spawn_detached(&compiled.program, &compiled.args, &log)
        {
            Ok(x) => x,
            Err(e) => {
                for (_, p) in &sidecars {
                    procs::terminate(*p, true);
                }
                self.set_state(id, VmState::Crashed, None, Some(&format!("{e:#}")));
                return Err(OpError::Failed(e));
            }
        };
        let args_json = serde_json::to_string(&compiled.args).unwrap_or_default();
        let vnc_json = vnc
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let run_id = self.store.insert_run(&NewRun {
            vm_id: id,
            args_json: &args_json,
            spec_json: &spec_json,
            accel: &compiled.accel,
            pid: proc_id.pid,
            proc_start: proc_id.start_time,
            run_dir: &path_str(&run_dir),
            vnc_json: vnc_json.as_deref(),
            log_path: &path_str(&log),
            started_at: now(),
        })?;
        let _ =
            self.store
                .set_run_binary(run_id, &caps.binary.sha256, &caps.qemu_version.to_string());
        self.live.lock().unwrap().insert(
            id.into(),
            Live {
                run_id,
                qmp: None,
                proc_id,
                stop_requested: false,
                shutdown_seen: false,
                suspending: false,
            },
        );
        lease.keep = true;
        for (kind, p) in &sidecars {
            let _ = self.store.insert_sidecar(run_id, kind, p.pid, p.start_time);
        }
        // CPU/memory limits (job object or cgroup) before the guest runs.
        if let Some(msg) = self.enforce_limits(id, proc_id.pid, &spec.limits).await {
            self.emit(id, "limits", json!({"message": msg}));
        }
        // The run is recorded: image operations now see this VM as a user.
        drop(start_locks);

        // Reap the child while we are its parent; report how it exited.
        let (exit_tx, exit_rx) = oneshot::channel::<String>();
        std::thread::spawn(move || {
            let mut child = child;
            let status = child
                .wait()
                .map(|s| s.to_string())
                .unwrap_or_else(|e| e.to_string());
            let _ = exit_tx.send(status);
        });

        match self
            .bring_up(id, &run_dir, proc_id, vnc.as_ref(), resume.as_ref())
            .await
        {
            Ok(qmp) => {
                if let Some(l) = self.live.lock().unwrap().get_mut(id) {
                    l.qmp = Some(qmp.clone());
                }
                if let Some(meta) = &resume {
                    self.resumed(id);
                    self.emit(id, "resumed", json!({"suspended_at": meta.at}));
                }
                if resume.as_ref().is_some_and(|m| m.was_paused) {
                    self.set_state(id, VmState::Paused, Some("user"), None);
                } else {
                    self.set_state(id, VmState::Running, None, None);
                }
                self.emit(
                    id,
                    "started",
                    json!({"accel": compiled.accel, "pid": proc_id.pid}),
                );
                let this = self.clone();
                let vm = id.to_string();
                tokio::spawn(async move { this.monitor(vm, qmp, Some(exit_rx)).await });
            }
            Err(e) => {
                procs::terminate(proc_id, true);
                let tail = procs::log_tail(&log, 5);
                let msg = if tail.is_empty() {
                    format!("{e:#}")
                } else {
                    format!("{e:#}\n{tail}")
                };
                self.finish(id, run_id, "failed to start", Some(&msg));
                if resume.is_some() {
                    // The saved state is still there: stay suspended.
                    let msg = format!("resuming failed: {msg}");
                    self.set_state(id, VmState::Suspended, None, Some(&msg));
                    return Err(OpError::Failed(anyhow!(msg)));
                }
                return Err(OpError::Failed(anyhow!(msg)));
            }
        }
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        Ok(self.summary(&row))
    }

    /// Connect, check, configure, `cont` (plan.md §6.3).
    async fn bring_up(
        &self,
        id: &str,
        run_dir: &Path,
        proc_id: ProcId,
        vnc: Option<&VncInfo>,
        resume: Option<&suspend::SuspendMeta>,
    ) -> Result<Qmp> {
        let qmp =
            connect_with_retry(&run_dir.join("qmp.sock"), proc_id, QMP_CONNECT_TIMEOUT).await?;
        let st = qmp.execute("query-status", None).await?;
        let status = st["status"].as_str().unwrap_or_default();
        let want = if resume.is_some() {
            "inmigrate"
        } else {
            "prelaunch"
        };
        if status != want {
            bail!("unexpected initial state '{status}'");
        }
        let blocks = qmp.execute("query-block", None).await?;
        self.emit(
            id,
            "checked",
            json!({"block_devices": blocks.as_array().map_or(0, Vec::len)}),
        );
        if let Some(VncInfo::Tcp { password, .. }) = vnc {
            qmp.execute(
                "set_password",
                Some(json!({"protocol": "vnc", "password": password})),
            )
            .await
            .context("setting the VNC password")?;
        }
        if let Some(s) = crate::spice::lookup(&self.store, id) {
            // Never logged: the password only travels over QMP.
            qmp.execute(
                "set_password",
                Some(json!({"protocol": "spice", "password": s.password})),
            )
            .await
            .context("setting the SPICE password")?;
        }
        if let Some(meta) = resume {
            self.load_state(id, &qmp, meta).await?;
            if meta.was_paused {
                return Ok(qmp);
            }
        }
        qmp.execute("cont", None).await?;
        Ok(qmp)
    }

    /// Follow a running VM until its process exits, reconnecting the monitor
    /// if the connection drops while QEMU is still alive.
    async fn monitor(
        self: Arc<Self>,
        id: String,
        mut qmp: Qmp,
        mut exit_rx: Option<oneshot::Receiver<String>>,
    ) {
        loop {
            let mut events = qmp.events();
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        Ok(ev) => self.on_event(&id, &qmp, ev.event, ev.data).await,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(vm = %id, "missed {n} QMP events; refreshing status");
                            self.refresh_status(&id, &qmp).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = qmp.closed() => break,
                }
            }
            // Monitor connection is gone. Has QEMU exited?
            let Some(proc_id) = self.live.lock().unwrap().get(&id).map(|l| l.proc_id) else {
                return;
            };
            let exit = match exit_rx.as_mut() {
                Some(rx) => tokio::time::timeout(EXIT_WAIT, rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                None => {
                    let mut status = None;
                    for _ in 0..(EXIT_WAIT.as_millis() / 250) {
                        if !procs::alive(proc_id) {
                            status = Some("exited".to_string());
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    status
                }
            };
            if let Some(status) = exit {
                let run_id = self.live.lock().unwrap().get(&id).map(|l| l.run_id);
                if let Some(run_id) = run_id {
                    self.finish(&id, run_id, &status, None);
                }
                return;
            }
            // Still alive: reconnect.
            let run_dir = self.run_dir(&id);
            match connect_with_retry(
                &run_dir.join("qmp.sock"),
                proc_id,
                Duration::from_secs(3600),
            )
            .await
            {
                Ok(q) => {
                    tracing::info!(vm = %id, "monitor reconnected");
                    if let Some(l) = self.live.lock().unwrap().get_mut(&id) {
                        l.qmp = Some(q.clone());
                    }
                    self.refresh_status(&id, &q).await;
                    qmp = q;
                }
                Err(_) => continue, // process died meanwhile; loop detects exit
            }
        }
    }

    async fn on_event(&self, id: &str, qmp: &Qmp, kind: String, data: Value) {
        if kind == "DEVICE_DELETED" {
            // A disk unplug the guest acknowledged late: free its nodes.
            self.disk_device_deleted(qmp, &data).await;
        }
        self.emit(id, &kind, data.clone());
        match kind.as_str() {
            "SHUTDOWN" => {
                if let Some(l) = self.live.lock().unwrap().get_mut(id) {
                    l.shutdown_seen = true;
                }
                self.set_state(id, VmState::Stopping, None, None);
            }
            "BLOCK_IO_ERROR" => {
                self.refresh_status(id, qmp).await;
                // Disks stop the guest on ENOSPC (`werror=enospc`): say which
                // disk, so the user knows what to free and can resume (X-02).
                if data["action"] == "stop" {
                    let spec = self
                        .store
                        .last_run(id)
                        .ok()
                        .flatten()
                        .and_then(|r| serde_json::from_str::<VmSpec>(&r.spec_json).ok());
                    let msg = io_error_message(&data, spec.as_ref());
                    self.set_state(id, VmState::Paused, Some("io-error"), Some(&msg));
                }
            }
            "RESUME" => {
                // Whatever stopped it (e.g. a full disk) is over.
                let _ = self.store.clear_vm_error(id);
                self.refresh_status(id, qmp).await;
            }
            "STOP" | "RESET" | "SUSPEND" | "WAKEUP" | "GUEST_PANICKED" => {
                self.refresh_status(id, qmp).await;
            }
            _ => {}
        }
    }

    async fn refresh_status(&self, id: &str, qmp: &Qmp) {
        let Ok(st) = qmp.execute("query-status", None).await else {
            return;
        };
        let (mut state, reason) = state_from_qmp(st["status"].as_str().unwrap_or_default());
        let stopping = self
            .live
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|l| l.stop_requested);
        if stopping && state == VmState::Running {
            state = VmState::Stopping;
        }
        self.set_state(id, state, reason.as_deref(), None);
    }

    /// The run is over: record it and settle the final state.
    fn finish(&self, id: &str, run_id: i64, exit_status: &str, error: Option<&str>) {
        let live = self.live.lock().unwrap().remove(id);
        self.release_host_disks(id);
        self.release_taps(id);
        self.switches.release(id);
        self.release_limits(id);
        crate::spice::clear(&self.store, id);
        let expected = live
            .as_ref()
            .is_some_and(|l| l.stop_requested || l.shutdown_seen);
        let suspended = live.as_ref().is_some_and(|l| l.suspending);
        let _ = self.store.end_run(run_id, now(), exit_status);
        // Helpers (virtiofsd) normally exit with QEMU; make sure.
        for (_, pid, start) in self.store.sidecars(run_id).unwrap_or_default() {
            let p = ProcId {
                pid,
                start_time: start,
            };
            if procs::alive(p) {
                procs::terminate(p, true);
            }
        }
        let _ = std::fs::remove_dir_all(self.run_dir(id));
        if suspended && error.is_none() && self.suspend_meta(id).is_some() {
            self.set_state(id, VmState::Suspended, None, None);
        } else if expected && error.is_none() {
            self.set_state(id, VmState::Stopped, None, None);
        } else {
            let msg = error.map(String::from).unwrap_or_else(|| {
                let log = self
                    .store
                    .last_run(id)
                    .ok()
                    .flatten()
                    .map(|r| procs::log_tail(Path::new(&r.log_path), 5))
                    .unwrap_or_default();
                format!("QEMU exited unexpectedly ({exit_status}). {log}")
                    .trim()
                    .to_string()
            });
            self.set_state(id, VmState::Crashed, None, Some(&msg));
            self.crashed(id);
        }
        self.emit(id, "exited", json!({"status": exit_status}));
    }

    /// The monitor of a running VM (for block jobs and other live operations).
    pub fn live_qmp(&self, id: &str) -> Option<Qmp> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .and_then(|l| l.qmp.clone())
    }

    /// The running VM (id) and disk node index (`qui-disk<N>`) that have
    /// this image open.
    pub fn find_open_disk(&self, r: &ImageRef) -> Option<(String, usize)> {
        self.store.list_vms().ok()?.into_iter().find_map(|row| {
            let run = self
                .store
                .last_run(&row.id)
                .ok()
                .flatten()
                .filter(|r| r.ended_at.is_none())?;
            let spec: VmSpec = serde_json::from_str(&run.spec_json).ok()?;
            let i = spec.disks.iter().position(|d| &d.image == r)?;
            Some((row.id, spec.disks[i].node_index(i)))
        })
    }

    // ---------------------------------------------------------------- clone

    /// Clone a stopped VM (M2-05). `Linked` replaces each writable disk with
    /// a qcow2 overlay on it (O(1), needs template disks); `Full` copies
    /// every writable disk in a job. Read-only media (ISOs) are shared,
    /// NVRAM is copied, MACs are new.
    pub async fn clone_vm(
        self: &Arc<Self>,
        id: &str,
        name: String,
        mode: CloneMode,
        pool: String,
    ) -> OpResult<CloneOutcome> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        self.refuse_if_suspended(id)?;
        if VmState::parse(&row.state).is_active() || self.live.lock().unwrap().contains_key(id) {
            return Err(OpError::Conflict(
                "stop the VM before cloning it: the disks of a running VM are not consistent"
                    .into(),
            ));
        }
        if !valid_name(&name) {
            return Err(OpError::Invalid(vec![Diagnostic {
                severity: Severity::Error,
                field: "name".into(),
                rule: "name.invalid".into(),
                message:
                    "use 1–63 letters, digits, '.', '_' or '-', starting with a letter or digit"
                        .into(),
            }]));
        }
        if self.store.vm_name_taken(&name, None)? {
            return Err(OpError::Conflict(format!(
                "a VM named '{name}' already exists"
            )));
        }
        let qemu_img = self
            .qemu_img()
            .await
            .ok_or_else(|| OpError::Failed(anyhow!("qemu-img is not installed")))?;
        if self.pools.get(&pool)?.readonly {
            return Err(OpError::Conflict(format!("pool '{pool}' is read-only")));
        }
        let src: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;

        // Plan: which disks get a new image, and its name.
        let mut plan: Vec<(usize, ImageRef, ImageRef)> = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        for (i, d) in src.disks.iter().enumerate() {
            if d.readonly || d.media == Media::Cdrom {
                continue;
            }
            if mode == CloneMode::Linked && !self.pools.is_template(&d.image) {
                return Err(OpError::Invalid(vec![Diagnostic {
                    severity: Severity::Error,
                    field: format!("disks[{i}].image"),
                    rule: "clone.linked-needs-template".into(),
                    message: format!(
                        "linked clones are built on templates, which never change; mark '{}/{}' as a template \
                         first (the source VM then stops using it writable), or make a full clone",
                        d.image.pool, d.image.name
                    ),
                }]));
            }
            let mut n = self
                .pools
                .free_name(&pool, &format!("{name}-disk{i}"), "qcow2")?;
            while taken.contains(&n) {
                n = self.pools.free_name(
                    &pool,
                    &format!("{name}-disk{i}-{}", taken.len()),
                    "qcow2",
                )?;
            }
            taken.push(n.clone());
            plan.push((
                i,
                d.image.clone(),
                ImageRef {
                    pool: pool.clone(),
                    name: n,
                },
            ));
        }

        let mut spec = src.clone();
        spec.name = name.clone();
        for nic in &mut spec.nics {
            nic.mac = None; // new addresses on create
        }
        for (i, _, dst) in &plan {
            spec.disks[*i].image = dst.clone();
        }
        let vars_src = self.vm_dir(id).join("VARS.qcow2");

        match mode {
            CloneMode::Linked => {
                let mut made = Vec::new();
                for (_, base, dst) in &plan {
                    match self.pools.linked_clone(&qemu_img, base, dst).await {
                        Ok(p) => made.push(p),
                        Err(e) => {
                            made.iter().for_each(|p| drop(std::fs::remove_file(p)));
                            return Err(OpError::Failed(e));
                        }
                    }
                }
                match self.create(spec).await {
                    Ok(vm) => {
                        if vars_src.exists() {
                            let _ = std::fs::copy(
                                &vars_src,
                                self.vm_dir(&vm.summary.id).join("VARS.qcow2"),
                            );
                        }
                        self.emit(
                            &vm.summary.id,
                            "cloned",
                            json!({"from": id, "mode": "linked"}),
                        );
                        Ok(CloneOutcome::Vm(Box::new(vm)))
                    }
                    Err(e) => {
                        made.iter().for_each(|p| drop(std::fs::remove_file(p)));
                        Err(e)
                    }
                }
            }
            CloneMode::Full => {
                // Validate the new spec now, so the job only fails on I/O.
                let caps = self.caps_for(&spec.arch).await?;
                let mut check = spec.clone();
                for (i, nic) in check.nics.iter_mut().enumerate() {
                    nic.mac = Some(mac_from_bytes([0, 0, i as u8]));
                }
                Self::check(&check, &caps)?;
                let mut need = 0u64;
                let mut jobs_plan = Vec::new();
                for (_, base, dst) in &plan {
                    let src_path = self.pools.resolve(base)?;
                    need += Pools::measure(&qemu_img, Some(&src_path), None, "qcow2")
                        .await
                        .map_or(0, |m| m.required);
                    jobs_plan.push((src_path, self.pools.new_image_path(dst)?));
                }
                self.pools.check_quota(Some(&qemu_img), &pool, need).await?;
                let this = self.clone();
                let src_id = id.to_string();
                let job = self.jobs.spawn(
                    "vm.clone",
                    &format!("Full clone of {} as {name}", row.name),
                    json!({"vm": id, "name": name}),
                    true,
                    move |ctx| async move {
                        let mut done: Vec<PathBuf> = Vec::new();
                        let total = jobs_plan.len();
                        let res: anyhow::Result<String> = async {
                            for (k, (from, to)) in jobs_plan.iter().enumerate() {
                                ctx.progress(
                                    None,
                                    Some(&format!("Copying disk {} of {total}", k + 1)),
                                );
                                let (f, t) = (from.display().to_string(), to.display().to_string());
                                done.push(to.clone());
                                ctx.run_qemu_img(&qemu_img, &["convert", "-O", "qcow2", &f, &t])
                                    .await?;
                            }
                            ctx.check_cancelled()?;
                            let vm = this
                                .create(spec)
                                .await
                                .map_err(|e| anyhow!("creating the VM: {e:?}"))?;
                            if vars_src.exists() {
                                std::fs::copy(
                                    &vars_src,
                                    this.vm_dir(&vm.summary.id).join("VARS.qcow2"),
                                )?;
                            }
                            this.emit(
                                &vm.summary.id,
                                "cloned",
                                json!({"from": src_id, "mode": "full"}),
                            );
                            Ok(format!("Created {}", vm.summary.name))
                        }
                        .await;
                        if res.is_err() {
                            done.iter().for_each(|p| drop(std::fs::remove_file(p)));
                        }
                        res
                    },
                );
                Ok(CloneOutcome::Job(Box::new(job)))
            }
        }
    }

    fn qmp_of(&self, id: &str) -> OpResult<Qmp> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .and_then(|l| l.qmp.clone())
            .ok_or_else(|| OpError::Conflict("the VM is not running".into()))
    }

    async fn summary_of(&self, id: &str) -> OpResult<VmSummary> {
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        Ok(self.summary(&row))
    }

    /// Ask the guest to power off: through the guest agent when the VM has
    /// one and it answers (M7-02), else ACPI. Returns immediately.
    pub async fn stop(&self, id: &str) -> OpResult<VmSummary> {
        let qmp = self.qmp_of(id)?;
        if let Some(l) = self.live.lock().unwrap().get_mut(id) {
            l.stop_requested = true;
        }
        let via_agent = match crate::qga::socket(&qmp).await {
            Some(sock) => match crate::qga::shutdown(&sock).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::debug!(vm = %id, "agent shutdown, using ACPI: {e:#}");
                    false
                }
            },
            None => false,
        };
        if !via_agent {
            qmp.execute("system_powerdown", None)
                .await
                .map_err(|e| OpError::Failed(e.into()))?;
        }
        self.emit(
            id,
            "stop-requested",
            json!({"via": if via_agent { "guest-agent" } else { "acpi" }}),
        );
        self.set_state(id, VmState::Stopping, None, None);
        self.summary_of(id).await
    }

    /// Quit QEMU now; kill it if it does not exit promptly.
    pub async fn force_stop(&self, id: &str) -> OpResult<VmSummary> {
        let (qmp, proc_id) = {
            let mut live = self.live.lock().unwrap();
            let l = live
                .get_mut(id)
                .ok_or_else(|| OpError::Conflict("the VM is not running".into()))?;
            l.stop_requested = true;
            (l.qmp.clone(), l.proc_id)
        };
        if let Some(q) = qmp {
            let _ = q
                .execute_with_timeout("quit", None, Duration::from_secs(3))
                .await;
        }
        for _ in 0..20 {
            if !procs::alive(proc_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if procs::alive(proc_id) {
            procs::terminate(proc_id, true);
        }
        self.summary_of(id).await
    }

    pub async fn qmp_action(&self, id: &str, action: &str) -> OpResult<VmSummary> {
        let cmd = match action {
            "pause" => "stop",
            "resume" => "cont",
            "reset" => "system_reset",
            _ => return Err(OpError::Conflict(format!("unknown action '{action}'"))),
        };
        let qmp = self.qmp_of(id)?;
        qmp.execute(cmd, None)
            .await
            .map_err(|e| OpError::Failed(e.into()))?;
        self.refresh_status(id, &qmp).await;
        self.summary_of(id).await
    }

    // -------------------------------------------------------------- reattach

    /// Re-attach to every run that was open when the daemon last stopped.
    pub async fn reattach_all(self: &Arc<Self>) {
        let runs = match self.store.open_runs() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("listing open runs: {e:#}");
                return;
            }
        };
        for run in runs {
            let this = self.clone();
            tokio::spawn(async move { this.reattach(run).await });
        }
    }

    async fn reattach(self: Arc<Self>, run: RunRow) {
        let id = run.vm_id.clone();
        let proc_id = ProcId {
            pid: run.pid,
            start_time: run.proc_start,
        };
        let name = self
            .store
            .get_vm(&id)
            .ok()
            .flatten()
            .map(|v| v.name)
            .unwrap_or_default();
        let was_stopping = self
            .store
            .get_vm(&id)
            .ok()
            .flatten()
            .is_some_and(|v| v.state == "stopping");
        self.live.lock().unwrap().insert(
            id.clone(),
            Live {
                run_id: run.id,
                qmp: None,
                proc_id,
                stop_requested: was_stopping,
                shutdown_seen: false,
                suspending: false,
            },
        );
        if !procs::alive(proc_id) {
            tracing::info!(vm = %name, "QEMU exited while the daemon was down");
            self.finish(&id, run.id, "exited while the daemon was not running", None);
            return;
        }
        // Bring back the switches of its isolated networks; QEMU reconnects.
        self.reacquire_switches(&id, &run.spec_json);
        let sock = PathBuf::from(&run.run_dir).join("qmp.sock");
        let qmp = match connect_with_retry(&sock, proc_id, Duration::from_secs(10)).await {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(vm = %name, "cannot reattach: {e:#}");
                self.set_state(
                    &id,
                    VmState::Crashed,
                    None,
                    Some(&format!("could not reconnect to QEMU: {e:#}")),
                );
                return;
            }
        };
        // Make sure this is the VM we think it is.
        let qname = qmp.execute("query-name", None).await.ok();
        if qname.as_ref().and_then(|v| v["name"].as_str()) != Some(name.as_str()) {
            tracing::warn!(vm = %name, "monitor belongs to a different VM; not reattaching");
            self.live.lock().unwrap().remove(&id);
            self.set_state(
                &id,
                VmState::Crashed,
                None,
                Some("run directory reused by another process"),
            );
            return;
        }
        tracing::info!(vm = %name, pid = run.pid, "reattached");
        if let Some(l) = self.live.lock().unwrap().get_mut(&id) {
            l.qmp = Some(qmp.clone());
        }
        self.emit(&id, "reattached", json!({"pid": run.pid}));
        self.refresh_status(&id, &qmp).await;
        self.monitor(id, qmp, None).await;
    }
}

/// Connect to a monitor socket, retrying until `timeout` or until the
/// process has exited.
async fn connect_with_retry(sock: &Path, proc_id: ProcId, timeout: Duration) -> Result<Qmp> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay = Duration::from_millis(50);
    loop {
        match Qmp::connect_unix(sock).await {
            Ok(q) => return Ok(q),
            Err(e) => {
                if !procs::alive(proc_id) {
                    bail!("QEMU exited before its monitor was ready");
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("monitor not reachable: {e}");
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(1));
    }
}

/// A free loopback port for VNC (Windows), in 5900+100 … 5900+999.
fn free_vnc_port() -> Result<u16> {
    let start = 100 + (rand::random::<u16>() % 900);
    for i in 0..900u16 {
        let port = 5900 + 100 + (start - 100 + i) % 900;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free VNC port in 6000–6899")
}

/// VNC passwords are limited to 8 characters by the protocol.
fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..8)
        .map(|_| ALPHABET[rand::random::<u32>() as usize % ALPHABET.len()] as char)
        .collect()
}

// ------------------------------------------------------ isolated networks

/// Releases a starting VM's host resources when dropped, unless its run was
/// recorded (`finish` owns them from then on).
struct StartLease<'a> {
    sv: &'a Supervisor,
    vm: &'a str,
    keep: bool,
}

impl Drop for StartLease<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.sv.release_host_disks(self.vm);
            self.sv.release_taps(self.vm);
            self.sv.switches.release(self.vm);
        }
    }
}

impl Supervisor {
    /// A reattached VM's isolated networks need their switches again.
    fn reacquire_switches(&self, vm_id: &str, spec_json: &str) {
        let Ok(spec) = serde_json::from_str::<VmSpec>(spec_json) else {
            return;
        };
        let networks = self.networks.list().unwrap_or_default();
        for nic in &spec.nics {
            let NicBackend::Network { network, .. } = &nic.backend else {
                continue;
            };
            if networks
                .iter()
                .any(|n| &n.id == network && n.config.needs_switch())
                && let Err(e) = self.switches.acquire(network, vm_id)
            {
                tracing::warn!(vm = %vm_id, "isolated network switch: {e}");
            }
        }
    }
}

// ------------------------------------------------------------- host disks

const HOST_EXPORTS_KEY: &str = "host-disk-exports:";

impl Supervisor {
    /// Use a different helper (tests, or a non-default install).
    pub fn set_helper(&self, h: crate::helper_client::HelperClient) {
        *self.helper.write().unwrap() = h;
    }

    pub fn helper(&self) -> crate::helper_client::HelperClient {
        self.helper.read().unwrap().clone()
    }

    /// How a physical host disk reaches QEMU in each build mode. Only an
    /// actual start asks the helper to export it; the export is recorded so
    /// that it is released when the run ends, even across daemon restarts.
    async fn resolve_host_disk(
        &self,
        vm_id: &str,
        i: usize,
        d: &qemu_ui_core::spec::DiskSpec,
        mode: &BuildMode,
    ) -> Result<ResolvedImage> {
        use qemu_ui_core::compile::NbdSource;
        let disk_id = &d.image.name;
        match mode {
            BuildMode::Preview => Ok(ResolvedImage {
                path: String::new(),
                format: "raw".into(),
                nbd: Some(NbdSource {
                    socket: "<helper export socket>".into(),
                    export: disk_id.clone(),
                }),
            }),
            BuildMode::Lint(dir) => {
                // Never touch the device: a small blank stand-in checks the
                // device model, like scratch images do for pool disks.
                let blank = dir.join(format!("disk{i}"));
                std::fs::File::create(&blank)?.set_len(64 << 20)?;
                Ok(ResolvedImage {
                    path: path_str(&blank),
                    format: "raw".into(),
                    nbd: None,
                })
            }
            BuildMode::Run => {
                let writable = !d.readonly && d.media == qemu_ui_core::spec::Media::Disk;
                let export = self.helper().export(disk_id, writable).await?;
                let key = format!("{HOST_EXPORTS_KEY}{vm_id}");
                let mut ids: Vec<String> = self
                    .store
                    .get_kv(&key)?
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                ids.push(export.export_id.clone());
                self.store.put_kv(&key, &serde_json::to_string(&ids)?)?;
                Ok(ResolvedImage {
                    path: String::new(),
                    format: "raw".into(),
                    nbd: Some(NbdSource {
                        socket: export.socket,
                        export: export.export_name,
                    }),
                })
            }
        }
    }

    /// Release a VM's host-disk exports (its run ended or failed to start).
    fn release_host_disks(&self, vm_id: &str) {
        let key = format!("{HOST_EXPORTS_KEY}{vm_id}");
        let ids: Vec<String> = self
            .store
            .get_kv(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        let _ = self.store.put_kv(&key, "[]");
        let helper = self.helper();
        let vm = vm_id.to_string();
        tokio::spawn(async move {
            for id in ids {
                if let Err(e) = helper.unexport(&id).await {
                    tracing::warn!(vm = %vm, "releasing host disk export {id}: {e:#}");
                }
            }
        });
    }
}

/// A human message for a `BLOCK_IO_ERROR` event: which disk, and whether
/// the host ran out of space.
fn io_error_message(data: &Value, spec: Option<&VmSpec>) -> String {
    let index = |s: &str, prefix: &str| -> Option<usize> {
        let rest = s.strip_prefix(prefix)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    let disk = data["device"]
        .as_str()
        .and_then(|d| index(d, "qui-dev"))
        .or_else(|| {
            data["node-name"]
                .as_str()
                .and_then(|n| index(n, "qui-disk"))
        })
        .and_then(|i| spec?.disks.get(i))
        .map(|d| format!("disk {}/{}", d.image.pool, d.image.name))
        .unwrap_or_else(|| {
            let d = data["device"].as_str().filter(|d| !d.is_empty());
            let n = data["node-name"].as_str();
            format!("disk {}", d.or(n).unwrap_or("?"))
        });
    let op = data["operation"].as_str().unwrap_or("I/O");
    if data["nospace"].as_bool() == Some(true) {
        format!(
            "The host ran out of space writing {disk}. The guest is paused and              loses nothing: free space on the host disk, then resume."
        )
    } else {
        let why = data["reason"].as_str().unwrap_or("I/O error");
        format!("A {op} on {disk} failed ({why}); the guest is paused.")
    }
}

/// Helpers shared by tests that need a real QEMU.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use qemu_ui_core::caps::{HostCaps, HostInfo};
    use qemu_ui_core::spec::*;

    pub fn find(exe: &str) -> Option<PathBuf> {
        let exe = if cfg!(windows) {
            format!("{exe}.exe")
        } else {
            exe.to_string()
        };
        let mut dirs: Vec<PathBuf> = std::env::var_os("QEMU_UI_QEMU_DIR")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
    }

    pub async fn host_caps(qemu: &Path, img: &Path) -> HostCaps {
        let host = crate::scan::host_info();
        let bin = qemu_ui_core::caps::Binary {
            id: "t".into(),
            path: qemu.display().to_string(),
            arch: Some("x86_64".into()),
            version: String::new(),
            sha256: String::new(),
            source: "path".into(),
        };
        let caps = crate::probe::probe_binary(bin, &host).await.unwrap();
        HostCaps {
            host: HostInfo {
                os: host.os,
                arch: host.arch,
            },
            binaries: vec![caps],
            failed: vec![],
            qemu_img: Some(qemu_ui_core::caps::Binary {
                id: "i".into(),
                path: img.display().to_string(),
                arch: None,
                version: String::new(),
                sha256: String::new(),
                source: "path".into(),
            }),
            firmware: vec![],
            parity: vec![],
            scanned_at: 0,
        }
    }

    pub async fn make(data: &Path, caps: &HostCaps) -> Arc<Supervisor> {
        let store = Store::open(&data.join("t.db")).unwrap();
        let scanner = Scanner::new(store.clone(), vec![], vec![]);
        *scanner.caps.write().await = Some(caps.clone());
        Supervisor::new(store, scanner, data, data.join("r")).unwrap()
    }

    pub async fn wait_state(sv: &Supervisor, id: &str, want: VmState) -> VmSummary {
        for _ in 0..100 {
            let s = sv.detail(id).ok().unwrap().summary;
            if s.state == want {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "VM never reached {want:?}: {:?}",
            sv.detail(id).ok().map(|d| d.summary)
        );
    }

    /// Kills every QEMU the test's store still records as running when it
    /// is dropped, including during a panic. VMs are spawned detached, so
    /// without this a failing assertion leaves them running.
    pub struct KillVmsOnDrop(pub PathBuf);

    impl Drop for KillVmsOnDrop {
        fn drop(&mut self) {
            let Ok(store) = Store::open(&self.0.join("t.db")) else {
                return;
            };
            for run in store.open_runs().unwrap_or_default() {
                procs::terminate(
                    ProcId {
                        pid: run.pid,
                        start_time: run.proc_start,
                    },
                    true,
                );
            }
        }
    }

    /// A short data dir (unix socket paths are limited to 108 bytes).
    pub fn short_data_dir(tag: &str) -> PathBuf {
        let base = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let data = base.join(format!("qui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        data
    }

    /// Create a small TCG VM with one disk; returns its id.
    pub async fn small_vm(sv: &Supervisor, caps: &HostCaps, name: &str) -> String {
        let img = ImageRef {
            pool: "default".into(),
            name: format!("{name}.qcow2"),
        };
        let qemu_img = PathBuf::from(&caps.qemu_img.as_ref().unwrap().path);
        sv.pools
            .create(&qemu_img, &img, 32 << 20, "qcow2")
            .await
            .unwrap();
        let (mut spec, _) = qemu_ui_core::vm::default_spec(name, &caps.binaries[0]);
        spec.accel = "tcg".into();
        spec.cpu.model = Some("qemu64".into());
        spec.memory.size_mib = 128;
        spec.disks.push(DiskSpec {
            image: img,
            bus: DiskBus::Virtio,
            media: Media::Disk,
            readonly: false,
            boot_index: Some(1),
            cache: CacheMode::None,
            aio: None,
            discard: true,
            slot: None,
        });
        sv.create(spec)
            .await
            .map_err(|e| format!("{e:?}"))
            .unwrap()
            .summary
            .id
    }
}

/// Lifecycle against a real QEMU: create → start → pause → resume →
/// "daemon restart" (drop the runtime, as a killed daemon would) → reattach
/// → force-stop → start → external kill → crashed. Skipped without QEMU.
#[cfg(test)]
mod lifecycle {
    use super::test_support::*;
    use super::*;
    use qemu_ui_core::spec::*;

    #[test]
    fn full_lifecycle_with_reattach() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let base = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let data = base.join(format!("qui-lc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();

        // --- first "daemon": create, start, pause/resume ---
        let rt1 = tokio::runtime::Runtime::new().unwrap();
        let (caps, id, pid) = rt1.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            sv.pools
                .create(
                    &img,
                    &ImageRef {
                        pool: "default".into(),
                        name: "d.qcow2".into(),
                    },
                    32 << 20,
                    "qcow2",
                )
                .await
                .unwrap();
            let (mut spec, _) = qemu_ui_core::vm::default_spec("lifecycle", &caps.binaries[0]);
            spec.accel = "tcg".into();
            spec.cpu.model = Some("qemu64".into());
            spec.memory.size_mib = 128;
            spec.disks.push(DiskSpec {
                image: ImageRef {
                    pool: "default".into(),
                    name: "d.qcow2".into(),
                },
                bus: DiskBus::Virtio,
                media: Media::Disk,
                readonly: false,
                boot_index: Some(1),
                cache: CacheMode::Writeback,
                aio: None,
                discard: true,
                slot: None,
            });
            let vm = sv.create(spec).await.map_err(|e| format!("{e:?}")).unwrap();
            let id = vm.summary.id.clone();
            assert!(
                vm.spec.machine.type_name.starts_with("pc-q35-"),
                "alias pinned"
            );
            assert!(vm.spec.nics[0].mac.is_some(), "MAC assigned");

            let s = sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert_eq!(s.state, VmState::Running);
            assert!(s.has_vnc);
            assert!(sv.vnc_of(&id).is_some());

            let s = sv
                .qmp_action(&id, "pause")
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(
                (s.state, s.pause_reason.as_deref()),
                (VmState::Paused, Some("user"))
            );
            sv.qmp_action(&id, "resume")
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Running).await;
            let pid = sv.store.last_run(&id).unwrap().unwrap().pid;
            (caps, id, pid)
        });
        // The daemon "dies": every task and connection is dropped. QEMU must survive.
        drop(rt1);
        let ident = {
            let run = Store::open(&data.join("t.db"))
                .unwrap()
                .last_run(&id)
                .unwrap()
                .unwrap();
            ProcId {
                pid: run.pid,
                start_time: run.proc_start,
            }
        };
        assert!(procs::alive(ident), "QEMU must outlive the daemon");

        // --- second "daemon": reattach, stop, start, crash ---
        let rt2 = tokio::runtime::Runtime::new().unwrap();
        rt2.block_on(async {
            let sv = make(&data, &caps).await;
            sv.reattach_all().await;
            wait_for(|| {
                sv.live
                    .lock()
                    .unwrap()
                    .get(&id)
                    .is_some_and(|l| l.qmp.is_some())
            })
            .await;
            let s = wait_state(&sv, &id, VmState::Running).await;
            assert_eq!(
                sv.store.last_run(&id).unwrap().unwrap().pid,
                pid,
                "same process"
            );
            assert!(s.started_at.is_some());

            // QMP works through the reattached monitor.
            let s = sv
                .qmp_action(&id, "pause")
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert_eq!(s.state, VmState::Paused);

            // No guest OS answers ACPI, so force-stop.
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let s = wait_state(&sv, &id, VmState::Stopped).await;
            assert!(s.last_error.is_none(), "{:?}", s.last_error);
            assert!(!procs::alive(ident));

            // Start again, then kill QEMU behind the supervisor's back.
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let run = sv.store.last_run(&id).unwrap().unwrap();
            procs::terminate(
                ProcId {
                    pid: run.pid,
                    start_time: run.proc_start,
                },
                true,
            );
            let s = wait_state(&sv, &id, VmState::Crashed).await;
            assert!(
                s.last_error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("unexpectedly"),
                "{:?}",
                s.last_error
            );

            // Events were recorded throughout.
            let kinds: Vec<String> = sv
                .store
                .events_after(0, 1000)
                .unwrap()
                .into_iter()
                .map(|e| e.3)
                .collect();
            for k in [
                "created",
                "started",
                "STOP",
                "RESUME",
                "reattached",
                "exited",
            ] {
                assert!(kinds.iter().any(|x| x == k), "missing event {k}: {kinds:?}");
            }

            sv.delete(&id).await.map_err(|e| format!("{e:?}")).unwrap();
        });
        drop(rt2);
        let _ = std::fs::remove_dir_all(&data);
    }

    async fn wait_for(f: impl Fn() -> bool) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("condition not met");
    }
}

/// "Check config" against a real QEMU, including while the VM runs: no lock,
/// port or socket conflicts; errors map to fields; results are cached.
#[cfg(test)]
mod dry_lint {
    use super::test_support::*;
    use super::*;
    use qemu_ui_core::spec::*;

    #[test]
    fn lint_is_side_effect_free_and_maps_errors() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("lint");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "linted").await;
            // Give it a port forward the lint must not try to bind again.
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.nics[0].backend.forwards_mut().push(PortForward {
                proto: "tcp".into(),
                host_addr: "127.0.0.1".into(),
                host_port: 45222,
                guest_port: 22,
            });
            sv.update(&id, spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();

            // Same config, VM running: must pass.
            let r = sv
                .lint(Some(&id), spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(r.ok, "{:#?}", r.diagnostics);
            assert!(!r.cached);
            let again = sv
                .lint(Some(&id), spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(again.cached);

            // A bad passthrough option: QEMU rejects it, pinned to the token.
            let mut bad = spec.clone();
            bad.passthrough = vec!["-global".into(), "ICH9-LPC.no-such-property=1".into()];
            let r = sv
                .lint(Some(&id), bad)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            assert!(!r.ok);
            assert!(
                r.diagnostics
                    .iter()
                    .any(|d| d.field == "passthrough[1]" && d.severity == Severity::Error),
                "{:#?}",
                r.diagnostics
            );

            // The running VM is untouched.
            assert_eq!(sv.detail(&id).ok().unwrap().summary.state, VmState::Running);
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            // No scratch directories are left behind.
            let leftovers: Vec<_> = std::fs::read_dir(data.join("r"))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("lint-"))
                .collect();
            assert!(leftovers.is_empty());
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// Physical host disks end to end (M2-11) against a real QEMU, with a
/// stand-in helper: it speaks the real protocol and serves a file through a
/// real qemu-storage-daemon, as the privileged helper does for a device once
/// its checks pass (those checks are tested in the helper crate).
#[cfg(test)]
mod host_disks {
    use super::test_support::*;
    use super::*;
    use qemu_ui_core::helper::{DiskExport, Envelope, Request, Response};
    use qemu_ui_core::spec::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeHelper {
        unexports: Arc<AtomicUsize>,
    }

    /// Serve the helper protocol on `sock`, exporting `file` on request.
    fn fake_helper(sock: PathBuf, file: PathBuf, qsd: PathBuf, dir: PathBuf) -> FakeHelper {
        #[cfg(unix)]
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        #[cfg(windows)]
        let listener = uds_windows::UnixListener::bind(&sock).unwrap();
        let unexports = Arc::new(AtomicUsize::new(0));
        let count = unexports.clone();
        std::thread::spawn(move || {
            let mut children: HashMap<String, std::process::Child> = HashMap::new();
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let mut w = conn.try_clone().unwrap();
                for line in BufReader::new(conn).lines() {
                    let Ok(line) = line else { break };
                    let env: Envelope<Request> = serde_json::from_str(&line).unwrap();
                    let body = match env.body {
                        Request::Hello { version, .. } => Response::Hello { version },
                        Request::ExportDisk { disk_id, writable } => {
                            let id = format!("e{}", children.len());
                            let s = dir.join(format!("{id}.sock"));
                            let s_str = s.display().to_string().replace('\\', "/");
                            let bd = serde_json::json!({"node-name": "disk", "driver": "file",
                                "filename": file.display().to_string(), "read-only": !writable});
                            let child = std::process::Command::new(&qsd)
                                .arg("--blockdev")
                                .arg(bd.to_string())
                                .arg("--nbd-server")
                                .arg(format!("addr.type=unix,addr.path={s_str}"))
                                .arg("--export")
                                .arg(format!(
                                    "type=nbd,id={id},node-name=disk,name={id},writable=off"
                                ))
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                                .unwrap();
                            for _ in 0..100 {
                                if s.exists() {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            children.insert(id.clone(), child);
                            Response::Exported {
                                export: DiskExport {
                                    export_id: id.clone(),
                                    disk_id,
                                    writable,
                                    socket: s_str,
                                    export_name: id,
                                },
                            }
                        }
                        Request::Unexport { export_id } => {
                            if let Some(mut c) = children.remove(&export_id) {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            count.fetch_add(1, Ordering::SeqCst);
                            Response::Unexported
                        }
                        _ => Response::Error {
                            message: "not supported by the test helper".into(),
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
        FakeHelper { unexports }
    }

    #[test]
    fn host_disk_is_exported_attached_over_nbd_and_released() {
        let (Some(qemu), Some(img), Some(qsd)) = (
            find("qemu-system-x86_64"),
            find("qemu-img"),
            find("qemu-storage-daemon"),
        ) else {
            eprintln!("skipping: QEMU tools not found");
            return;
        };
        let data = short_data_dir("hd");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "hostdisk").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.disks.push(DiskSpec {
                image: ImageRef { pool: HOST_DISK_POOL.into(), name: "usb-Example_Stick_0001".into() },
                bus: DiskBus::Virtio,
                media: Media::Disk,
                readonly: true,
                boot_index: None,
                cache: CacheMode::None,
                aio: None,
                discard: false,
                slot: None,
            });
            sv.update(&id, spec).await.map_err(|e| format!("{e:?}")).unwrap();

            // No helper installed: a clear error, and the VM is not left starting.
            sv.set_helper(crate::helper_client::HelperClient { socket: data.join("none.sock"), secret_file: None });
            let err = sv.start(&id).await.err().map(|e| format!("{e:?}")).unwrap_or_default();
            assert!(err.contains("helper is not running"), "{err}");
            assert_ne!(sv.detail(&id).ok().unwrap().summary.state, VmState::Starting);

            // With a helper: exported, attached over NBD, released on stop.
            let stick = data.join("stick.raw");
            std::fs::File::create(&stick).unwrap().set_len(16 << 20).unwrap();
            let hdir = data.join("h");
            std::fs::create_dir_all(&hdir).unwrap();
            let fake = fake_helper(hdir.join("helper.sock"), stick, qsd, hdir.clone());
            sv.set_helper(crate::helper_client::HelperClient { socket: hdir.join("helper.sock"), secret_file: None });

            let s = sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert_eq!(s.state, VmState::Running);
            let qmp = sv.live.lock().unwrap().get(&id).and_then(|l| l.qmp.clone()).unwrap();
            let nodes = qmp.execute("query-named-block-nodes", None).await.unwrap();
            let node = nodes.as_array().unwrap().iter().find(|n| n["node-name"] == "qui-disk1-file").cloned();
            assert_eq!(node.as_ref().map(|n| n["drv"].clone()), Some(serde_json::json!("nbd")), "{nodes}");

            sv.force_stop(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
            for _ in 0..50 {
                if fake.unexports.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(fake.unexports.load(Ordering::SeqCst), 1, "the export is released when the run ends");

            // A start that fails after the export (here: an image operation
            // holds the boot disk) still gives the export back.
            let boot = sv.detail(&id).ok().unwrap().spec.disks[0].image.clone();
            let held = crate::image_ops::lock_for_start(&[boot]).unwrap();
            assert!(matches!(sv.start(&id).await, Err(OpError::Conflict(_))));
            drop(held);
            for _ in 0..50 {
                if fake.unexports.load(Ordering::SeqCst) > 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(fake.unexports.load(Ordering::SeqCst), 2, "a failed start releases the export");
            assert_ne!(sv.detail(&id).ok().unwrap().summary.state, VmState::Starting);

            // Live hotplug of host disks is refused with a reason.
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let extra = DiskSpec {
                image: ImageRef { pool: HOST_DISK_POOL.into(), name: "other".into() },
                bus: DiskBus::Virtio,
                media: Media::Disk,
                readonly: true,
                boot_index: None,
                cache: CacheMode::None,
                aio: None,
                discard: false,
                slot: None,
            };
            assert!(matches!(sv.add_disk(&id, extra).await, Err(OpError::Conflict(m)) if m.contains("stopped")));
            sv.force_stop(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// A plain cloud-init password sent to the API is stored only as a
/// SHA-512-crypt hash (M7-03); hashes are kept as they are.
fn hash_guest_password(spec: &mut VmSpec) -> OpResult<()> {
    use sha_crypt::{PasswordHasher, ShaCrypt};
    let Some(ci) = spec.guest.cloud_init.as_mut() else {
        return Ok(());
    };
    match ci.password.as_deref() {
        None => {}
        Some("") => ci.password = None,
        Some(p) if qemu_ui_core::guest::is_password_hash(p) => {}
        Some(p) => {
            let salt: [u8; 12] = rand::random();
            let hash = ShaCrypt::default()
                .hash_password_with_salt(p.as_bytes(), &salt)
                .map_err(|e| OpError::Failed(anyhow::anyhow!("hashing the password: {e}")))?;
            ci.password = Some(hash.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod guest_password_tests {
    use super::*;

    #[test]
    fn plain_passwords_are_hashed_and_hashes_kept() {
        use sha_crypt::{PasswordVerifier, ShaCrypt};
        let mut spec: VmSpec = serde_json::from_value(serde_json::json!({
            "spec_version": 1, "name": "t", "arch": "x86_64",
            "machine": {"type": "q35"}, "cpu": {"count": 1}, "memory": {"size_mib": 64},
            "guest": {"cloud_init": {"password": "s3cret"}}
        }))
        .unwrap();
        hash_guest_password(&mut spec).unwrap();
        let h = spec
            .guest
            .cloud_init
            .as_ref()
            .unwrap()
            .password
            .clone()
            .unwrap();
        assert!(h.starts_with("$6$"), "{h}");
        assert!(
            ShaCrypt::default()
                .verify_password(b"s3cret", h.as_str())
                .is_ok()
        );
        let before = spec.clone();
        hash_guest_password(&mut spec).unwrap();
        assert_eq!(spec, before, "an existing hash is kept");
    }
}
