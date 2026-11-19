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

use crate::buses::{MachineBuses, SPARE_ROOT_PORTS};
use crate::caps::BinaryCaps;
use crate::network::{Network, NetworkConfig};
use crate::spec::{
    CacheMode, DiskBus, DiskSpec, FirmwareSpec, ImageRef, Media, NicBackend, PortForward, VmSpec,
    valid_image_name, valid_mac, valid_name, valid_share_path, valid_share_tag,
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
    pub(crate) fn error(field: impl Into<String>, rule: &str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            field: field.into(),
            rule: rule.into(),
            message: message.into(),
        }
    }
    pub(crate) fn warning(
        field: impl Into<String>,
        rule: &str,
        message: impl Into<String>,
    ) -> Self {
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
    /// Served over NBD on an AF_UNIX socket instead of opened as a file:
    /// physical host disks exported by the privileged helper (M2-11).
    pub nbd: Option<NbdSource>,
}

/// An NBD export on a local AF_UNIX socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NbdSource {
    pub socket: String,
    pub export: String,
}

/// How a shared folder reaches the guest; chosen by the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareBackend {
    /// virtio-9p: built into QEMU, works everywhere QEMU has fsdev support.
    NineP,
    /// virtiofs: faster, needs a `virtiofsd` sidecar listening on `socket`
    /// and the guest RAM in shared memory.
    VirtioFs { socket: String },
}

/// A share after the daemon resolved its root and folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShare {
    /// Canonical host folder, confined to its share root.
    pub path: String,
    pub backend: ShareBackend,
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
    /// One per `spec.shares`, in order.
    pub shares: Vec<ResolvedShare>,
    /// The machine's buses, from introspection (M2-09). When known, disk
    /// buses are validated against it and, on PCIe machines, PCI disks are
    /// placed behind root ports so they can be hot-(un)plugged.
    pub machine: Option<&'a MachineBuses>,
    /// Network objects NICs may reference (M3-01).
    pub networks: &'a [Network],
    /// Switch socket per isolated network id (M3-04), resolved by the daemon.
    pub switch_sockets: &'a BTreeMap<String, String>,
    /// Host taps for NICs on tap-backed networks, by NIC index (M3-05/06);
    /// the daemon resolves them (helper-created taps on Linux, the
    /// TAP-Windows adapter on Windows).
    pub taps: BTreeMap<usize, TapSource>,
}

/// A host tap interface a NIC is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapSource {
    pub ifname: String,
    /// `/dev/vhost-net` is usable: in-kernel virtio-net data path.
    pub vhost: bool,
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

/// The virtio-9p device this binary offers, if folder sharing via 9p works.
/// Absent on Windows builds ("fsdev support is disabled").
pub fn ninep_device(caps: &BinaryCaps) -> Option<&'static str> {
    ["virtio-9p-pci", "virtio-9p-device"]
        .into_iter()
        .find(|d| has_device(caps, d))
}

/// Can this binary attach a virtiofs share (given a `virtiofsd`)?
pub fn virtiofs_supported(caps: &BinaryCaps) -> bool {
    has_device(caps, "vhost-user-fs-pci") && caps.backends.has_object("memory-backend-memfd")
}

