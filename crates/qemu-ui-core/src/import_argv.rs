//! Import a pasted QEMU command line into a VM spec (M4-07, plan.md §5.4).
//!
//! Best-effort and pure. Recognised options become spec fields; options
//! qemu-ui manages itself (monitors, display servers, pidfiles, …) are
//! dropped with a warning; everything else is kept verbatim in
//! `passthrough`. Host paths never enter the spec: images come back in
//! `files_to_map` with placeholder references (see [`crate::import`]).

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::import::{
    FileToMap, FileUse, FirmwareHint, ImportResult, PLACEHOLDER_POOL, is_block_device_path,
    looks_like_secure_boot, looks_like_uefi, suggest_image_name, suggest_vm_name,
};
use crate::spec::{
    CacheMode, CpuSpec, DiskBus, DiskSpec, DisplaySpec, FirmwareSpec, ImageRef, InputSpec,
    MachineSpec, Media, MemorySpec, NicBackend, NicSpec, PortForward, SPEC_VERSION, Topology,
    UsbPassthrough, VmSpec, valid_mac,
};

// ------------------------------------------------------------- tokenizing

/// Split a command line into tokens. Understands single and double quotes,
/// line continuations (`\`, `^` and `` ` `` before a newline, as in POSIX
/// shells, cmd.exe and PowerShell), and a backslash escaping a space or a
/// quote. Other backslashes are literal, so Windows paths survive.
pub fn tokenize(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && matches!(chars.peek(), Some('"') | Some('\\')) {
                cur.push(chars.next().unwrap_or_default());
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_token = true;
            }
            '\\' | '^' | '`' if continuation(&mut chars) => {}
            '\\' if matches!(chars.peek(), Some(' ') | Some('"') | Some('\'')) => {
                cur.push(chars.next().unwrap_or_default());
                in_token = true;
            }
            c if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            c => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return Err("the command line has an unterminated quote".into());
    }
    if in_token {
        out.push(cur);
    }
    Ok(out)
}

/// Consume a newline (optionally after spaces or `\r`) following a
/// continuation character. Returns false, consuming nothing, otherwise.
fn continuation(chars: &mut std::iter::Peekable<std::str::Chars>) -> bool {
    let mut look = chars.clone();
    while matches!(look.peek(), Some(' ') | Some('\t') | Some('\r')) {
        look.next();
    }
    if look.peek() == Some(&'\n') {
        look.next();
        *chars = look;
        true
    } else {
        false
    }
}

/// Parse a QemuOpts/keyval value (`a=b,c=d`, `,,` escapes a comma). A first
/// item without `=` gets the `implied` key; other bare items (`+feature`,
/// flags) keep an empty value. JSON objects are flattened to dotted keys.
pub fn parse_opts(s: &str, implied: Option<&str>) -> Vec<(String, String)> {
    if s.trim_start().starts_with('{')
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(s)
    {
        let mut out = Vec::new();
        flatten("", &Value::Object(map), &mut out);
        return out;
    }
    let mut items = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == ',' {
            if it.peek() == Some(&',') {
                it.next();
                cur.push(',');
            } else {
                items.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    items.push(cur);
    items
        .into_iter()
        .enumerate()
        .filter(|(_, x)| !x.is_empty())
        .map(|(i, x)| match x.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None if i == 0 && implied.is_some() => (implied.unwrap_or_default().to_string(), x),
            None => (x, String::new()),
        })
        .collect()
}

fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                flatten(&format!("{prefix}.{i}"), v, out);
            }
        }
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Bool(b) => out.push((prefix.to_string(), if *b { "on" } else { "off" }.into())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        Value::Null => {}
    }
}

/// Options without a value.
const FLAGS: &[&str] = &[
    "-enable-kvm",
    "-nodefaults",
    "-no-user-config",
    "-S",
    "-no-reboot",
    "-no-shutdown",
    "-daemonize",
    "-snapshot",
    "-nographic",
    "-usb",
    "-full-screen",
    "-no-hpet",
    "-no-acpi",
    "-localtime",
    "-enable-fips",
    "-only-migratable",
    "-preconfig",
    "-s",
    "-version",
    "-help",
    "-h",
    "-no-fd-bootchk",
    "-win2k-hack",
    "-alt-grab",
    "-ctrl-grab",
    "-no-quit",
];

/// Options qemu-ui sets itself; imported values are dropped (with a note).
const MANAGED: &[&str] = &[
    "-qmp",
    "-monitor",
    "-mon",
    "-chardev",
    "-serial",
    "-parallel",
    "-pidfile",
    "-daemonize",
    "-display",
    "-vnc",
    "-spice",
    "-S",
    "-nodefaults",
    "-no-user-config",
    "-msg",
    "-D",
    "-d",
    "-sandbox",
    "-runas",
    "-chroot",
    "-run-with",
    "-L",
    "-k",
    "-full-screen",
    "-name",
    "-trace",
    "-version",
    "-help",
    "-h",
    "-s",
    "-gdb",
    "-preconfig",
    "-usb",
    "-alt-grab",
    "-ctrl-grab",
    "-no-quit",
];

// ---------------------------------------------------------------- importer

#[derive(Default, Clone)]
struct DriveInfo {
    file: Option<String>,
    format: Option<String>,
    media: Media,
    readonly: bool,
    cache: Option<CacheMode>,
    aio: Option<String>,
    discard: Option<bool>,
}

#[derive(Default, Clone)]
struct Node {
    driver: String,
    filename: Option<String>,
    child: Option<String>,
    readonly: bool,
    cache: Option<CacheMode>,
    aio: Option<String>,
    discard: Option<bool>,
}

struct Netdev {
    kind: String,
    forwards: Vec<PortForward>,
}

struct PendingNic {
    model: String,
    netdev: Option<String>,
    mac: Option<String>,
}

const FORMATS: &[&str] = &[
    "qcow2",
    "raw",
    "vmdk",
    "vdi",
    "vhdx",
    "vpc",
    "qed",
    "luks",
    "qcow",
    "parallels",
    "dmg",
];

const CONTROLLERS: &[&str] = &[
    "qemu-xhci",
    "nec-usb-xhci",
    "piix3-usb-uhci",
    "piix4-usb-uhci",
    "ich9-usb-ehci1",
    "ich9-usb-uhci1",
    "ich9-usb-uhci2",
    "ich9-usb-uhci3",
    "usb-ehci",
    "pci-ohci",
    "virtio-scsi-pci",
    "virtio-scsi-device",
    "virtio-scsi-ccw",
    "ahci",
    "ich9-ahci",
    "pcie-root-port",
    "ioh3420",
    "pci-bridge",
    "pcie-pci-bridge",
    "lsi53c895a",
    "megasas",
    "pvscsi",
];

