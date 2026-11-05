//! Spec → argv compiler (plan.md §5.2) and its validator (§6.1).
//!
//! `compile` is a pure function of the spec, the capability snapshot of the
//! chosen binary, the platform, and host paths resolved by the daemon. The
//! output is an argv array, never a shell string. Rules:
//!
//! * `-nodefaults -no-user-config`: everything is explicit.
//! * JSON syntax for `-blockdev`, `-device`, `-netdev`, so file names can
//!   contain any character. Keyval options double every `,` in values.
//! * Our ids live in the reserved `qui-` namespace; passthrough may not use it.
//! * Always `-S`: the supervisor checks the VM and then sends `cont` (§6.3).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::caps::BinaryCaps;
use crate::spec::{
    CacheMode, DiskBus, DiskSpec, FirmwareSpec, ImageRef, Media, NicBackend, VmSpec,
    valid_image_name, valid_mac, valid_name,
};

/// Prefix reserved for ids we generate.
pub const RESERVED_PREFIX: &str = "qui-";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum Severity {
    Error,
    Warning,
}

/// A validation finding pinned to a spec field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Field path, e.g. `disks[0].bus`, `cpu.count`, `passthrough[3]`.
    pub field: String,
    /// Stable rule id, e.g. `cpu.max-exceeded`.
    pub rule: String,
    pub message: String,
}

impl Diagnostic {
    fn error(field: impl Into<String>, rule: &str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            field: field.into(),
            rule: rule.into(),
            message: message.into(),
        }
    }
    fn warning(field: impl Into<String>, rule: &str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            field: field.into(),
            rule: rule.into(),
            message: message.into(),
        }
    }
}

/// How the VNC server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VncListen {
    /// Unix socket (Linux).
    Unix(String),
    /// Loopback TCP display number (port 5900+n) with `password=on`; the
    /// supervisor sets a per-run password via QMP (Windows).
    Tcp { display: u16 },
}

/// Per-run host paths chosen by the supervisor.
#[derive(Debug, Clone)]
pub struct RunPaths {
    pub qmp: String,
    pub qmp_user: String,
    pub serial: String,
    pub serial_log: String,
    pub pidfile: String,
    pub vnc: VncListen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub path: String,
    /// `qcow2`, `raw`, … from `qemu-img info`.
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFirmware {
    pub code: String,
    pub code_format: String,
    /// Per-VM NVRAM copy (qcow2).
    pub vars: Option<String>,
    pub vars_format: String,
    pub secure_boot: bool,
    pub requires_smm: bool,
}

/// Everything the compiler needs besides the spec.
pub struct Context<'a> {
    pub caps: &'a BinaryCaps,
    /// `linux`, `windows`, `macos`.
    pub os: &'a str,
    pub run: RunPaths,
    pub images: &'a BTreeMap<ImageRef, ResolvedImage>,
    /// Required when the spec uses `FirmwareSpec::Descriptor`.
    pub firmware: Option<ResolvedFirmware>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub program: String,
    pub args: Vec<String>,
    /// The accelerator actually chosen (resolves `auto`).
    pub accel: String,
    pub warnings: Vec<Diagnostic>,
}

/// Double commas for QemuOpts/keyval values.
pub fn kv(value: &str) -> String {
    value.replace(',', ",,")
}

/// Resolve a machine alias (`q35`, `pc`, `virt`) to the versioned machine it
/// points at, so the guest ABI stays fixed across QEMU upgrades (§5.2).
pub fn pin_machine(name: &str, caps: &BinaryCaps) -> Result<String, String> {
    if let Some(m) = caps
        .machines
        .iter()
        .find(|m| m.alias.as_deref() == Some(name))
    {
        return Ok(m.name.clone());
    }
    if caps.machines.iter().any(|m| m.name == name) {
        return Ok(name.to_string());
    }
    Err(format!(
        "machine '{name}' is not supported by qemu-system-{}",
        caps.arch
    ))
}

