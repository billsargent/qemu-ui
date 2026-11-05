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
use qemu_ui_core::caps::BinaryCaps;
use qemu_ui_core::compile::{
    self, Compiled, Diagnostic, ResolvedFirmware, ResolvedImage, RunPaths, Severity, VncListen,
};
use qemu_ui_core::spec::{FirmwareSpec, ImageRef, VmSpec};
use qemu_ui_core::vm::{VmDetail, VmEvent, VmState, VmSummary, mac_from_bytes, state_from_qmp};
use qemu_ui_qmp::{Qmp, transport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::images::Pools;
use crate::procs::{self, ProcId};
use crate::scan::Scanner;
use crate::store::{NewRun, RunRow, Store};

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
}

pub struct Supervisor {
    pub store: Store,
    pub scanner: Arc<Scanner>,
    pub pools: Pools,
    data_dir: PathBuf,
    run_root: PathBuf,
    events: broadcast::Sender<VmEvent>,
    live: Mutex<HashMap<String, Live>>,
    /// Serialises lifecycle operations (start/stop/delete/edit).
    ops: tokio::sync::Mutex<()>,
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
        Ok(Arc::new(Supervisor {
            store,
            scanner,
            pools: Pools::new(data_dir)?,
            data_dir: data_dir.to_path_buf(),
            run_root,
            events,
            live: Mutex::default(),
            ops: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<VmEvent> {
        self.events.subscribe()
    }

    fn vm_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("vms").join(id)
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.run_root.join(id)
    }

    // ---------------------------------------------------------------- events

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

    async fn qemu_img(&self) -> Option<PathBuf> {
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
        VmSummary {
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
        let last_run_args = self
            .store
            .last_run(id)?
            .and_then(|r| serde_json::from_str::<Vec<String>>(&r.args_json).ok());
        Ok(VmDetail {
            summary: self.summary(&row),
            spec,
            last_run_args,
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
        for nic in &mut spec.nics {
            if nic.mac.is_none() {
                nic.mac = Some(mac_from_bytes(rand::random()));
            }
        }
        Ok((spec, caps))
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
        self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        let (spec, caps) = self.normalise(spec).await?;
        Self::check(&spec, &caps)?;
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

    /// Resolve images and firmware and compile. `run` = for an actual start:
    /// creates the NVRAM copy and allocates a VNC endpoint.
    async fn build(
        &self,
        id: &str,
        spec: &VmSpec,
        caps: &BinaryCaps,
        run: bool,
    ) -> Result<(Compiled, Option<VncInfo>), Vec<Diagnostic>> {
        let fail = |field: &str, rule: &str, msg: String| {
            vec![Diagnostic {
                severity: Severity::Error,
                field: field.into(),
                rule: rule.into(),
                message: msg,
            }]
        };
        let qemu_img = self.qemu_img().await;
        let mut images: BTreeMap<ImageRef, ResolvedImage> = BTreeMap::new();
        for (i, d) in spec.disks.iter().enumerate() {
            let Ok(path) = self.pools.resolve(&d.image) else {
                continue;
            }; // compile reports it
            let format = match &qemu_img {
                Some(q) => Pools::info(q, &path)
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
            images.insert(
                d.image.clone(),
                ResolvedImage {
                    path: path_str(&path),
                    format,
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
                let vars_path = self.vm_dir(id).join("VARS.qcow2");
                let vars = match fw.nvram_template.as_ref().and_then(|t| t.resolved.clone()) {
                    Some(template) => {
                        if run && !vars_path.exists() {
                            let Some(q) = &qemu_img else {
                                return Err(fail(
                                    "firmware",
                                    "qemu-img.missing",
                                    "qemu-img is not installed".into(),
                                ));
                            };
                            let _ = std::fs::create_dir_all(self.vm_dir(id));
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

        let dir = self.run_dir(id);
        let s = |n: &str| path_str(&dir.join(n));
        let vnc = if !spec.display.vnc {
            None
        } else if cfg!(windows) {
            let port = if run {
                free_vnc_port().map_err(|e| fail("display.vnc", "vnc.port", e.to_string()))?
            } else {
                5900
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
        let ctx = compile::Context {
            caps,
            os: std::env::consts::OS,
            run: RunPaths {
                qmp: s("qmp.sock"),
                qmp_user: s("qmp-user.sock"),
                serial: s("serial.sock"),
                serial_log: path_str(&self.vm_dir(id).join("serial.log")),
                pidfile: s("qemu.pid"),
                vnc: vnc_listen,
            },
            images: &images,
            firmware,
        };
        compile::compile(spec, &ctx).map(|c| (c, vnc))
    }

    /// Compile without side effects, for the live argv drawer.
    pub async fn preview(
        &self,
        id: Option<&str>,
        spec: VmSpec,
    ) -> OpResult<(Compiled, Vec<Diagnostic>)> {
        let (spec, caps) = self.normalise(spec).await?;
        match self.build(id.unwrap_or("new"), &spec, &caps, false).await {
            Ok((c, _)) => {
                let w = c.warnings.clone();
                Ok((c, w))
            }
            Err(d) => Err(OpError::Invalid(d)),
        }
    }

    // ------------------------------------------------------------ lifecycle

    pub async fn start(self: &Arc<Self>, id: &str) -> OpResult<VmSummary> {
        let _g = self.ops.lock().await;
        let row = self.store.get_vm(id)?.ok_or(OpError::NotFound)?;
        if VmState::parse(&row.state).is_active() || self.live.lock().unwrap().contains_key(id) {
            return Err(OpError::Conflict("the VM is already running".into()));
        }
        let spec: VmSpec =
            serde_json::from_str(&row.spec_json).map_err(|e| OpError::Failed(e.into()))?;
        let caps = self.caps_for(&spec.arch).await?;
        let (compiled, vnc) = self
            .build(id, &spec, &caps, true)
            .await
            .map_err(OpError::Invalid)?;

        let run_dir = self.run_dir(id);
        let _ = std::fs::remove_dir_all(&run_dir); // stale sockets from a crashed run
        std::fs::create_dir_all(&run_dir).map_err(|e| OpError::Failed(e.into()))?;
        let logs = self.vm_dir(id).join("logs");
        std::fs::create_dir_all(&logs).map_err(|e| OpError::Failed(e.into()))?;
        let log = logs.join(format!("run-{}.log", now()));

        self.store.clear_vm_error(id)?;
        self.set_state(id, VmState::Starting, None, None);
        tracing::info!(vm = %row.name, "starting: {}", compile::display_command(&compiled.program, &compiled.args));

        let (child, proc_id) = match procs::spawn_detached(&compiled.program, &compiled.args, &log)
        {
            Ok(x) => x,
            Err(e) => {
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
            spec_json: &row.spec_json,
            accel: &compiled.accel,
            pid: proc_id.pid,
            proc_start: proc_id.start_time,
            run_dir: &path_str(&run_dir),
            vnc_json: vnc_json.as_deref(),
            log_path: &path_str(&log),
            started_at: now(),
        })?;
        self.live.lock().unwrap().insert(
            id.into(),
            Live {
                run_id,
                qmp: None,
                proc_id,
                stop_requested: false,
                shutdown_seen: false,
            },
        );

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

        match self.bring_up(id, &run_dir, proc_id, vnc.as_ref()).await {
            Ok(qmp) => {
                if let Some(l) = self.live.lock().unwrap().get_mut(id) {
                    l.qmp = Some(qmp.clone());
                }
                self.set_state(id, VmState::Running, None, None);
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
    ) -> Result<Qmp> {
        let qmp =
            connect_with_retry(&run_dir.join("qmp.sock"), proc_id, QMP_CONNECT_TIMEOUT).await?;
        let st = qmp.execute("query-status", None).await?;
        let status = st["status"].as_str().unwrap_or_default();
        if status != "prelaunch" {
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
        self.emit(id, &kind, data);
        match kind.as_str() {
            "SHUTDOWN" => {
                if let Some(l) = self.live.lock().unwrap().get_mut(id) {
                    l.shutdown_seen = true;
                }
                self.set_state(id, VmState::Stopping, None, None);
            }
            "STOP" | "RESUME" | "RESET" | "SUSPEND" | "WAKEUP" | "GUEST_PANICKED"
            | "BLOCK_IO_ERROR" => {
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
        let expected = live
            .as_ref()
            .is_some_and(|l| l.stop_requested || l.shutdown_seen);
        let _ = self.store.end_run(run_id, now(), exit_status);
        let _ = std::fs::remove_dir_all(self.run_dir(id));
        if expected && error.is_none() {
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
        }
        self.emit(id, "exited", json!({"status": exit_status}));
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

    /// Ask the guest to power off (ACPI). Returns immediately.
    pub async fn stop(&self, id: &str) -> OpResult<VmSummary> {
        let qmp = self.qmp_of(id)?;
        if let Some(l) = self.live.lock().unwrap().get_mut(id) {
            l.stop_requested = true;
        }
        qmp.execute("system_powerdown", None)
            .await
            .map_err(|e| OpError::Failed(e.into()))?;
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
            },
        );
        if !procs::alive(proc_id) {
            tracing::info!(vm = %name, "QEMU exited while the daemon was down");
            self.finish(&id, run.id, "exited while the daemon was not running", None);
            return;
        }
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

/// Lifecycle against a real QEMU: create → start → pause → resume →
/// "daemon restart" (drop the runtime, as a killed daemon would) → reattach
/// → force-stop → start → external kill → crashed. Skipped without QEMU.
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
        });
        sv.create(spec)
            .await
            .map_err(|e| format!("{e:?}"))
            .unwrap()
            .summary
            .id
    }
}

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