fn validate_shares(spec: &VmSpec, caps: &BinaryCaps) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    let supported = ninep_device(caps).is_some() || virtiofs_supported(caps);
    let mut tags = BTreeMap::new();
    for (i, s) in spec.shares.iter().enumerate() {
        let f = |x: &str| format!("shares[{i}].{x}");
        if !supported {
            d.push(Diagnostic::error(
                format!("shares[{i}]"),
                "share.unsupported",
                "this QEMU has no folder sharing (no virtio-9p device). QEMU on Windows hosts cannot \
                 share host folders at all (platform support: folder sharing); use an SMB share or \
                 SFTP inside the guest instead",
            ));
        }
        if !crate::storage::valid_pool_name(&s.root) {
            d.push(Diagnostic::error(
                f("root"),
                "share.root-invalid",
                "choose a registered share root",
            ));
        }
        if !valid_share_path(&s.path) {
            d.push(Diagnostic::error(
                f("path"),
                "share.path-invalid",
                "use a folder inside the root, like 'projects/web' (no '..', no absolute paths)",
            ));
        }
        if !valid_share_tag(&s.tag) {
            d.push(Diagnostic::error(
                f("tag"),
                "share.tag-invalid",
                "mount tags use 1-31 letters, digits, '-' or '_', starting with a letter or digit",
            ));
        } else if let Some(prev) = tags.insert(s.tag.clone(), i) {
            d.push(Diagnostic::error(
                f("tag"),
                "share.tag-duplicate",
                format!("tag '{}' is also used by share {prev}", s.tag),
            ));
        }
    }
    d
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
        if crate::spec::is_host_disk(&disk.image) {
            if !crate::spec::valid_host_disk_id(&disk.image.name) {
                d.push(Diagnostic::error(
                    f("image"),
                    "disk.host-disk-id",
                    "invalid host disk id",
                ));
            }
        } else if !valid_image_name(&disk.image.name) {
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

    let mut indices = BTreeMap::new();
    for (i, disk) in spec.disks.iter().enumerate() {
        if let Some(prev) = indices.insert(disk.node_index(i), i) {
            d.push(Diagnostic::error(
                format!("disks[{i}].slot"),
                "disk.slot-duplicate",
                format!("disk {i} uses the same QEMU id index as disk {prev}"),
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
        for (j, fw) in nic.backend.forwards().iter().enumerate() {
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
    d.extend(validate_forward_duplicates(spec));
    d.extend(crate::nic::validate_nics(&spec.nics));

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
    d.extend(validate_shares(spec, caps));
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

    d.extend(validate_usb(spec, caps));
    d.extend(validate_passthrough(&spec.passthrough));
    d.extend(crate::devices::validate_devices(spec, &caps.devices));
    d
}

/// USB passthrough rules (M2-12).
fn validate_usb(spec: &VmSpec, caps: &BinaryCaps) -> Vec<Diagnostic> {
    use crate::usb::{normalize_id, valid_port};
    let mut d = Vec::new();
    if spec.usb.is_empty() {
        return d;
    }
    if !(has_device(caps, "usb-host") && has_device(caps, "qemu-xhci")) {
        d.push(Diagnostic::error(
            "usb",
            "usb.host-missing",
            "this QEMU was built without usb-host (libusb), so host USB devices cannot be passed through",
        ));
    }
    let mut seen = BTreeSet::new();
    for (i, u) in spec.usb.iter().enumerate() {
        let f = |s: &str| format!("usb[{i}].{s}");
        if normalize_id(&u.vendor_id).is_none() {
            d.push(Diagnostic::error(
                f("vendor_id"),
                "usb.id-invalid",
                "vendor id must be 4 hex digits",
            ));
        }
        if normalize_id(&u.product_id).is_none() {
            d.push(Diagnostic::error(
                f("product_id"),
                "usb.id-invalid",
                "product id must be 4 hex digits",
            ));
        }
        if let Some(p) = &u.host_port
            && !valid_port(p)
        {
            d.push(Diagnostic::error(
                f("host_port"),
                "usb.port-invalid",
                "port must look like 1 or 1.2.3",
            ));
        }
        let key = (
            u.vendor_id.to_ascii_lowercase(),
            u.product_id.to_ascii_lowercase(),
            u.host_bus,
            u.host_port.clone(),
        );
        if !seen.insert(key) {
            d.push(Diagnostic::warning(
                f("vendor_id"),
                "usb.duplicate",
                "the same device is listed twice; only one VM-side device can hold it",
            ));
        }
    }
    d
}

/// The `-device` JSON for USB passthrough rule `i` (also used for hotplug).
pub fn usb_host_device(spec: &VmSpec, i: usize) -> Value {
    let u = &spec.usb[i];
    let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0);
    let mut dev = json!({
        "driver": "usb-host",
        "id": crate::usb::device_id(&spec.usb, i),
        "bus": "qui-usb.0",
        "vendorid": hex(&u.vendor_id),
        "productid": hex(&u.product_id),
    });
    if let Some(b) = u.host_bus {
        dev["hostbus"] = b.into();
    }
    if let Some(p) = &u.host_port {
        dev["hostport"] = p.as_str().into();
    }
    dev
}

/// Check disk buses against what the machine actually provides (M2-09).
pub fn validate_buses(spec: &VmSpec, caps: &BinaryCaps, machine: &MachineBuses) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, disk) in spec.disks.iter().enumerate() {
        let s = machine.support(caps, disk.bus);
        if !s.available {
            d.push(Diagnostic::error(
                format!("disks[{i}].bus"),
                "disk.bus-unavailable",
                s.reason
                    .unwrap_or_else(|| "this bus is not available on this machine".into()),
            ));
        }
    }
    d.extend(crate::nic::validate_nic_buses(
        &spec.nics,
        &caps.devices,
        machine,
    ));
    d
}

/// Whether a disk on `bus` is a PCI device of its own (and so goes behind a
/// PCIe root port on PCIe machines).
pub fn is_pci_disk(bus: DiskBus) -> bool {
    matches!(bus, DiskBus::Virtio | DiskBus::Nvme)
}

/// Root ports to create: one per PCI disk plus spares for hotplug, when the
/// machine's PCI root is PCIe and this QEMU has `pcie-root-port`.
pub fn root_port_count(spec: &VmSpec, caps: &BinaryCaps, machine: Option<&MachineBuses>) -> usize {
    let Some(m) = machine else { return 0 };
    if !m.pcie_root() || !caps.devices.iter().any(|x| x.name == "pcie-root-port") {
        return 0;
    }
    spec.disks.iter().filter(|d| is_pci_disk(d.bus)).count() + SPARE_ROOT_PORTS
}

/// The `-device` JSON for root port `k` on root bus `root`.
pub fn root_port_device(k: usize, root: &str) -> Value {
    json!({
        "driver": "pcie-root-port",
        "id": format!("qui-rp{k}"),
        "bus": root,
        "chassis": k + 1,
        "port": 16 + k,
    })
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
/// The cache mode actually used on `os`. QEMU on Windows hosts cannot open
/// images with direct I/O (`cache.direct=on` makes qcow2 headers unreadable),
/// so modes that need it fall back to their buffered equivalent.
pub fn effective_cache(c: CacheMode, os: &str) -> CacheMode {
    match (os, c) {
        ("windows", CacheMode::None) => CacheMode::Writeback,
        ("windows", CacheMode::Directsync) => CacheMode::Writethrough,
        _ => c,
    }
}

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
/// NICs attached to network objects: the network exists and takes what the
/// NIC asks of it (M3-01).
/// Where NIC `nic`'s passt sidecar listens: in the run directory, next to
/// the QMP socket `qmp` (short root, so the AF_UNIX path limit holds).
pub fn passt_socket(qmp: &str, nic: usize) -> String {
    let name = format!("passt{nic}.sock");
    match std::path::Path::new(qmp).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name).display().to_string(),
        _ => name,
    }
}

/// Network kinds this host cannot run (passt off Linux, M3-03).
fn validate_network_hosts(spec: &VmSpec, networks: &[Network], os: &str) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, nic) in spec.nics.iter().enumerate() {
        if let NicBackend::Network { network, .. } = &nic.backend
            && let Some(why) = networks
                .iter()
                .find(|n| &n.id == network)
                .and_then(|n| n.config.host_unsupported_reason(os))
        {
            d.push(Diagnostic::error(
                format!("nics[{i}].backend.network"),
                if why.starts_with("passt") {
                    "net.passt-unsupported"
                } else {
                    "net.host-unsupported"
                },
                why,
            ));
        }
    }
    d
}