/// Pick the accelerator for `spec.accel` (`auto` → best available).
pub fn resolve_accel(spec: &VmSpec, caps: &BinaryCaps) -> Result<String, Diagnostic> {
    if spec.accel == "auto" {
        return caps.best_accel().map(|a| a.name.clone()).ok_or_else(|| {
            Diagnostic::error("accel", "accel.none", "no accelerator is available")
        });
    }
    match caps.accels.iter().find(|a| a.name == spec.accel) {
        Some(a) if a.available => Ok(a.name.clone()),
        Some(a) => Err(Diagnostic::error(
            "accel",
            "accel.unavailable",
            format!(
                "{} is not available: {}",
                a.name,
                a.reason.clone().unwrap_or_else(|| "unknown reason".into())
            ),
        )),
        None => Err(Diagnostic::error(
            "accel",
            "accel.unknown",
            format!("this QEMU does not support accelerator '{}'", spec.accel),
        )),
    }
}

fn has_device(caps: &BinaryCaps, name: &str) -> bool {
    caps.devices.iter().any(|d| d.name == name)
}

/// Device model used for a disk on `bus` with `media`.
fn disk_device(bus: DiskBus, media: Media) -> Result<&'static str, &'static str> {
    Ok(match (bus, media) {
        (DiskBus::Virtio, Media::Disk) => "virtio-blk-pci",
        (DiskBus::Virtio, Media::Cdrom) => {
            return Err("virtio-blk cannot present a CD-ROM; use the SCSI or IDE bus");
        }
        (DiskBus::Scsi, Media::Disk) => "scsi-hd",
        (DiskBus::Scsi, Media::Cdrom) => "scsi-cd",
        (DiskBus::Nvme, Media::Disk) => "nvme",
        (DiskBus::Nvme, Media::Cdrom) => {
            return Err("NVMe cannot present a CD-ROM; use the SCSI or IDE bus");
        }
        (DiskBus::Ide, Media::Disk) => "ide-hd",
        (DiskBus::Ide, Media::Cdrom) => "ide-cd",
        (DiskBus::Usb, Media::Disk) => "usb-storage",
        (DiskBus::Usb, Media::Cdrom) => "usb-storage",
    })
}

/// Singleton options that the compiler owns; passthrough may not repeat them.
const OWNED_OPTIONS: &[&str] = &[
    "-machine",
    "-M",
    "-accel",
    "-enable-kvm",
    "-m",
    "-smp",
    "-cpu",
    "-bios",
    "-pflash",
    "-name",
    "-pidfile",
    "-qmp",
    "-qmp-pretty",
    "-monitor",
    "-mon",
    "-serial",
    "-vnc",
    "-display",
    "-nographic",
    "-nodefaults",
    "-no-user-config",
    "-daemonize",
    "-S",
    "-runas",
    "-chroot",
    "-sandbox",
];