const NIC_MODELS: &[&str] = &[
    "virtio-net-pci",
    "virtio-net-device",
    "virtio-net-ccw",
    "virtio-net-pci-non-transitional",
    "e1000",
    "e1000e",
    "e1000-82545em",
    "igb",
    "rtl8139",
    "vmxnet3",
    "ne2k_pci",
    "i82559er",
    "i82557b",
    "pcnet",
    "usb-net",
    "spapr-vlan",
];

const DISPLAYS: &[&str] = &[
    "VGA",
    "virtio-vga",
    "virtio-vga-gl",
    "virtio-gpu-pci",
    "virtio-gpu-gl-pci",
    "virtio-gpu-device",
    "virtio-gpu",
    "qxl-vga",
    "qxl",
    "cirrus-vga",
    "bochs-display",
    "ramfb",
    "vmware-svga",
    "secondary-vga",
    "ati-vga",
];

struct Importer {
    arch: String,
    devices: Vec<crate::devices::DeviceSpec>,
    spec_name: Option<String>,
    machine: Option<MachineSpec>,
    accel: Option<String>,
    cpu: CpuSpec,
    smp: Option<Vec<(String, String)>>,
    memory_mib: Option<u64>,
    disks: Vec<DiskSpec>,
    /// Bus decided later from the architecture (plain `-drive`, `-cdrom`).
    default_bus: Vec<bool>,
    files: Vec<FileToMap>,
    drives: HashMap<String, DriveInfo>,
    nodes: HashMap<String, Node>,
    netdevs: HashMap<String, Netdev>,
    pending_nics: Vec<PendingNic>,
    legacy_nics: Vec<PendingNic>,
    legacy_forwards: Vec<PortForward>,
    nics: Vec<NicSpec>,
    display: Option<Option<String>>,
    nographic: bool,
    nodefaults: bool,
    usb_tablet: bool,
    usb: Vec<UsbPassthrough>,
    boot_order: Option<String>,
    explicit_boot: bool,
    firmware: Option<FirmwareHint>,
    passthrough: Vec<String>,
    warnings: Vec<String>,
    managed: Vec<String>,
    generated: Vec<String>,
}

/// Import `text`, a QEMU command line.
pub fn import_argv(text: &str) -> Result<ImportResult, String> {
    let tokens = tokenize(text)?;
    let mut i = 0;
    let mut arch = None;
    if let Some(first) = tokens.first()
        && !first.starts_with('-')
    {
        arch = arch_from_program(first);
        i = 1;
    }
    if !tokens[i..].iter().any(|t| t.starts_with('-')) {
        return Err("this does not look like a QEMU command line (no options found)".into());
    }
    let mut imp = Importer::new(arch.clone().unwrap_or_else(|| "x86_64".into()));
    if arch.is_none() {
        imp.warn("no qemu-system-<arch> program at the start; assumed x86_64".into());
    }
    while i < tokens.len() {
        let raw = &tokens[i];
        if !raw.starts_with('-') {
            imp.warn(format!("ignored stray argument '{raw}'"));
            i += 1;
            continue;
        }
        let opt = normalize_opt(raw);
        let takes_value = !FLAGS.contains(&opt.as_str())
            && tokens
                .get(i + 1)
                .is_some_and(|n| !n.starts_with('-') || n.len() == 1 || known_valued(&opt));
        let val = if takes_value {
            i += 1;
            Some(tokens[i].clone())
        } else {
            None
        };
        imp.option(&opt, raw, val.as_deref());
        i += 1;
    }
    Ok(imp.finish())
}

fn known_valued(opt: &str) -> bool {
    matches!(opt, "-cpu" | "-append" | "-global" | "-device" | "-object")
}

fn normalize_opt(raw: &str) -> String {
    let s = raw
        .strip_prefix("--")
        .map_or(raw.to_string(), |r| format!("-{r}"));
    match s.as_str() {
        "-M" => "-machine".into(),
        _ => s,
    }
}

/// `qemu-system-aarch64.exe`, `/usr/bin/qemu-system-x86_64`, `qemu-kvm`.
fn arch_from_program(p: &str) -> Option<String> {
    let base = p.rsplit(['/', '\\']).next().unwrap_or(p);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if let Some(a) = base.strip_prefix("qemu-system-") {
        let a = a
            .strip_suffix('w')
            .filter(|_| a.ends_with("64w"))
            .unwrap_or(a);
        return Some(a.to_string());
    }
    (base == "qemu-kvm").then(|| "x86_64".into())
}

fn on(v: &str) -> bool {
    matches!(v, "on" | "yes" | "true" | "1" | "")
}

fn cache_mode(v: &str) -> Option<CacheMode> {
    Some(match v {
        "none" => CacheMode::None,
        "writeback" => CacheMode::Writeback,
        "writethrough" => CacheMode::Writethrough,
        "directsync" => CacheMode::Directsync,
        "unsafe" => CacheMode::Unsafe,
        _ => return None,
    })
}

/// `-blockdev` cache options to a cache mode.
fn blockdev_cache(direct: Option<bool>, no_flush: Option<bool>) -> Option<CacheMode> {
    match (direct, no_flush) {
        (None, None) => None,
        (_, Some(true)) => Some(CacheMode::Unsafe),
        (Some(true), _) => Some(CacheMode::None),
        _ => Some(CacheMode::Writeback),
    }
}

fn discard(v: &str) -> Option<bool> {
    match v {
        "unmap" | "on" => Some(true),
        "ignore" | "off" => Some(false),
        _ => None,
    }
}

/// `4G`, `4096` (MiB), `512M`, `1T`, `2048k`.
pub fn parse_size_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches(['b', 'i'])
    {
        "" | "m" => 1.0,
        "k" => 1.0 / 1024.0,
        "g" => 1024.0,
        "t" => 1024.0 * 1024.0,
        _ => return None,
    };
    let v = (n * mult).round();
    (v >= 1.0).then_some(v as u64)
}