pub fn validate_networks(spec: &VmSpec, networks: &[Network]) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, nic) in spec.nics.iter().enumerate() {
        let f = |s: &str| format!("nics[{i}].{s}");
        if let NicBackend::Network { network, forwards } = &nic.backend {
            match networks.iter().find(|n| &n.id == network) {
                None => d.push(Diagnostic::error(
                    f("backend.network"),
                    "nic.network-missing",
                    format!("network '{network}' does not exist (deleted?); pick another"),
                )),
                Some(n) if !forwards.is_empty() && !n.config.supports_forwards() => {
                    d.push(Diagnostic::error(
                        f("backend.forwards"),
                        "forward.unsupported",
                        n.config.forwards_unsupported_reason(),
                    ))
                }
                Some(_) => {}
            }
        }
    }
    d
}

/// `-netdev tap` for a resolved tap. QEMU must not run scripts (the helper
/// already attached the tap); on Windows only `ifname` is understood.
fn tap_netdev(i: usize, tap: &TapSource, queues: u16, os: &str) -> Value {
    let mut nd = json!({"type": "tap", "id": format!("qui-net{i}"), "ifname": tap.ifname});
    if os != "windows" {
        nd["script"] = "no".into();
        nd["downscript"] = "no".into();
        if tap.vhost {
            nd["vhost"] = true.into();
        }
        if queues > 1 {
            nd["queues"] = json!(queues);
        }
    }
    nd
}