/// Validate a spec against a capability snapshot. Paths/images are checked
/// by `compile` (they need the context).
pub fn validate(spec: &VmSpec, caps: &BinaryCaps) -> Vec<Diagnostic> {
    let mut d = Vec::new();

    if !valid_name(&spec.name) {
        d.push(Diagnostic::error(
            "name",
            "name.invalid",
            "use 1–63 letters, digits, '.', '_' or '-', starting with a letter or digit",
        ));
    }
    if spec.arch != caps.arch {
        d.push(Diagnostic::error(
            "arch",
            "arch.mismatch",
            format!(
                "spec is for {} but the binary emulates {}",
                spec.arch, caps.arch
            ),
        ));
    }

    // Machine
    let machine = caps
        .machines
        .iter()
        .find(|m| m.name == spec.machine.type_name);
    match machine {
        None if caps.machines.iter().any(|m| m.alias.as_deref() == Some(&*spec.machine.type_name)) => {
            d.push(Diagnostic::error(
                "machine.type",
                "machine.unpinned-alias",
                format!(
                    "'{}' is an alias; it must be pinned to a versioned machine when the VM is created",
                    spec.machine.type_name
                ),
            ))
        }
        None => d.push(Diagnostic::error(
            "machine.type",
            "machine.unknown",
            format!("machine '{}' is not supported by this QEMU", spec.machine.type_name),
        )),
        Some(m) if m.deprecated => d.push(Diagnostic::warning(
            "machine.type",
            "machine.deprecated",
            format!("machine '{}' is deprecated and will be removed from a future QEMU", m.name),
        )),
        Some(_) => {}
    }

    // Accelerator
    let accel = match resolve_accel(spec, caps) {
        Ok(a) => Some(a),
        Err(e) => {
            d.push(e);
            None
        }
    };
    if accel.as_deref() == Some("tcg") && caps.native {
        d.push(Diagnostic::warning(
            "accel",
            "accel.tcg-on-native",
            "running without hardware acceleration; the guest will be much slower",
        ));
    }

    // CPU
    if spec.cpu.count == 0 {
        d.push(Diagnostic::error(
            "cpu.count",
            "cpu.zero",
            "at least one vCPU is required",
        ));
    }
    if let Some(max) = machine.and_then(|m| m.cpu_max)
        && spec.cpu.count > max
    {
        d.push(Diagnostic::error(
            "cpu.count",
            "cpu.max-exceeded",
            format!(
                "machine '{}' supports at most {max} vCPUs",
                spec.machine.type_name
            ),
        ));
    }
    if let Some(t) = spec.cpu.topology {
        if t.sockets == 0 || t.cores == 0 || t.threads == 0 {
            d.push(Diagnostic::error(
                "cpu.topology",
                "cpu.topology-zero",
                "sockets, cores and threads must be at least 1",
            ));
        } else if t.sockets * t.cores * t.threads != spec.cpu.count {
            d.push(Diagnostic::error(
                "cpu.topology",
                "cpu.topology-mismatch",
                format!(
                    "{} sockets × {} cores × {} threads = {}, but the VM has {} vCPUs",
                    t.sockets,
                    t.cores,
                    t.threads,
                    t.sockets * t.cores * t.threads,
                    spec.cpu.count
                ),
            ));
        }
    }
    if let Some(model) = &spec.cpu.model {
        let hardware = accel.as_deref().is_some_and(|a| a != "tcg");
        if model == "host" && !hardware {
            d.push(Diagnostic::error(
                "cpu.model",
                "cpu.host-needs-accel",
                "'host' CPU requires hardware acceleration",
            ));
        } else if !caps.cpus.is_empty()
            && model != "host"
            && !caps.cpus.iter().any(|c| &c.name == model)
        {
            d.push(Diagnostic::error(
                "cpu.model",
                "cpu.unknown",
                format!("CPU model '{model}' is not supported"),
            ));
        }
    }

    // Memory
    if spec.memory.size_mib < 16 {
        d.push(Diagnostic::error(
            "memory.size_mib",
            "memory.too-small",
            "at least 16 MiB of memory is required",
        ));
    }

    // Disks
    let mut boot = BTreeMap::new();
    for (i, disk) in spec.disks.iter().enumerate() {
        let f = |s: &str| format!("disks[{i}].{s}");
        if !valid_image_name(&disk.image.name) {
            d.push(Diagnostic::error(
                f("image"),
                "disk.image-name",
                "invalid image file name",
            ));
        }
        match disk_device(disk.bus, disk.media) {
            Err(msg) => d.push(Diagnostic::error(f("bus"), "disk.bus-media", msg)),
            Ok(dev) => {
                for needed in std::iter::once(dev).chain(controller_for(disk.bus)) {
                    if !has_device(caps, needed) {
                        d.push(Diagnostic::error(
                            f("bus"),
                            "disk.device-missing",
                            format!("this QEMU has no '{needed}' device"),
                        ));
                    }
                }
            }
        }
        if disk.cache == CacheMode::Unsafe {
            d.push(Diagnostic::warning(
                f("cache"),
                "disk.cache-unsafe",
                "cache=unsafe ignores flushes; a host crash can corrupt the disk",
            ));
        }
        if disk.aio.as_deref() == Some("native") && !cache_direct(disk.cache) {
            d.push(Diagnostic::error(
                f("aio"),
                "disk.aio-native-direct",
                "aio=native requires cache mode 'none' or 'directsync'",
            ));
        }
        if let Some(aio) = &disk.aio
            && !["threads", "native", "io_uring"].contains(&aio.as_str())
        {
            d.push(Diagnostic::error(
                f("aio"),
                "disk.aio-unknown",
                format!("unknown aio mode '{aio}'"),
            ));
        }
        if let Some(b) = disk.boot_index
            && let Some(prev) = boot.insert(b, i)
        {
            d.push(Diagnostic::error(
                f("boot_index"),
                "disk.boot-index-duplicate",
                format!("boot index {b} is also used by disk {prev}"),
            ));
        }
    }

    // NICs
    for (i, nic) in spec.nics.iter().enumerate() {
        let f = |s: &str| format!("nics[{i}].{s}");
        if !has_device(caps, &nic.model) {
            d.push(Diagnostic::error(
                f("model"),
                "nic.model-missing",
                format!("this QEMU has no '{}' device", nic.model),
            ));
        }
        match &nic.mac {
            Some(m) if !valid_mac(m) => d.push(Diagnostic::error(
                f("mac"),
                "nic.mac-invalid",
                "MAC must be a unicast xx:xx:xx:xx:xx:xx address",
            )),
            None => d.push(Diagnostic::error(
                f("mac"),
                "nic.mac-missing",
                "a MAC address must be assigned",
            )),
            _ => {}
        }
        let NicBackend::User { forwards } = &nic.backend;
        for (j, fw) in forwards.iter().enumerate() {
            let ff = format!("nics[{i}].backend.forwards[{j}]");
            if !matches!(fw.proto.as_str(), "tcp" | "udp") {
                d.push(Diagnostic::error(
                    &ff,
                    "forward.proto",
                    "protocol must be tcp or udp",
                ));
            }
            if fw.host_port == 0 || fw.guest_port == 0 {
                d.push(Diagnostic::error(
                    &ff,
                    "forward.port-zero",
                    "ports must be non-zero",
                ));
            }
            match fw.host_addr.parse::<std::net::IpAddr>() {
                Err(_) => d.push(Diagnostic::error(
                    &ff,
                    "forward.addr",
                    "host address must be an IP address",
                )),
                Ok(ip) if !ip.is_loopback() => d.push(Diagnostic::warning(
                    &ff,
                    "forward.exposed",
                    format!(
                        "port {} will be reachable from other machines on the network",
                        fw.host_port
                    ),
                )),
                Ok(_) => {}
            }
        }
    }

    // Display / input
    if let Some(dev) = &spec.display.device
        && !has_device(caps, dev)
    {
        d.push(Diagnostic::error(
            "display.device",
            "display.device-missing",
            format!("this QEMU has no '{dev}' device"),
        ));
    }
    if spec.display.vnc && spec.display.device.is_none() {
        d.push(Diagnostic::warning(
            "display.device",
            "display.vnc-without-device",
            "VNC is enabled but there is no graphics device; the console will stay blank",
        ));
    }
    if spec.display.vdagent {
        let has_chardev = caps
            .backends
            .chardev
            .as_ref()
            .is_some_and(|c| c.iter().any(|x| x == "qemu-vdagent"));
        if !has_chardev || !has_device(caps, "virtio-serial-pci") {
            d.push(Diagnostic::error(
                "display.vdagent",
                "display.vdagent-missing",
                "this QEMU lacks the qemu-vdagent chardev or virtio-serial-pci",
            ));
        }
    }
    if spec.input.usb_tablet && !(has_device(caps, "usb-tablet") && has_device(caps, "qemu-xhci")) {
        d.push(Diagnostic::error(
            "input.usb_tablet",
            "input.tablet-missing",
            "this QEMU lacks usb-tablet or qemu-xhci",
        ));
    }

    // Firmware
    if let FirmwareSpec::Descriptor { id } = &spec.firmware
        && id.is_empty()
    {
        d.push(Diagnostic::error(
            "firmware",
            "firmware.empty",
            "firmware descriptor id is empty",
        ));
    }
    if spec
        .machine
        .props
        .keys()
        .any(|k| k == "pflash0" || k == "pflash1")
    {
        d.push(Diagnostic::error(
            "machine.props",
            "machine.pflash-owned",
            "pflash is managed through the firmware setting",
        ));
    }

    d.extend(validate_passthrough(&spec.passthrough));
    d
}