/// `tcp:127.0.0.1:2222-:22`, `tcp::2222-10.0.2.15:22`, `::2222-:22`.
fn parse_hostfwd(s: &str) -> Option<(PortForward, bool)> {
    let (host, guest) = s.split_once('-')?;
    let mut parts: Vec<&str> = host.split(':').collect();
    let proto = if parts.len() == 3 {
        parts.remove(0)
    } else {
        "tcp"
    };
    let proto = if proto.is_empty() { "tcp" } else { proto };
    let (haddr, hport) = match parts.as_slice() {
        [a, p] => (*a, *p),
        [p] => ("", *p),
        _ => return None,
    };
    let gport = guest.rsplit(':').next()?;
    // An empty host address means all interfaces; it becomes loopback here
    // and the caller says so.
    Some((
        PortForward {
            proto: proto.to_ascii_lowercase(),
            host_addr: if haddr.is_empty() {
                "127.0.0.1".into()
            } else {
                haddr.into()
            },
            host_port: hport.parse().ok()?,
            guest_port: gport.parse().ok()?,
        },
        haddr.is_empty(),
    ))
}

impl Importer {
    fn new(arch: String) -> Self {
        Importer {
            arch,
            spec_name: None,
            machine: None,
            accel: None,
            cpu: CpuSpec::default(),
            smp: None,
            memory_mib: None,
            disks: vec![],
            default_bus: vec![],
            files: vec![],
            drives: HashMap::new(),
            nodes: HashMap::new(),
            netdevs: HashMap::new(),
            pending_nics: vec![],
            legacy_nics: vec![],
            legacy_forwards: vec![],
            nics: vec![],
            display: None,
            nographic: false,
            nodefaults: false,
            usb_tablet: false,
            usb: vec![],
            boot_order: None,
            explicit_boot: false,
            firmware: None,
            devices: vec![],
            passthrough: vec![],
            warnings: vec![],
            managed: vec![],
            generated: vec![],
        }
    }

    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    fn keep(&mut self, raw: &str, val: Option<&str>) {
        self.passthrough.push(raw.to_string());
        if let Some(v) = val {
            self.passthrough.push(v.to_string());
        }
    }

    fn option(&mut self, opt: &str, raw: &str, val: Option<&str>) {
        let v = val.unwrap_or_default();
        match opt {
            "-name" => {
                let kv = parse_opts(v, Some("guest"));
                if let Some((_, n)) = kv.iter().find(|(k, _)| k == "guest") {
                    self.spec_name = Some(n.clone());
                }
            }
            "-machine" => self.machine_opt(v),
            "-accel" => {
                let kv = parse_opts(v, Some("accel"));
                if let Some((_, a)) = kv.iter().find(|(k, _)| k == "accel") {
                    self.accel.get_or_insert_with(|| a.clone());
                }
            }
            "-enable-kvm" => self.accel = Some("kvm".into()),
            "-cpu" => self.cpu_opt(v),
            "-smp" => self.smp = Some(parse_opts(v, Some("cpus"))),
            "-m" => {
                let kv = parse_opts(v, Some("size"));
                for (k, x) in &kv {
                    match k.as_str() {
                        "size" => self.memory_mib = parse_size_mib(x),
                        "maxmem" | "slots" => self.warn(
                            "memory hotplug (-m slots/maxmem) is not modelled; the VM gets fixed memory".into(),
                        ),
                        _ => {}
                    }
                }
            }
            "-drive" => self.drive_opt(v),
            "-blockdev" => self.blockdev_opt(v),
            "-cdrom" => {
                let i = self.add_disk(v, None, None, Media::Cdrom, true, None, None, None);
                self.default_bus[i] = true;
            }
            "-hda" | "-hdb" | "-hdc" | "-hdd" => {
                self.add_disk(v, None, Some(DiskBus::Ide), Media::Disk, false, None, None, None);
            }
            "-fda" | "-fdb" => self.warn(format!("floppy {opt} {v} is not supported and was not imported")),
            "-boot" => {
                let kv = parse_opts(v, Some("order"));
                for (k, x) in kv {
                    match k.as_str() {
                        "order" | "once" => self.boot_order = Some(x),
                        "menu" | "splash" | "splash-time" | "reboot-timeout" | "strict" => {}
                        _ => {}
                    }
                }
            }
            "-netdev" => self.netdev_opt(v),
            "-nic" => self.nic_opt(v),
            "-net" => self.net_opt(v),
            "-device" => self.device_opt(raw, v),
            "-vga" => {
                self.display = Some(match v {
                    "std" => Some("VGA".into()),
                    "virtio" => Some("virtio-vga".into()),
                    "qxl" => Some("qxl-vga".into()),
                    "cirrus" => Some("cirrus-vga".into()),
                    "vmware" => Some("vmware-svga".into()),
                    "none" => None,
                    other => {
                        self.warn(format!("unknown -vga '{other}'; used standard VGA"));
                        Some("VGA".into())
                    }
                })
            }
            "-nographic" => self.nographic = true,
            "-usbdevice" => {
                if v == "tablet" {
                    self.usb_tablet = true;
                } else {
                    self.keep(raw, val);
                }
            }
            "-bios" => {
                if looks_like_uefi(v) {
                    self.uefi(v);
                } else {
                    self.warn(format!(
                        "-bios {v} kept in passthrough; it refers to a host file outside the pools"
                    ));
                    self.keep(raw, val);
                }
            }
            "-kernel" | "-initrd" | "-dtb" => {
                self.warn(format!(
                    "direct kernel boot ({opt}) kept in passthrough; it refers to host files outside the pools"
                ));
                self.keep(raw, val);
            }
            "-snapshot" => self.warn(
                "-snapshot (discard all disk writes) is not modelled; use a snapshot or a linked clone instead"
                    .into(),
            ),
            "-virtfs" | "-fsdev" => self.warn(format!(
                "folder share ({opt}) not imported: add it under Shared folders after import (needs a share root)"
            )),
            "-nodefaults" => {
                self.nodefaults = true;
                self.managed.push(opt.into());
            }
            o if MANAGED.contains(&o) => self.managed.push(o.into()),
            _ => self.keep(raw, val),
        }
    }

    fn machine_opt(&mut self, v: &str) {
        let mut m = self.machine.take().unwrap_or_default();
        for (k, x) in parse_opts(v, Some("type")) {
            match k.as_str() {
                "type" => m.type_name = x,
                "accel" => {
                    self.accel = x.split(':').next().map(String::from);
                }
                "usb" | "graphics" | "dump-guest-core" | "mem-merge" => {}
                k if k.starts_with("pflash") => {
                    if self.nodes.contains_key(&x) {
                        let f = self.node_file(&x).0;
                        if let Some(f) = f
                            && k == "pflash0"
                        {
                            self.uefi(&f);
                        }
                    } else if k == "pflash0" {
                        self.firmware.get_or_insert(FirmwareHint::Uefi);
                    }
                    if k == "pflash1" {
                        self.nvram_note();
                    }
                }
                _ => {
                    m.props.insert(k, x);
                }
            }
        }
        self.machine = Some(m);
    }