/// NICs on tap-backed networks (M3-05/06): the kind runs on this host, the
/// tap was resolved, and multiqueue only where the backend has queues.
pub fn validate_tap_networks(spec: &VmSpec, ctx: &Context) -> Vec<Diagnostic> {
    let mut d = Vec::new();
    for (i, nic) in spec.nics.iter().enumerate() {
        let NicBackend::Network { network, .. } = &nic.backend else {
            continue;
        };
        let Some(n) = ctx.networks.iter().find(|n| &n.id == network) else {
            continue; // nic.network-missing
        };
        if nic.queues.is_some_and(|q| q > 1) && !n.config.supports_multiqueue() {
            d.push(Diagnostic::error(
                format!("nics[{i}].queues"),
                "nic.mq-backend",
                format!(
                    "{} networks have a single queue; multiqueue needs a bridged or host-only network on Linux",
                    n.config.kind_label()
                ),
            ));
        }
        if !n.config.is_tap() {
            continue;
        }
        // `validate_network_hosts` reports kinds this host cannot run.
        if n.config.host_unsupported_reason(ctx.os).is_none() && !ctx.taps.contains_key(&i) {
            d.push(Diagnostic::error(
                format!("nics[{i}].backend.network"),
                "nic.tap-unresolved",
                format!(
                    "no host tap was set up for {} network '{}'",
                    n.config.kind_label(),
                    n.name
                ),
            ));
        }
    }
    d
}

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
    if ctx.shares.len() != spec.shares.len() {
        diags.push(Diagnostic::error(
            "shares",
            "share.unresolved",
            "shared folders could not be resolved",
        ));
    }
    for (i, r) in ctx.shares.iter().enumerate() {
        if matches!(r.backend, ShareBackend::VirtioFs { .. }) && !virtiofs_supported(caps) {
            diags.push(Diagnostic::error(
                format!("shares[{i}]"),
                "share.virtiofs-unsupported",
                "this QEMU cannot attach virtiofs shares",
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
    for (i, disk) in spec.disks.iter().enumerate() {
        let eff = effective_cache(disk.cache, ctx.os);
        if eff != disk.cache {
            diags.push(Diagnostic::warning(
                format!("disks[{i}].cache"),
                "disk.no-direct-io",
                format!(
                    "QEMU on Windows hosts does not support direct I/O; using '{}' instead",
                    serde_json::to_value(eff)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default()
                ),
            ));
        }
        if disk.aio.as_deref() == Some("native") && !cache_direct(eff) {
            diags.push(Diagnostic::error(
                format!("disks[{i}].aio"),
                "disk.aio-native-direct",
                "aio=native needs direct I/O, which this host does not support",
            ));
        }
    }
    if let Some(m) = ctx.machine {
        diags.extend(validate_buses(spec, caps, m));
        diags.extend(crate::devices::validate_device_buses(
            spec,
            &caps.devices,
            m,
        ));
    }
    diags.extend(validate_networks(spec, ctx.networks));
    diags.extend(validate_network_hosts(spec, ctx.networks, ctx.os));
    for (i, nic) in spec.nics.iter().enumerate() {
        if let NicBackend::Network { network, .. } = &nic.backend {
            let needs = ctx
                .networks
                .iter()
                .any(|n| &n.id == network && n.config.needs_switch());
            if needs && !ctx.switch_sockets.contains_key(network) {
                diags.push(Diagnostic::error(
                    format!("nics[{i}].backend.network"),
                    "nic.switch-missing",
                    "the isolated network's switch is not running",
                ));
            }
        }
    }
    diags.extend(validate_tap_networks(spec, ctx));
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
    let need_usb = spec.input.usb_tablet || buses.contains(&DiskBus::Usb) || !spec.usb.is_empty();
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

    // PCIe root ports: one per PCI disk, plus spares for hotplug.
    let ports = root_port_count(spec, caps, ctx.machine);
    if ports > 0 {
        let root = ctx
            .machine
            .and_then(|m| m.pci_root())
            .map(|b| b.name.clone())
            .unwrap_or_default();
        for k in 0..ports {
            push(&mut a, "-device", json_arg(root_port_device(k, &root)));
        }
    }

    // Disks
    let mut next_port = 0;
    for (i, disk) in spec.disks.iter().enumerate() {
        let img = &ctx.images[&disk.image];
        let (file, format, mut dev) = disk_objects(disk.node_index(i), disk, img, ctx.os);
        if ports > 0 && is_pci_disk(disk.bus) {
            dev["bus"] = format!("qui-rp{next_port}").into();
            next_port += 1;
        }
        for (opt, v) in [("-blockdev", file), ("-blockdev", format), ("-device", dev)] {
            push(&mut a, opt, v.to_string());
        }
    }

    // NICs
    for (i, nic) in spec.nics.iter().enumerate() {
        let mut netdev = json!({"type": "user", "id": format!("qui-net{i}")});
        let mut passt = false;
        if let Some(tap) = ctx.taps.get(&i) {
            netdev = tap_netdev(i, tap, nic.queues.unwrap_or(1), ctx.os);
        }
        if let NicBackend::Network { network, .. } = &nic.backend {
            // Validated above; a missing network never reaches here.
            if let Some(n) = ctx.networks.iter().find(|n| &n.id == network) {
                match &n.config {
                    NetworkConfig::Nat { subnet } => {
                        if let Some(s) = subnet {
                            netdev["net"] = s.as_str().into();
                        }
                    }
                    // Built from `ctx.taps` above.
                    NetworkConfig::Bridged { .. }
                    | NetworkConfig::HostOnly {}
                    | NetworkConfig::Tap { .. } => {}
                    NetworkConfig::Isolated {} => {
                        // A stream client of the daemon's switch; it keeps
                        // reconnecting, so a daemon restart heals the link.
                        netdev = json!({
                            "type": "stream",
                            "id": format!("qui-net{i}"),
                            "server": false,
                            "addr": {"type": "unix", "path": ctx.switch_sockets.get(network).cloned().unwrap_or_default()},
                            "reconnect-ms": 1000,
                        });
                    }
                    // QEMU connects to the NIC's passt sidecar, which the
                    // daemon starts (with the forwards) before QEMU.
                    NetworkConfig::Passt { .. } => {
                        passt = true;
                        netdev = json!({
                            "type": "stream",
                            "id": format!("qui-net{i}"),
                            "server": false,
                            "addr": {"type": "unix", "path": passt_socket(&ctx.run.qmp, i)},
                        });
                    }
                }
            }
        }
        let forwards = nic.backend.forwards();
        if !forwards.is_empty() && !passt {
            netdev["hostfwd"] = forwards
                .iter()
                .map(|f| json!({"str": hostfwd_rule(f)}))
                .collect();
        }
        push(&mut a, "-netdev", json_arg(netdev));
        let mut dev = json!({"driver": nic.model, "id": format!("qui-nic{i}"), "netdev": format!("qui-net{i}")});
        if let Some(mac) = &nic.mac {
            dev["mac"] = mac.as_str().into();
        }
        crate::nic::device_props(nic, &mut dev);
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
    for i in 0..spec.usb.len() {
        push(&mut a, "-device", json_arg(usb_host_device(spec, i)));
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
    // Shared folders
    let mut shared_memory = false;
    for (i, (share, r)) in spec.shares.iter().zip(&ctx.shares).enumerate() {
        match &r.backend {
            ShareBackend::NineP => {
                let dev = ninep_device(caps).expect("validated");
                let mut fsdev = format!(
                    "local,id=qui-fs{i},path={},security_model=mapped-xattr",
                    kv(&r.path)
                );
                if share.readonly {
                    fsdev.push_str(",readonly=on");
                }
                push(&mut a, "-fsdev", fsdev);
                push(
                    &mut a,
                    "-device",
                    json_arg(json!({
                        "driver": dev,
                        "id": format!("qui-share{i}"),
                        "fsdev": format!("qui-fs{i}"),
                        "mount_tag": share.tag,
                    })),
                );
            }
            ShareBackend::VirtioFs { socket } => {
                shared_memory = true;
                push(
                    &mut a,
                    "-chardev",
                    format!("socket,id=qui-vfs{i},path={}", kv(socket)),
                );
                push(
                    &mut a,
                    "-device",
                    json_arg(json!({
                        "driver": "vhost-user-fs-pci",
                        "id": format!("qui-share{i}"),
                        "chardev": format!("qui-vfs{i}"),
                        "tag": share.tag,
                    })),
                );
            }
        }
    }
    if shared_memory {
        // vhost-user devices need the guest RAM in shared memory.
        push(
            &mut a,
            "-object",
            format!(
                "memory-backend-memfd,id=qui-mem,size={}M,share=on",
                spec.memory.size_mib
            ),
        );
        push(&mut a, "-numa", "node,memdev=qui-mem".into());
    }

    // Devices added from the palette, after everything they may sit on.
    for (i, dev) in spec.devices.iter().enumerate() {
        push(
            &mut a,
            "-device",
            json_arg(crate::devices::device_json(i, dev)),
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

/// The protocol node, format node and device for disk index `i` (its
/// `node_index`): the same JSON serves `-blockdev`/`-device` on the command
/// line and `blockdev-add`/`device_add` for hotplug. The caller sets the
/// device's `bus` when it must go behind a root port.
pub fn disk_objects(
    i: usize,
    disk: &DiskSpec,
    img: &ResolvedImage,
    os: &str,
) -> (Value, Value, Value) {
    let (direct, no_flush, write_cache) = cache_flags(effective_cache(disk.cache, os));
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

    let mut file = match &img.nbd {
        // A host disk exported by the helper: QEMU stays unprivileged.
        Some(n) => json!({
            "node-name": file_node,
            "driver": "nbd",
            "server": {"type": "unix", "path": n.socket},
            "export": n.export,
            "read-only": readonly,
        }),
        None => json!({
            "node-name": file_node,
            "driver": "file",
            "filename": img.path,
            "aio": aio,
            "cache": {"direct": direct, "no-flush": no_flush},
            "read-only": readonly,
        }),
    };
    if !readonly && img.nbd.is_none() {
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
    (file, format, dev)
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

// ---------------------------------------------------------- port forwards

/// A slirp `hostfwd` rule, as `-netdev user,hostfwd=` and HMP `hostfwd_add`
/// take it: `tcp:127.0.0.1:2222-:22`.
pub fn hostfwd_rule(f: &PortForward) -> String {
    format!(
        "{}:{}:{}-:{}",
        f.proto, f.host_addr, f.host_port, f.guest_port
    )
}

/// The host side of a forward, as HMP `hostfwd_remove` takes it.
pub fn hostfwd_host(f: &PortForward) -> String {
    format!("{}:{}:{}", f.proto, f.host_addr, f.host_port)
}

/// Do two forwards claim the same host socket? A wildcard address overlaps
/// every address.
pub fn forwards_overlap(a: &PortForward, b: &PortForward) -> bool {
    if a.proto != b.proto || a.host_port != b.host_port {
        return false;
    }
    match (
        a.host_addr.parse::<std::net::IpAddr>(),
        b.host_addr.parse::<std::net::IpAddr>(),
    ) {
        (Ok(x), Ok(y)) => x == y || x.is_unspecified() || y.is_unspecified(),
        _ => a.host_addr == b.host_addr,
    }
}

/// The same host port claimed twice within one VM (across all its NICs).
fn validate_forward_duplicates(spec: &VmSpec) -> Vec<Diagnostic> {
    let all: Vec<(String, &PortForward)> = spec
        .nics
        .iter()
        .enumerate()
        .flat_map(|(i, nic)| {
            nic.backend
                .forwards()
                .iter()
                .enumerate()
                .map(move |(j, f)| (format!("nics[{i}].backend.forwards[{j}]"), f))
        })
        .collect();
    let mut d = Vec::new();
    for (k, (field, f)) in all.iter().enumerate() {
        if let Some((other, _)) = all[..k].iter().find(|(_, g)| forwards_overlap(f, g)) {
            d.push(Diagnostic::error(
                field,
                "forward.duplicate",
                format!(
                    "host {} port {} is already forwarded by {other}",
                    f.proto, f.host_port
                ),
            ));
        }
    }
    d
}

#[cfg(test)]
mod forward_tests {
    use super::*;

    fn fw(proto: &str, addr: &str, port: u16) -> PortForward {
        PortForward {
            proto: proto.into(),
            host_addr: addr.into(),
            host_port: port,
            guest_port: 22,
        }
    }

    #[test]
    fn rules_match_slirp_syntax() {
        let f = fw("tcp", "127.0.0.1", 2222);
        assert_eq!(hostfwd_rule(&f), "tcp:127.0.0.1:2222-:22");
        assert_eq!(hostfwd_host(&f), "tcp:127.0.0.1:2222");
    }

    #[test]
    fn overlap_rules() {
        let a = fw("tcp", "127.0.0.1", 2222);
        assert!(forwards_overlap(&a, &fw("tcp", "127.0.0.1", 2222)));
        assert!(
            forwards_overlap(&a, &fw("tcp", "0.0.0.0", 2222)),
            "wildcard"
        );
        assert!(
            !forwards_overlap(&a, &fw("udp", "127.0.0.1", 2222)),
            "protocol"
        );
        assert!(
            !forwards_overlap(&a, &fw("tcp", "127.0.0.2", 2222)),
            "address"
        );
        assert!(!forwards_overlap(&a, &fw("tcp", "127.0.0.1", 2223)), "port");
    }

    #[test]
    fn duplicates_across_nics_are_errors() {
        // JSON keeps the test independent of fields other work adds.
        let nic = |fs: serde_json::Value| {
            serde_json::json!({"backend": {"kind": "user", "forwards": fs},
                "model": "e1000", "mac": "52:54:00:00:00:01"})
        };
        let f = |proto: &str, addr: &str| serde_json::json!({"proto": proto, "host_addr": addr, "host_port": 2222, "guest_port": 22});
        let spec: VmSpec = serde_json::from_value(serde_json::json!({
            "spec_version": crate::spec::SPEC_VERSION, "name": "t", "arch": "x86_64",
            "machine": {"type": "pc"}, "cpu": {"count": 1}, "memory": {"size_mib": 128},
            "nics": [
                nic(serde_json::json!([f("tcp", "127.0.0.1"), f("udp", "127.0.0.1")])),
                nic(serde_json::json!([f("tcp", "0.0.0.0")])),
            ],
        }))
        .unwrap();
        let d = validate_forward_duplicates(&spec);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].rule, "forward.duplicate");
        assert_eq!(d[0].field, "nics[1].backend.forwards[0]");
        assert!(d[0].message.contains("nics[0].backend.forwards[0]"));
    }
}