fn validate_passthrough(tokens: &[String]) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        let field = format!("passthrough[{i}]");
        let opt = tok.split('=').next().unwrap_or(tok);
        let opt = if opt.starts_with("--") {
            &opt[1..]
        } else {
            opt
        };
        if OWNED_OPTIONS.contains(&opt) {
            d.push(Diagnostic::error(
                &field,
                "passthrough.owned-option",
                format!("{opt} is managed by the editor and cannot be passed through"),
            ));
        }
        let lower = tok.to_ascii_lowercase();
        let reserved = [
            "id=",
            "node-name=",
            "\"id\":\"",
            "\"node-name\":\"",
            "\"id\": \"",
            "\"node-name\": \"",
        ]
        .iter()
        .any(|k| lower.contains(&format!("{k}{RESERVED_PREFIX}")));
        if reserved {
            d.push(Diagnostic::error(
                &field,
                "passthrough.reserved-id",
                format!("ids starting with '{RESERVED_PREFIX}' are reserved for generated options"),
            ));
        }
    }
    d
}

fn cache_direct(c: CacheMode) -> bool {
    matches!(c, CacheMode::None | CacheMode::Directsync)
}

/// `(direct, no_flush, write_cache)` for a cache mode, per QEMU's mapping.
fn cache_flags(c: CacheMode) -> (bool, bool, bool) {
    match c {
        CacheMode::None => (true, false, true),
        CacheMode::Writeback => (false, false, true),
        CacheMode::Writethrough => (false, false, false),
        CacheMode::Directsync => (true, false, false),
        CacheMode::Unsafe => (false, true, true),
    }
}