    fn cpu_opt(&mut self, v: &str) {
        for (i, (k, x)) in parse_opts(v, Some("model")).into_iter().enumerate() {
            if i == 0 && k == "model" {
                self.cpu.model = Some(x);
            } else if let Some(f) = k.strip_prefix('+').filter(|_| x.is_empty()) {
                self.cpu.props.insert(f.into(), "on".into());
            } else if let Some(f) = k.strip_prefix('-').filter(|_| x.is_empty()) {
                self.cpu.props.insert(f.into(), "off".into());
            } else if x.is_empty() {
                self.cpu.props.insert(k, "on".into());
            } else {
                self.cpu.props.insert(k, x);
            }
        }
    }

    fn uefi(&mut self, file: &str) {
        let hint = if looks_like_secure_boot(file) {
            FirmwareHint::UefiSecureBoot
        } else {
            FirmwareHint::Uefi
        };
        if self.firmware != Some(FirmwareHint::UefiSecureBoot) {
            self.firmware = Some(hint);
        }
    }

    fn nvram_note(&mut self) {
        self.warn(
            "UEFI variables (NVRAM) were not imported; the VM starts with a fresh variable store, \
             so boot entries may need recreating"
                .into(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_disk(
        &mut self,
        file: &str,
        format: Option<String>,
        bus: Option<DiskBus>,
        media: Media,
        readonly: bool,
        cache: Option<CacheMode>,
        aio: Option<String>,
        discard: Option<bool>,
    ) -> usize {
        let index = self.disks.len();
        let usage = if is_block_device_path(file) {
            self.warn(format!(
                "disk {index} is a host device ({file}); attach it as a host disk through the qemu-ui helper"
            ));
            FileUse::HostDevice
        } else {
            FileUse::Image
        };
        let mut name = suggest_image_name(file);
        let taken: Vec<&str> = self
            .files
            .iter()
            .map(|f| f.suggested_name.as_str())
            .collect();
        if taken.contains(&name.as_str()) {
            let mut n = 2;
            while taken.contains(&format!("{n}-{name}").as_str()) {
                n += 1;
            }
            name = format!("{n}-{name}");
        }
        self.files.push(FileToMap {
            path: file.to_string(),
            usage,
            disk: index,
            suggested_name: name.clone(),
            format: format.clone(),
            mapped: None,
        });
        self.disks.push(DiskSpec {
            image: ImageRef {
                pool: PLACEHOLDER_POOL.into(),
                name,
            },
            bus: bus.unwrap_or(DiskBus::Virtio),
            media,
            readonly: readonly || media == Media::Cdrom,
            boot_index: None,
            cache: cache.unwrap_or_default(),
            aio,
            discard: discard.unwrap_or(true),
            slot: None,
        });
        self.default_bus.push(bus.is_none());
        index
    }

    fn drive_opt(&mut self, v: &str) {
        let kv: BTreeMap<String, String> = parse_opts(v, None).into_iter().collect();
        let iface = kv.get("if").map(String::as_str).unwrap_or("");
        let info = DriveInfo {
            file: kv.get("file").cloned(),
            format: kv.get("format").cloned(),
            media: if kv.get("media").map(String::as_str) == Some("cdrom") {
                Media::Cdrom
            } else {
                Media::Disk
            },
            readonly: kv
                .get("readonly")
                .or(kv.get("read-only"))
                .is_some_and(|x| on(x)),
            cache: kv.get("cache").and_then(|c| cache_mode(c)),
            aio: kv.get("aio").cloned(),
            discard: kv.get("discard").and_then(|d| discard(d)),
        };
        if kv.get("snapshot").is_some_and(|x| on(x)) {
            self.warn(
                "drive snapshot=on (discard writes) is not modelled; writes will be kept".into(),
            );
        }
        match iface {
            "pflash" => {
                if let Some(f) = &info.file {
                    if info.readonly
                        || self.firmware.is_none()
                            && looks_like_uefi(f)
                            && !f.to_ascii_lowercase().contains("vars")
                    {
                        self.uefi(&f.clone());
                    } else {
                        self.nvram_note();
                    }
                }
            }
            "none" => {
                if let Some(id) = kv.get("id") {
                    self.drives.insert(id.clone(), info);
                }
            }
            "floppy" | "sd" | "mtd" => self.warn(format!(
                "{iface} drive {} not imported (unsupported)",
                info.file.unwrap_or_default()
            )),
            _ => {
                let Some(file) = info.file.clone() else {
                    if info.media == Media::Cdrom {
                        self.warn(
                            "an empty CD drive was not imported; add one in the editor".into(),
                        );
                    }
                    return;
                };
                let bus = match iface {
                    "virtio" => Some(DiskBus::Virtio),
                    "ide" => Some(DiskBus::Ide),
                    "scsi" => Some(DiskBus::Scsi),
                    "" => None,
                    other => {
                        self.warn(format!(
                            "drive interface '{other}' not supported; used the default"
                        ));
                        None
                    }
                };
                let i = self.add_disk(
                    &file,
                    info.format,
                    bus,
                    info.media,
                    info.readonly,
                    info.cache,
                    info.aio,
                    info.discard,
                );
                if let Some(b) = kv.get("bootindex").and_then(|b| b.parse().ok()) {
                    self.disks[i].boot_index = Some(b);
                    self.explicit_boot = true;
                }
            }
        }
    }

    fn blockdev_opt(&mut self, v: &str) {
        let kv: BTreeMap<String, String> = parse_opts(v, None).into_iter().collect();
        let Some(name) = kv.get("node-name").cloned() else {
            self.warn("a -blockdev without node-name was not imported".into());
            return;
        };
        self.add_node(&name, "", &kv);
    }

    /// Register a node and its inline children (`file.driver=…` or JSON).
    fn add_node(&mut self, name: &str, prefix: &str, kv: &BTreeMap<String, String>) {
        let get = |k: &str| kv.get(&format!("{prefix}{k}")).cloned();
        let driver = get("driver").unwrap_or_default();
        let child_prefix = format!("{prefix}file.");
        let child = if kv.keys().any(|k| k.starts_with(&child_prefix)) {
            let child_name = format!("{name}#file");
            self.add_node(&child_name, &child_prefix, kv);
            Some(child_name)
        } else {
            get("file")
        };
        self.nodes.insert(
            name.to_string(),
            Node {
                driver,
                filename: get("filename"),
                child,
                readonly: get("read-only").is_some_and(|x| on(&x)),
                cache: blockdev_cache(
                    get("cache.direct").map(|x| on(&x)),
                    get("cache.no-flush").map(|x| on(&x)),
                ),
                aio: get("aio"),
                discard: get("discard").and_then(|d| discard(&d)),
            },
        );
    }

    /// Follow a node chain to its file and format.
    fn node_file(&self, name: &str) -> (Option<String>, Option<String>, Node) {
        let Some(n) = self.nodes.get(name).cloned() else {
            return (None, None, Node::default());
        };
        if FORMATS.contains(&n.driver.as_str()) {
            let child = n.child.as_deref().and_then(|c| self.nodes.get(c));
            let file = child
                .and_then(|c| c.filename.clone())
                .or_else(|| n.child.clone().filter(|c| !self.nodes.contains_key(c)));
            let mut merged = n.clone();
            if let Some(c) = child {
                merged.readonly |= c.readonly;
                merged.cache = merged.cache.or(c.cache);
                merged.aio = merged.aio.clone().or(c.aio.clone());
                merged.discard = merged.discard.or(c.discard);
            }
            (file, Some(n.driver.clone()), merged)
        } else {
            (n.filename.clone(), Some("raw".into()), n)
        }
    }

    fn netdev_opt(&mut self, v: &str) {
        let kv = parse_opts(v, Some("type"));
        let get = |k: &str| kv.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        let Some(id) = get("id") else {
            self.warn("a -netdev without id was not imported".into());
            return;
        };
        let kind = get("type").unwrap_or_else(|| "user".into());
        let forwards = self.forwards_from(&kv);
        if kind == "user"
            && let Some(net) = get("net")
        {
            self.warn(format!(
                "NAT subnet {net} not kept on the NIC; create a NAT network with that subnet and attach the NIC to it"
            ));
        }
        self.netdevs.insert(id, Netdev { kind, forwards });
    }

    fn forwards_from(&mut self, kv: &[(String, String)]) -> Vec<PortForward> {
        let mut out = vec![];
        for (k, v) in kv {
            if k == "hostfwd" || (k.starts_with("hostfwd.") && k.ends_with(".str")) {
                match parse_hostfwd(v) {
                    Some((f, all_interfaces)) => {
                        if all_interfaces {
                            self.warn(format!(
                                "forward of host port {} listened on all interfaces; it now listens on 127.0.0.1 \
                                 (widen it in the editor if other machines need it)",
                                f.host_port
                            ));
                        }
                        out.push(f);
                    }
                    None => self.warn(format!("could not read port forward '{v}'; not imported")),
                }
            }
        }
        out
    }

    fn nic_opt(&mut self, v: &str) {
        let kv = parse_opts(v, Some("type"));
        let get = |k: &str| kv.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        let kind = get("type").unwrap_or_else(|| "user".into());
        if kind == "none" {
            return;
        }
        let forwards = self.forwards_from(&kv);
        let model = nic_model(&get("model").unwrap_or_else(|| "virtio-net-pci".into()));
        let mac = get("mac");
        let id = format!("#nic{}", self.netdevs.len());
        self.netdevs.insert(id.clone(), Netdev { kind, forwards });
        self.pending_nics.push(PendingNic {
            model,
            netdev: Some(id),
            mac,
        });
    }

    fn net_opt(&mut self, v: &str) {
        let kv = parse_opts(v, Some("type"));
        let get = |k: &str| kv.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        match get("type").as_deref() {
            Some("nic") => self.legacy_nics.push(PendingNic {
                model: nic_model(&get("model").unwrap_or_else(|| "e1000".into())),
                netdev: None,
                mac: get("macaddr"),
            }),
            Some("user") => {
                let f = self.forwards_from(&kv);
                self.legacy_forwards.extend(f);
            }
            Some("none") => {}
            Some(other) => self.warn(format!(
                "legacy -net {other} backend attached as NAT; pick a network for it in the editor"
            )),
            None => {}
        }
    }

    fn device_opt(&mut self, raw: &str, v: &str) {
        let kv = parse_opts(v, Some("driver"));
        let map: BTreeMap<String, String> = kv.iter().cloned().collect();
        let get = |k: &str| map.get(k).cloned();
        let driver = get("driver").unwrap_or_default();
        let boot: Option<u32> = get("bootindex").and_then(|b| b.parse().ok());
        let disk_bus = match driver.as_str() {
            "virtio-blk-pci" | "virtio-blk-device" | "virtio-blk-ccw" | "virtio-blk" => {
                Some((DiskBus::Virtio, Media::Disk))
            }
            "scsi-hd" | "scsi-disk" | "scsi-block" => Some((DiskBus::Scsi, Media::Disk)),
            "scsi-cd" => Some((DiskBus::Scsi, Media::Cdrom)),
            "ide-hd" | "ide-drive" => Some((DiskBus::Ide, Media::Disk)),
            "ide-cd" => Some((DiskBus::Ide, Media::Cdrom)),
            "nvme" => Some((DiskBus::Nvme, Media::Disk)),
            "usb-storage" => Some((DiskBus::Usb, Media::Disk)),
            _ => None,
        };
        if let Some((bus, media)) = disk_bus {
            let Some(drive) = get("drive") else {
                if media == Media::Cdrom {
                    self.warn("an empty CD drive was not imported; add one in the editor".into());
                } else {
                    self.warn(format!("{driver} without a drive was not imported"));
                }
                return;
            };
            let resolved = if let Some(d) = self.drives.get(&drive).cloned() {
                d.file.clone().map(|f| {
                    (
                        f,
                        d.format.clone(),
                        d.media,
                        d.readonly,
                        d.cache,
                        d.aio.clone(),
                        d.discard,
                    )
                })
            } else if self.nodes.contains_key(&drive) {
                let (file, format, n) = self.node_file(&drive);
                file.map(|f| {
                    (
                        f,
                        format,
                        Media::Disk,
                        n.readonly,
                        n.cache,
                        n.aio.clone(),
                        n.discard,
                    )
                })
            } else {
                None
            };
            let Some((file, format, dmedia, ro, cache, aio, disc)) = resolved else {
                self.warn(format!(
                    "{driver} refers to drive '{drive}', which has no file (or was not found); not imported"
                ));
                return;
            };
            let media = if media == Media::Cdrom || dmedia == Media::Cdrom {
                Media::Cdrom
            } else {
                Media::Disk
            };
            let i = self.add_disk(&file, format, Some(bus), media, ro, cache, aio, disc);
            if let Some(b) = boot {
                self.disks[i].boot_index = Some(b);
                self.explicit_boot = true;
            }
            return;
        }
        if NIC_MODELS.contains(&driver.as_str()) {
            self.pending_nics.push(PendingNic {
                model: driver,
                netdev: get("netdev"),
                mac: get("mac"),
            });
            return;
        }
        if DISPLAYS.contains(&driver.as_str()) {
            self.display = Some(Some(driver));
            return;
        }
        match driver.as_str() {
            "usb-tablet" => self.usb_tablet = true,
            "usb-host" => {
                let hex = |k: &str| {
                    get(k).and_then(|x| {
                        let x = x.trim_start_matches("0x").to_ascii_lowercase();
                        (x.len() <= 4 && u16::from_str_radix(&x, 16).is_ok()).then(|| format!("{x:0>4}"))
                    })
                };
                match (hex("vendorid"), hex("productid")) {
                    (Some(vendor_id), Some(product_id)) => self.usb.push(UsbPassthrough {
                        vendor_id,
                        product_id,
                        host_bus: get("hostbus").and_then(|b| b.parse().ok()),
                        host_port: get("hostport"),
                        label: None,
                    }),
                    _ => self.warn(
                        "a usb-host device matched only by bus/address was not imported; pick the device in the USB shelf"
                            .into(),
                    ),
                }
            }
            "usb-kbd" | "usb-mouse" => {}
            "virtio-9p-pci" | "virtio-9p-device" | "vhost-user-fs-pci" | "vhost-user-fs-device" => self.warn(
                "folder share not imported: add it under Shared folders after import (needs a share root)".into(),
            ),
            "virtio-serial-pci" | "virtio-serial" | "virtio-serial-device" | "virtserialport" | "virtconsole" => {
                self.managed.push(format!("-device {driver}"))
            }
            d if CONTROLLERS.contains(&d) => self.generated.push(d.to_string()),
            _ => match crate::import::generic_device(&driver, &kv) {
                Some(dev) if !driver.is_empty() => self.devices.push(dev),
                _ => self.keep(raw, Some(v)),
            },
        }
    }

    fn finish(mut self) -> ImportResult {
        let x86 = matches!(self.arch.as_str(), "x86_64" | "i386");
        // Machine
        let mut machine = self.machine.take().unwrap_or_default();
        if machine.type_name.is_empty() {
            machine.type_name = match self.arch.as_str() {
                "x86_64" | "i386" => "pc".into(),
                "ppc64" | "ppc64le" => "pseries".into(),
                "s390x" => "s390-ccw-virtio".into(),
                _ => {
                    self.warn(format!(
                        "no -machine given; used 'virt' for {} (check it in the editor)",
                        self.arch
                    ));
                    "virt".into()
                }
            };
        }
        // Default buses: IDE on x86 PCs, SCSI for CDs and virtio for disks elsewhere.
        for (i, d) in self.disks.iter_mut().enumerate() {
            if self.default_bus[i] {
                d.bus = match (x86, d.media) {
                    (true, _) => DiskBus::Ide,
                    (false, Media::Cdrom) => DiskBus::Scsi,
                    (false, Media::Disk) => DiskBus::Virtio,
                };
            }
        }
        // Boot order from -boot when no device had a bootindex.
        if !self.explicit_boot {
            let order = self.boot_order.clone().unwrap_or_else(|| "cd".into());
            let mut next = 1;
            for c in order.chars() {
                let want = match c {
                    'c' => Media::Disk,
                    'd' => Media::Cdrom,
                    _ => continue,
                };
                if let Some(d) = self
                    .disks
                    .iter_mut()
                    .find(|d| d.media == want && d.boot_index.is_none())
                {
                    d.boot_index = Some(next);
                    next += 1;
                }
            }
        }
        // CPU
        let mut cpu = self.cpu.clone();
        let smp: BTreeMap<String, String> =
            self.smp.take().unwrap_or_default().into_iter().collect();
        let n = |k: &str| smp.get(k).and_then(|v| v.parse::<u32>().ok());
        let (sockets, cores, threads) = (n("sockets"), n("cores"), n("threads"));
        cpu.count = n("cpus")
            .or_else(|| Some(sockets.unwrap_or(1) * cores.unwrap_or(1) * threads.unwrap_or(1)))
            .unwrap_or(1)
            .max(1);
        if sockets.is_some() || cores.is_some() || threads.is_some() {
            let c = cores.unwrap_or(1);
            let t = threads.unwrap_or(1);
            let s = sockets.unwrap_or_else(|| (cpu.count / (c * t).max(1)).max(1));
            if s * c * t == cpu.count {
                cpu.topology = Some(Topology {
                    sockets: s,
                    cores: c,
                    threads: t,
                });
            } else {
                self.warn(format!(
                    "CPU topology {s}×{c}×{t} does not multiply to {} vCPUs; topology not imported",
                    cpu.count
                ));
            }
        }
        if smp.contains_key("maxcpus") {
            self.warn(
                "CPU hotplug (-smp maxcpus) is not modelled; the VM gets a fixed vCPU count".into(),
            );
        }
        if let Some(d) = smp.get("dies").filter(|d| *d != "1") {
            self.warn(format!(
                "{d} dies per socket not modelled; topology may differ"
            ));
        }
        // NICs: -device/-nic with netdevs, then legacy -net.
        let pending = std::mem::take(&mut self.pending_nics);
        for p in pending {
            let nd = p.netdev.as_ref().and_then(|id| self.netdevs.get(id));
            let forwards = match nd {
                Some(nd) if nd.kind == "user" => nd.forwards.clone(),
                Some(nd) => {
                    let w = format!(
                        "NIC {} used a '{}' network; it is attached to NAT for now — pick a network for it in the editor",
                        self.nics.len(),
                        nd.kind
                    );
                    self.warn(w);
                    vec![]
                }
                None => {
                    if p.netdev.is_some() {
                        self.warn(format!(
                            "NIC {} refers to an unknown netdev; attached to NAT",
                            self.nics.len()
                        ));
                    }
                    vec![]
                }
            };
            let nic = self.make_nic(p.model, p.mac, forwards);
            self.nics.push(nic);
        }
        let legacy = std::mem::take(&mut self.legacy_nics);
        let mut lf = std::mem::take(&mut self.legacy_forwards);
        for p in legacy {
            let nic = self.make_nic(p.model, p.mac, std::mem::take(&mut lf));
            self.nics.push(nic);
        }
        // Display
        let device = match self.display.take() {
            Some(d) => d,
            None if self.nographic || self.nodefaults || !x86 => None,
            None => Some("VGA".into()),
        };
        // Notes
        if !self.managed.is_empty() {
            let mut m = self.managed.clone();
            m.sort();
            m.dedup();
            self.warn(format!(
                "managed by qemu-ui, not imported: {} (monitors, consoles and the display are set up automatically)",
                m.join(" ")
            ));
        }
        if !self.generated.is_empty() {
            let mut g = self.generated.clone();
            g.sort();
            g.dedup();
            self.warn(format!(
                "controllers are generated from the disks and USB devices, not imported: {}",
                g.join(", ")
            ));
        }
        if !self.passthrough.is_empty() {
            self.warn(format!(
                "kept as passthrough (expert, applied verbatim): {}",
                self.passthrough
                    .iter()
                    .filter(|t| t.starts_with('-'))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        let accel = self
            .accel
            .take()
            .map(|a| if a == "hax" { "auto".into() } else { a })
            .unwrap_or_else(|| "auto".into());
        let name = suggest_vm_name(self.spec_name.as_deref().unwrap_or("imported-vm"));
        let memory = self.memory_mib.unwrap_or(128);
        if self.memory_mib.is_none() {
            self.warn("no -m given; used QEMU's default of 128 MiB".into());
        }
        let spec = VmSpec {
            limits: Default::default(),
            guest: Default::default(),
            spec_version: SPEC_VERSION,
            name,
            arch: self.arch.clone(),
            machine,
            accel,
            cpu,
            memory: MemorySpec { size_mib: memory },
            firmware: FirmwareSpec::Default,
            disks: self.disks,
            nics: self.nics,
            display: DisplaySpec {
                device,
                vnc: true,
                vdagent: false,
                spice: false,
            },
            input: InputSpec {
                usb_tablet: self.usb_tablet,
            },
            shares: vec![],
            usb: self.usb,
            devices: self.devices,
            passthrough: self.passthrough,
        };
        ImportResult {
            spec,
            warnings: self.warnings,
            files_to_map: self.files,
            firmware_hint: self.firmware,
        }
    }

    fn make_nic(
        &mut self,
        model: String,
        mac: Option<String>,
        forwards: Vec<PortForward>,
    ) -> NicSpec {
        let mac = match mac {
            Some(m) if valid_mac(&m.to_ascii_lowercase()) => Some(m.to_ascii_lowercase()),
            Some(m) => {
                self.warn(format!(
                    "MAC {m} is not a valid unicast address; a new one is assigned"
                ));
                None
            }
            None => None,
        };
        NicSpec {
            backend: NicBackend::User { forwards },
            model,
            mac,
            queues: None,
            virtio: None,
        }
    }
}

/// `-nic model=` short names to device names.
fn nic_model(m: &str) -> String {
    match m {
        "virtio" | "virtio-net" => "virtio-net-pci".into(),
        "e1000-82540em" => "e1000".into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_shells() {
        assert_eq!(
            tokenize("qemu-system-x86_64 \\\n  -name 'my vm' -drive \"file=/a b.qcow2\"").unwrap(),
            [
                "qemu-system-x86_64",
                "-name",
                "my vm",
                "-drive",
                "file=/a b.qcow2"
            ]
        );
        assert_eq!(
            tokenize("qemu-system-x86_64.exe ^\r\n -hda C:\\vms\\win.qcow2 `\n -m 2G").unwrap(),
            [
                "qemu-system-x86_64.exe",
                "-hda",
                "C:\\vms\\win.qcow2",
                "-m",
                "2G"
            ]
        );
        assert_eq!(tokenize("a\\ b").unwrap(), ["a b"]);
        assert!(tokenize("-name 'x").is_err());
    }

    #[test]
    fn opts() {
        assert_eq!(
            parse_opts("q35,accel=kvm,smm=on", Some("type")),
            [
                ("type".into(), "q35".into()),
                ("accel".into(), "kvm".into()),
                ("smm".into(), "on".into())
            ]
        );
        assert_eq!(parse_opts("file=a,,b.img,if=none", None)[0].1, "a,b.img");
        let j = parse_opts(
            r#"{"driver":"qcow2","node-name":"d0","file":{"driver":"file","filename":"/x.qcow2"},"read-only":true}"#,
            None,
        );
        assert!(j.contains(&("file.filename".into(), "/x.qcow2".into())));
        assert!(j.contains(&("read-only".into(), "on".into())));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mib("4G"), Some(4096));
        assert_eq!(parse_size_mib("4096"), Some(4096));
        assert_eq!(parse_size_mib("512M"), Some(512));
        assert_eq!(parse_size_mib("1.5G"), Some(1536));
        assert_eq!(parse_size_mib("2GiB"), Some(2048));
        assert_eq!(parse_size_mib("x"), None);
    }

    #[test]
    fn hostfwd() {
        let (f, all) = parse_hostfwd("tcp::2222-:22").unwrap();
        assert_eq!(
            (f.host_addr.as_str(), f.host_port, f.guest_port, all),
            ("127.0.0.1", 2222, 22, true)
        );
        let (f, all) = parse_hostfwd("udp:127.0.0.1:5353-10.0.2.15:53").unwrap();
        assert_eq!(
            (f.proto.as_str(), f.host_port, f.guest_port, all),
            ("udp", 5353, 53, false)
        );
        let (f, _) = parse_hostfwd("::8080-:80").unwrap();
        assert_eq!(f.proto, "tcp");
    }

    #[test]
    fn imports_a_typical_linux_command_line() {
        let r = import_argv(
            r#"/usr/bin/qemu-system-x86_64 -name guest=web01,debug-threads=on \
              -machine q35,accel=kvm,smm=on -cpu host,+invtsc,-hle -smp 4,sockets=1,cores=2,threads=2 -m 4G \
              -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE.secboot.fd \
              -drive if=pflash,format=raw,file=/var/lib/vms/web01_VARS.fd \
              -drive file=/var/lib/vms/web01.qcow2,if=none,id=d0,format=qcow2,cache=none,discard=unmap \
              -device virtio-blk-pci,drive=d0,bootindex=1 \
              -cdrom /srv/iso/installer.iso \
              -netdev user,id=n0,hostfwd=tcp::2222-:22,hostfwd=tcp:127.0.0.1:8080-:80 \
              -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
              -device qemu-xhci -device usb-tablet -vga virtio \
              -device usb-host,vendorid=0x046d,productid=0xc52b \
              -rtc base=utc -qmp unix:/run/x.sock,server,nowait -display none -daemonize"#,
        )
        .unwrap();
        let s = &r.spec;
        assert_eq!(s.name, "web01");
        assert_eq!(
            (
                s.arch.as_str(),
                s.machine.type_name.as_str(),
                s.accel.as_str()
            ),
            ("x86_64", "q35", "kvm")
        );
        assert_eq!(s.machine.props.get("smm").map(String::as_str), Some("on"));
        assert_eq!(s.cpu.model.as_deref(), Some("host"));
        assert_eq!(s.cpu.props.get("invtsc").map(String::as_str), Some("on"));
        assert_eq!(s.cpu.props.get("hle").map(String::as_str), Some("off"));
        assert_eq!(s.cpu.count, 4);
        assert_eq!(
            s.cpu.topology,
            Some(Topology {
                sockets: 1,
                cores: 2,
                threads: 2
            })
        );
        assert_eq!(s.memory.size_mib, 4096);
        assert_eq!(r.firmware_hint, Some(FirmwareHint::UefiSecureBoot));
        assert_eq!(s.disks.len(), 2);
        assert_eq!(
            (s.disks[0].bus, s.disks[0].boot_index, s.disks[0].cache),
            (DiskBus::Virtio, Some(1), CacheMode::None)
        );
        assert_eq!(
            (s.disks[1].bus, s.disks[1].media, s.disks[1].readonly),
            (DiskBus::Ide, Media::Cdrom, true)
        );
        assert_eq!(r.files_to_map[0].path, "/var/lib/vms/web01.qcow2");
        assert_eq!(r.files_to_map[0].format.as_deref(), Some("qcow2"));
        assert_eq!(s.disks[0].image.name, "web01.qcow2");
        assert_eq!(s.nics.len(), 1);
        assert_eq!(s.nics[0].mac.as_deref(), Some("52:54:00:12:34:56"));
        assert_eq!(s.nics[0].backend.forwards().len(), 2);
        assert_eq!(s.display.device.as_deref(), Some("virtio-vga"));
        assert!(s.input.usb_tablet);
        assert_eq!(s.usb[0].vendor_id, "046d");
        assert_eq!(s.passthrough, ["-rtc", "base=utc"]);
        assert!(r.warnings.iter().any(|w| w.contains("NVRAM")));
        assert!(r.warnings.iter().any(|w| w.contains("-qmp")));
        assert!(r.warnings.iter().any(|w| w.contains("qemu-xhci")));
        assert!(r.warnings.iter().any(|w| w.contains("all interfaces")));
    }

    #[test]
    fn imports_a_windows_style_command_line_with_blockdev() {
        let r = import_argv(
            r#""C:\Program Files\qemu\qemu-system-x86_64.exe" -M pc -accel whpx -smp 2 -m 2048 `
               -blockdev '{"driver":"qcow2","node-name":"sys","file":{"driver":"file","filename":"D:\\vms\\win10.qcow2"}}' `
               -device ide-hd,drive=sys,bootindex=0 `
               -hdb D:\vms\data.vhdx `
               -nic user,model=e1000,hostfwd=tcp:127.0.0.1:3389-:3389 `
               -drive file=\\.\PhysicalDrive2,format=raw,if=virtio -usbdevice tablet"#,
        )
        .unwrap();
        let s = &r.spec;
        assert_eq!(
            (s.machine.type_name.as_str(), s.accel.as_str()),
            ("pc", "whpx")
        );
        assert_eq!(s.disks.len(), 3);
        assert_eq!(r.files_to_map[0].path, r"D:\vms\win10.qcow2");
        assert_eq!(r.files_to_map[0].format.as_deref(), Some("qcow2"));
        assert_eq!(
            (s.disks[0].bus, s.disks[0].boot_index),
            (DiskBus::Ide, Some(0))
        );
        assert_eq!(s.disks[1].image.name, "data.vhdx");
        assert_eq!(r.files_to_map[2].usage, FileUse::HostDevice);
        assert_eq!(s.nics[0].model, "e1000");
        assert_eq!(s.nics[0].backend.forwards()[0].guest_port, 3389);
        assert_eq!(s.display.device.as_deref(), Some("VGA"), "x86 default VGA");
        assert!(s.input.usb_tablet);
    }

    #[test]
    fn imports_aarch64_and_keeps_unknowns() {
        let r = import_argv(
            "qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a72 -m 1024 -nographic \
             -drive if=pflash,format=raw,readonly=on,file=/usr/share/AAVMF/AAVMF_CODE.fd \
             -drive file=/vm/arm.img,format=raw -cdrom /iso/arm.iso -boot d \
             -netdev tap,id=t0,ifname=tap0 -device virtio-net-device,netdev=t0 \
             -device virtio-rng-pci,max-bytes=1024,period=1000 -device i6300esb              -object rng-random,id=rng0,filename=/dev/urandom -device virtio-rng-pci,rng=rng0",
        )
        .unwrap();
        let s = &r.spec;
        assert_eq!(
            (s.arch.as_str(), s.machine.type_name.as_str()),
            ("aarch64", "virt")
        );
        assert_eq!(
            s.machine.props.get("gic-version").map(String::as_str),
            Some("3")
        );
        assert_eq!(r.firmware_hint, Some(FirmwareHint::Uefi));
        assert_eq!(s.disks[0].bus, DiskBus::Virtio);
        assert_eq!(s.disks[1].bus, DiskBus::Scsi, "CDs use SCSI off x86");
        assert_eq!(s.disks[1].boot_index, Some(1), "-boot d puts the CD first");
        assert_eq!(s.disks[0].boot_index, None);
        assert_eq!(s.display.device, None);
        assert!(r.warnings.iter().any(|w| w.contains("'tap' network")));
        // Stand-alone devices become generic devices with typed properties.
        assert_eq!(s.devices.len(), 2, "{:?}", s.devices);
        assert_eq!(s.devices[0].driver, "virtio-rng-pci");
        assert_eq!(
            s.devices[0].props.get("max-bytes"),
            Some(&serde_json::json!(1024))
        );
        assert_eq!(s.devices[1].driver, "i6300esb");
        // A device naming another object by id stays in passthrough with it.
        assert!(s.passthrough.contains(&"-object".into()));
        assert!(s.passthrough.contains(&"virtio-rng-pci,rng=rng0".into()));
    }

    #[test]
    fn rejects_non_command_lines() {
        assert!(import_argv("hello world").is_err());
        assert!(import_argv("").is_err());
    }
}