fn controller_for(bus: DiskBus) -> Option<&'static str> {
    match bus {
        DiskBus::Scsi => Some("virtio-scsi-pci"),
        DiskBus::Usb => Some("qemu-xhci"),
        _ => None,
    }
}

/// Compile a spec. Errors are returned as diagnostics; warnings travel with
/// the result.
pub fn compile(spec: &VmSpec, ctx: &Context) -> Result<Compiled, Vec<Diagnostic>> {
    let caps = ctx.caps;
    let mut diags = validate(spec, caps);

    // Context-dependent checks.
    for (i, disk) in spec.disks.iter().enumerate() {
        if !ctx.images.contains_key(&disk.image) {
            diags.push(Diagnostic::error(
                format!("disks[{i}].image"),
                "disk.image-missing",
                format!(
                    "image '{}' was not found in pool '{}'",
                    disk.image.name, disk.image.pool
                ),
            ));
        }
    }
    if matches!(spec.firmware, FirmwareSpec::Descriptor { .. }) && ctx.firmware.is_none() {
        diags.push(Diagnostic::error(
            "firmware",
            "firmware.unresolved",
            "the selected firmware is not installed",
        ));
    }
    if let Some(fw) = &ctx.firmware
        && fw.requires_smm
        && spec.machine.props.get("smm").is_some_and(|v| v == "off")
    {
        diags.push(Diagnostic::error(
            "machine.props",
            "firmware.smm",
            "this firmware requires smm=on",
        ));
    }
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return Err(diags);
    }
    let accel = resolve_accel(spec, caps).map_err(|e| vec![e])?;

    let mut a: Vec<String> = Vec::new();
    let push = |args: &mut Vec<String>, opt: &str, val: String| {
        args.push(opt.to_string());
        args.push(val);
    };
    let json_arg = |v: Value| v.to_string();

    // Identity and defaults
    let mut name = format!("guest={}", kv(&spec.name));
    if ctx.os == "linux" {
        name.push_str(",debug-threads=on");
    }
    push(&mut a, "-name", name);
    push(&mut a, "-msg", "timestamp=on".into());
    a.push("-nodefaults".into());
    a.push("-no-user-config".into());
    push(&mut a, "-accel", accel.clone());

    // Machine (+ firmware flash)
    let mut machine = kv(&spec.machine.type_name);
    let mut props = spec.machine.props.clone();
    if let Some(fw) = &ctx.firmware {
        if fw.requires_smm {
            props.insert("smm".into(), "on".into());
        }
        props.insert("pflash0".into(), "qui-fw-code".into());
        if fw.vars.is_some() {
            props.insert("pflash1".into(), "qui-fw-vars".into());
        }
    }
    for (k, v) in &props {
        machine.push_str(&format!(",{}={}", kv(k), kv(v)));
    }
    push(&mut a, "-machine", machine);
    if let Some(fw) = &ctx.firmware
        && fw.secure_boot
        && (caps.arch == "x86_64" || caps.arch == "i386")
    {
        push(
            &mut a,
            "-global",
            "driver=cfi.pflash01,property=secure,value=on".into(),
        );
    }

    // CPU and memory
    if let Some(model) = &spec.cpu.model {
        let mut cpu = kv(model);
        for (k, v) in &spec.cpu.props {
            cpu.push_str(&format!(",{}={}", kv(k), kv(v)));
        }
        push(&mut a, "-cpu", cpu);
    }
    let mut smp = spec.cpu.count.to_string();
    if let Some(t) = spec.cpu.topology {
        smp.push_str(&format!(
            ",sockets={},cores={},threads={}",
            t.sockets, t.cores, t.threads
        ));
    }
    push(&mut a, "-smp", smp);
    push(&mut a, "-m", format!("{}M", spec.memory.size_mib));

    // Firmware block nodes
    if let Some(fw) = &ctx.firmware {
        push(
            &mut a,
            "-blockdev",
            json_arg(
                json!({"node-name": "qui-fw-code-file", "driver": "file", "filename": fw.code, "read-only": true}),
            ),
        );
        push(
            &mut a,
            "-blockdev",
            json_arg(
                json!({"node-name": "qui-fw-code", "driver": fw.code_format, "file": "qui-fw-code-file", "read-only": true}),
            ),
        );
        if let Some(vars) = &fw.vars {
            push(
                &mut a,
                "-blockdev",
                json_arg(
                    json!({"node-name": "qui-fw-vars-file", "driver": "file", "filename": vars}),
                ),
            );
            push(
                &mut a,
                "-blockdev",
                json_arg(
                    json!({"node-name": "qui-fw-vars", "driver": fw.vars_format, "file": "qui-fw-vars-file"}),
                ),
            );
        }
    }

    // Controllers needed by disks and input
    let buses: BTreeSet<DiskBus> = spec.disks.iter().map(|d| d.bus).collect();
    let need_usb = spec.input.usb_tablet || buses.contains(&DiskBus::Usb);
    if need_usb {
        push(
            &mut a,
            "-device",
            json_arg(json!({"driver": "qemu-xhci", "id": "qui-usb"})),
        );
    }
    if buses.contains(&DiskBus::Scsi) {
        push(
            &mut a,
            "-device",
            json_arg(json!({"driver": "virtio-scsi-pci", "id": "qui-scsi0"})),
        );
    }

    // Disks
    for (i, disk) in spec.disks.iter().enumerate() {
        let img = &ctx.images[&disk.image];
        a.extend(disk_args(i, disk, img));
    }

    // NICs
    for (i, nic) in spec.nics.iter().enumerate() {
        let NicBackend::User { forwards } = &nic.backend;
        let mut netdev = json!({"type": "user", "id": format!("qui-net{i}")});
        if !forwards.is_empty() {
            netdev["hostfwd"] = forwards
                .iter()
                .map(|f| json!({"str": format!("{}:{}:{}-:{}", f.proto, f.host_addr, f.host_port, f.guest_port)}))
                .collect();
        }
        push(&mut a, "-netdev", json_arg(netdev));
        let mut dev = json!({"driver": nic.model, "id": format!("qui-nic{i}"), "netdev": format!("qui-net{i}")});
        if let Some(mac) = &nic.mac {
            dev["mac"] = mac.as_str().into();
        }
        push(&mut a, "-device", json_arg(dev));
    }

    // Display and input
    if let Some(dev) = &spec.display.device {
        push(
            &mut a,
            "-device",
            json_arg(json!({"driver": dev, "id": "qui-gpu0"})),
        );
    }
    if spec.input.usb_tablet {
        push(
            &mut a,
            "-device",
            json_arg(json!({"driver": "usb-tablet", "id": "qui-tablet", "bus": "qui-usb.0"})),
        );
    }
    if spec.display.vdagent {
        push(
            &mut a,
            "-device",
            json_arg(json!({"driver": "virtio-serial-pci", "id": "qui-vser0"})),
        );
        push(
            &mut a,
            "-chardev",
            "qemu-vdagent,id=qui-vdagent,name=vdagent,clipboard=on,mouse=on".into(),
        );
        push(
            &mut a,
            "-device",
            json_arg(
                json!({"driver": "virtserialport", "chardev": "qui-vdagent", "name": "com.redhat.spice.0", "bus": "qui-vser0.0"}),
            ),
        );
    }
    // Never open a local window (Windows builds default to SDL/GTK).
    push(&mut a, "-display", "none".into());
    if spec.display.vnc {
        let vnc = match &ctx.run.vnc {
            VncListen::Unix(p) => format!("unix:{}", kv(p)),
            VncListen::Tcp { display } => format!("127.0.0.1:{display},password=on"),
        };
        push(&mut a, "-vnc", vnc);
    }

    // Monitors and serial
    let use_objects = caps.backends.has_object("monitor-qmp");
    for (id, path) in [
        ("qui-qmp", &ctx.run.qmp),
        ("qui-qmp-user", &ctx.run.qmp_user),
    ] {
        push(
            &mut a,
            "-chardev",
            format!("socket,id={id},path={},server=on,wait=off", kv(path)),
        );
        if use_objects {
            push(
                &mut a,
                "-object",
                format!("monitor-qmp,id={id}-mon,chardev={id}"),
            );
        } else {
            push(&mut a, "-mon", format!("chardev={id},mode=control"));
        }
    }
    push(
        &mut a,
        "-chardev",
        format!(
            "socket,id=qui-ser0,path={},server=on,wait=off,logfile={},logappend=on",
            kv(&ctx.run.serial),
            kv(&ctx.run.serial_log)
        ),
    );
    push(&mut a, "-serial", "chardev:qui-ser0".into());
    push(&mut a, "-pidfile", ctx.run.pidfile.clone());
    a.push("-S".into());

    a.extend(spec.passthrough.iter().cloned());

    Ok(Compiled {
        program: caps.binary.path.clone(),
        args: a,
        accel,
        warnings: diags,
    })
}

fn disk_args(i: usize, disk: &DiskSpec, img: &ResolvedImage) -> Vec<String> {
    let (direct, no_flush, write_cache) = cache_flags(disk.cache);
    let readonly = disk.readonly || disk.media == Media::Cdrom;
    // `threads` works everywhere; io_uring/native are opt-in until the probe
    // checks host support.
    let aio = disk.aio.clone().unwrap_or_else(|| "threads".into());
    let file_node = format!("qui-disk{i}-file");
    let fmt_node = format!("qui-disk{i}");
    let discard = if disk.discard && !readonly {
        "unmap"
    } else {
        "ignore"
    };

    let mut file = json!({
        "node-name": file_node,
        "driver": "file",
        "filename": img.path,
        "aio": aio,
        "cache": {"direct": direct, "no-flush": no_flush},
        "read-only": readonly,
    });
    if !readonly {
        file["discard"] = discard.into();
    }
    let mut format = json!({
        "node-name": fmt_node,
        "driver": img.format,
        "file": file_node,
        "read-only": readonly,
        "cache": {"direct": direct, "no-flush": no_flush},
    });
    if !readonly {
        format["discard"] = discard.into();
    }

    let driver = disk_device(disk.bus, disk.media).expect("validated");
    let mut dev = json!({"driver": driver, "id": format!("qui-dev{i}"), "drive": fmt_node});
    match disk.bus {
        DiskBus::Scsi => dev["bus"] = "qui-scsi0.0".into(),
        DiskBus::Usb => {
            dev["bus"] = "qui-usb.0".into();
            dev["removable"] = (disk.media == Media::Cdrom).into();
        }
        DiskBus::Nvme => dev["serial"] = format!("qui-disk{i}").into(),
        _ => {}
    }
    if matches!(driver, "virtio-blk-pci" | "scsi-hd" | "ide-hd") {
        dev["write-cache"] = if write_cache { "on" } else { "off" }.into();
    }
    if let Some(b) = disk.boot_index {
        dev["bootindex"] = b.into();
    }
    vec![
        "-blockdev".into(),
        file.to_string(),
        "-blockdev".into(),
        format.to_string(),
        "-device".into(),
        dev.to_string(),
    ]
}

/// Render argv for display/copying, quoting for a POSIX shell. Never executed.
pub fn display_command(program: &str, args: &[String]) -> String {
    fn quote(s: &str) -> String {
        if !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+%".contains(c))
        {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', r"'\''"))
        }
    }
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ")
}
