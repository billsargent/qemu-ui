//! Import a libvirt domain XML document into a VM spec (M4-07).
//!
//! Like the argv importer this is best-effort and pure: host paths go to
//! `files_to_map`, elements qemu-ui does not model become warnings (never
//! silent drops), and `<qemu:commandline>` arguments become passthrough.

use roxmltree::{Document, Node};

use crate::import::{
    FileToMap, FileUse, FirmwareHint, ImportResult, PLACEHOLDER_POOL, is_block_device_path,
    looks_like_secure_boot, looks_like_uefi, suggest_image_name, suggest_vm_name,
};
use crate::spec::{
    CacheMode, CpuSpec, DiskBus, DiskSpec, DisplaySpec, FirmwareSpec, ImageRef, InputSpec,
    MachineSpec, Media, MemorySpec, NicBackend, NicSpec, PortForward, SPEC_VERSION, Topology,
    UsbPassthrough, VmSpec, valid_mac,
};

/// Import a libvirt `<domain>` document.
pub fn import_libvirt(xml: &str) -> Result<ImportResult, String> {
    let doc = Document::parse(xml).map_err(|e| format!("not valid XML: {e}"))?;
    let dom = doc.root_element();
    if dom.tag_name().name() != "domain" {
        return Err(format!(
            "expected a libvirt <domain> document, found <{}>",
            dom.tag_name().name()
        ));
    }
    let mut im = Lv::default();
    Ok(im.domain(dom))
}

#[derive(Default)]
struct Lv {
    warnings: Vec<String>,
    files: Vec<FileToMap>,
    firmware: Option<FirmwareHint>,
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text(n: Option<Node>) -> Option<String> {
    n.and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn attr(n: Option<Node>, a: &str) -> Option<String> {
    n.and_then(|n| n.attribute(a)).map(String::from)
}

/// A libvirt memory value with its unit (default KiB) in MiB.
pub fn memory_mib(value: &str, unit: Option<&str>) -> Option<u64> {
    let v: f64 = value.trim().parse().ok()?;
    let bytes = match unit.unwrap_or("KiB") {
        "b" | "bytes" => 1.0,
        "KB" => 1000.0,
        "k" | "KiB" => 1024.0,
        "MB" => 1e6,
        "M" | "MiB" => 1024.0 * 1024.0,
        "GB" => 1e9,
        "G" | "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1e12,
        "T" | "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    let mib = (v * bytes / (1024.0 * 1024.0)).round();
    (mib >= 1.0).then_some(mib as u64)
}

impl Lv {
    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    fn domain(&mut self, dom: Node) -> ImportResult {
        let accel = match dom.attribute("type") {
            Some("kvm") => "kvm",
            Some("qemu") => "tcg",
            Some("hvf") => "hvf",
            _ => "auto",
        }
        .to_string();
        let name =
            suggest_vm_name(&text(child(dom, "name")).unwrap_or_else(|| "imported-vm".into()));
        let os = child(dom, "os");
        let os_type = os.and_then(|o| child(o, "type"));
        let arch = attr(os_type, "arch").unwrap_or_else(|| "x86_64".into());
        let x86 = matches!(arch.as_str(), "x86_64" | "i686" | "i386");
        let s390 = arch == "s390x";
        let arch = if arch == "i686" { "i386".into() } else { arch };
        let mut machine = MachineSpec {
            type_name: attr(os_type, "machine")
                .unwrap_or_else(|| if x86 { "pc" } else { "virt" }.into()),
            ..Default::default()
        };
        let memory = child(dom, "memory")
            .and_then(|m| memory_mib(m.text().unwrap_or_default(), m.attribute("unit")))
            .unwrap_or_else(|| {
                self.warn("no <memory>; used 512 MiB".into());
                512
            });
        let mut cpu = self.cpu(dom);
        if let Some(o) = os {
            self.os(o);
        }
        let features = child(dom, "features");
        if let Some(f) = features {
            self.features(f, &mut machine, &mut cpu);
        }
        let mut res = Devices::default();
        if let Some(devs) = child(dom, "devices") {
            res = self.devices(devs, x86, s390);
        }
        // Boot order from <os><boot dev=…/> when no device had <boot order>.
        if !res.explicit_boot
            && let Some(o) = os
        {
            let mut next = 1;
            for b in children(o, "boot") {
                let want = match b.attribute("dev") {
                    Some("hd") => Media::Disk,
                    Some("cdrom") => Media::Cdrom,
                    _ => continue,
                };
                if let Some(d) = res
                    .disks
                    .iter_mut()
                    .find(|d| d.media == want && d.boot_index.is_none())
                {
                    d.boot_index = Some(next);
                    next += 1;
                }
            }
        }
        let mut passthrough = vec![];
        for cl in dom
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "commandline")
        {
            for a in children(cl, "arg") {
                if let Some(v) = a.attribute("value") {
                    passthrough.push(v.to_string());
                }
            }
        }
        if !passthrough.is_empty() {
            self.warn(format!(
                "<qemu:commandline> kept as passthrough (expert, applied verbatim): {}",
                passthrough.join(" ")
            ));
        }
        let spec = VmSpec {
            spec_version: SPEC_VERSION,
            name,
            arch,
            machine,
            accel,
            cpu,
            memory: MemorySpec { size_mib: memory },
            firmware: FirmwareSpec::Default,
            disks: res.disks,
            nics: res.nics,
            display: DisplaySpec {
                device: res.display,
                vnc: true,
                vdagent: false,
                spice: false,
            },
            input: InputSpec {
                usb_tablet: res.tablet,
            },
            shares: vec![],
            usb: res.usb,
            devices: res.devices,
            passthrough,
        };
        ImportResult {
            spec,
            warnings: std::mem::take(&mut self.warnings),
            files_to_map: std::mem::take(&mut self.files),
            firmware_hint: self.firmware,
        }
    }

    fn cpu(&mut self, dom: Node) -> CpuSpec {
        let count = text(child(dom, "vcpu"))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);
        if child(dom, "vcpu")
            .and_then(|v| v.attribute("current"))
            .is_some()
        {
            self.warn(
                "vCPU hotplug (<vcpu current=…>) is not modelled; all vCPUs are present".into(),
            );
        }
        let mut cpu = CpuSpec {
            count,
            ..Default::default()
        };
        let Some(c) = child(dom, "cpu") else {
            return cpu;
        };
        match c.attribute("mode") {
            Some("host-passthrough") => cpu.model = Some("host".into()),
            Some("maximum") => cpu.model = Some("max".into()),
            Some("host-model") => {
                cpu.model = Some("host".into());
                self.warn("CPU mode host-model imported as host passthrough".into());
            }
            _ => cpu.model = text(child(c, "model")),
        }
        for f in children(c, "feature") {
            let Some(name) = f.attribute("name") else {
                continue;
            };
            match f.attribute("policy").unwrap_or("require") {
                "require" | "force" => {
                    cpu.props.insert(name.into(), "on".into());
                }
                "disable" | "forbid" => {
                    cpu.props.insert(name.into(), "off".into());
                }
                _ => {}
            }
        }
        if let Some(t) = child(c, "topology") {
            let n = |a: &str| t.attribute(a).and_then(|v| v.parse::<u32>().ok());
            let (s, d, cl, co, th) = (
                n("sockets").unwrap_or(1),
                n("dies").unwrap_or(1),
                n("clusters").unwrap_or(1),
                n("cores").unwrap_or(1),
                n("threads").unwrap_or(1),
            );
            if d != 1 || cl != 1 {
                self.warn("CPU dies/clusters are not modelled; folded into sockets".into());
            }
            let topo = Topology {
                sockets: s * d * cl,
                cores: co,
                threads: th,
            };
            if topo.sockets * topo.cores * topo.threads == count {
                cpu.topology = Some(topo);
            } else {
                self.warn(format!(
                    "CPU topology {}×{}×{} does not match {count} vCPUs; topology not imported",
                    topo.sockets, topo.cores, topo.threads
                ));
            }
        }
        cpu
    }

    fn os(&mut self, os: Node) {
        let efi = os.attribute("firmware") == Some("efi");
        let loader = child(os, "loader");
        let secure_feature = child(os, "firmware").is_some_and(|fw| {
            children(fw, "feature").any(|f| {
                f.attribute("name") == Some("secure-boot") && f.attribute("enabled") == Some("yes")
            })
        });
        let loader_path = text(loader);
        let uefi_loader = loader.is_some_and(|l| l.attribute("type") == Some("pflash"))
            || loader_path.as_deref().is_some_and(looks_like_uefi);
        if efi || uefi_loader {
            let secure = secure_feature
                || loader.and_then(|l| l.attribute("secure")) == Some("yes")
                || loader_path.as_deref().is_some_and(looks_like_secure_boot);
            self.firmware = Some(if secure {
                FirmwareHint::UefiSecureBoot
            } else {
                FirmwareHint::Uefi
            });
        }
        if child(os, "nvram").is_some() {
            self.warn(
                "UEFI variables (NVRAM) were not imported; the VM starts with a fresh variable store, \
                 so boot entries may need recreating"
                    .into(),
            );
        }
        for k in ["kernel", "initrd", "cmdline", "dtb"] {
            if child(os, k).is_some() {
                self.warn(format!(
                    "direct kernel boot (<{k}>) not imported; add -kernel/-initrd as passthrough if needed"
                ));
            }
        }
    }

    fn features(&mut self, f: Node, machine: &mut MachineSpec, cpu: &mut CpuSpec) {
        for x in f.children().filter(|c| c.is_element()) {
            let state_on = x.attribute("state").is_none_or(|s| s == "on");
            match x.tag_name().name() {
                "smm" if state_on => {
                    machine.props.insert("smm".into(), "on".into());
                }
                "hyperv" => {
                    for h in x.children().filter(|c| c.is_element()) {
                        if h.attribute("state") != Some("on") {
                            continue;
                        }
                        let n = h.tag_name().name();
                        match n {
                            "spinlocks" => {
                                let retries = h
                                    .attribute("retries")
                                    .and_then(|r| r.parse::<u32>().ok())
                                    .unwrap_or(8191);
                                cpu.props
                                    .insert("hv-spinlocks".into(), format!("0x{retries:x}"));
                            }
                            "vendor_id" => {
                                if let Some(v) = h.attribute("value") {
                                    cpu.props.insert("hv-vendor-id".into(), v.into());
                                }
                            }
                            "relaxed" | "vapic" | "vpindex" | "runtime" | "synic" | "stimer"
                            | "reset" | "frequencies" | "reenlightenment" | "tlbflush" | "ipi"
                            | "evmcs" | "avic" => {
                                cpu.props.insert(format!("hv-{n}"), "on".into());
                            }
                            other => {
                                self.warn(format!("Hyper-V enlightenment '{other}' not imported"))
                            }
                        }
                    }
                }
                "kvm" => {
                    if child(x, "hidden").and_then(|h| h.attribute("state")) == Some("on") {
                        cpu.props.insert("kvm".into(), "off".into());
                    }
                }
                "acpi" | "apic" | "pae" | "vmport" | "ioapic" | "gic" | "vmcoreinfo" | "smm"
                | "pmu" => {}
                other => self.warn(format!("feature <{other}> not imported")),
            }
        }
    }

    fn devices(&mut self, devs: Node, x86: bool, s390: bool) -> Devices {
        let mut out = Devices::default();
        let mut skipped: Vec<String> = vec![];
        for d in devs.children().filter(|c| c.is_element()) {
            match d.tag_name().name() {
                "disk" => self.disk(d, &mut out, x86),
                "interface" => self.interface(d, &mut out),
                "video" => {
                    let model = child(d, "model");
                    let device = match attr(model, "type").as_deref() {
                        Some("virtio") if x86 => Some("virtio-vga".into()),
                        Some("virtio") => Some("virtio-gpu-pci".into()),
                        Some("vga") => Some("VGA".into()),
                        Some("qxl") => Some("qxl-vga".into()),
                        Some("cirrus") => Some("cirrus-vga".into()),
                        Some("bochs") => Some("bochs-display".into()),
                        Some("ramfb") => Some("ramfb".into()),
                        Some("vmvga") => Some("vmware-svga".into()),
                        Some("none") => None,
                        other => {
                            self.warn(format!("video model {other:?} not supported; used VGA"));
                            Some("VGA".into())
                        }
                    };
                    if out.display_seen {
                        self.warn("only the first video device is imported".into());
                    } else {
                        out.display = device;
                        out.display_seen = true;
                    }
                }
                "input" => {
                    if d.attribute("type") == Some("tablet") {
                        out.tablet = true;
                    }
                }
                "hostdev" => self.hostdev(d, &mut out),
                "filesystem" => {
                    let src = attr(child(d, "source"), "dir").unwrap_or_default();
                    let tag = attr(child(d, "target"), "dir").unwrap_or_default();
                    self.warn(format!(
                        "folder share '{tag}' ({src}) not imported: add it under Shared folders after import \
                         (needs a share root)"
                    ));
                }
                "graphics" | "controller" | "emulator" | "serial" | "console" | "channel"
                | "memballoon" => {}
                "tpm" => self.warn("TPM not imported (not modelled yet)".into()),
                "sound" | "audio" => self.warn("sound device not imported (not modelled)".into()),
                "watchdog" => match d.attribute("model") {
                    Some(m @ ("i6300esb" | "ib700" | "diag288")) => out.devices.push(device(m)),
                    other => self.warn(format!("watchdog model {other:?} not imported")),
                },
                "panic" => match d.attribute("model") {
                    Some("isa") | None if x86 => out.devices.push(device("pvpanic")),
                    Some("pci") => out.devices.push(device("pvpanic-pci")),
                    other => self.warn(format!("panic device model {other:?} not imported")),
                },
                "rng" => {
                    let backend = child(d, "backend");
                    if d.attribute("model") == Some("virtio")
                        && backend.and_then(|b| b.attribute("model")) == Some("random")
                    {
                        // QEMU's default RNG backend reads the host's random source.
                        out.devices.push(device(if s390 {
                            "virtio-rng-ccw"
                        } else {
                            "virtio-rng-pci"
                        }));
                    } else {
                        self.warn("RNG device with a non-random backend not imported".into());
                    }
                }
                other => skipped.push(other.into()),
            }
        }
        if !out.display_seen {
            out.display = if x86 { Some("VGA".into()) } else { None };
        }
        if !skipped.is_empty() {
            skipped.sort();
            skipped.dedup();
            self.warn(format!("devices not imported: {}", skipped.join(", ")));
        }
        out
    }

    fn disk(&mut self, d: Node, out: &mut Devices, x86: bool) {
        let device = d.attribute("device").unwrap_or("disk");
        let media = match device {
            "disk" => Media::Disk,
            "cdrom" => Media::Cdrom,
            other => {
                self.warn(format!("{other} disk not imported (unsupported)"));
                return;
            }
        };
        let source = child(d, "source");
        let (path, usage) = match d.attribute("type").unwrap_or("file") {
            "file" => (attr(source, "file"), FileUse::Image),
            "block" => (attr(source, "dev"), FileUse::HostDevice),
            "volume" => {
                let p = attr(source, "pool").unwrap_or_default();
                let v = attr(source, "volume").unwrap_or_default();
                self.warn(format!(
                    "disk uses libvirt storage volume {p}/{v}; map it to the image file it refers to"
                ));
                (Some(format!("{p}/{v}")), FileUse::Image)
            }
            other => {
                self.warn(format!(
                    "{other} disk not imported (network and other sources are not supported)"
                ));
                return;
            }
        };
        let Some(path) = path else {
            if media == Media::Cdrom {
                self.warn("an empty CD drive was not imported; add one in the editor".into());
            } else {
                self.warn("a disk without a source was not imported".into());
            }
            return;
        };
        let usage = if is_block_device_path(&path) {
            FileUse::HostDevice
        } else {
            usage
        };
        let target = child(d, "target");
        let bus = match attr(target, "bus").as_deref() {
            Some("virtio") => DiskBus::Virtio,
            Some("scsi") => DiskBus::Scsi,
            Some("sata") | Some("ide") => DiskBus::Ide,
            Some("usb") => DiskBus::Usb,
            Some("nvme") => DiskBus::Nvme,
            Some(other) => {
                self.warn(format!(
                    "disk bus '{other}' not supported; used the default"
                ));
                if x86 { DiskBus::Ide } else { DiskBus::Virtio }
            }
            None => {
                if x86 {
                    DiskBus::Ide
                } else {
                    DiskBus::Virtio
                }
            }
        };
        let driver = child(d, "driver");
        let cache = match attr(driver, "cache").as_deref() {
            Some("none") => CacheMode::None,
            Some("writeback") | Some("default") => CacheMode::Writeback,
            Some("writethrough") => CacheMode::Writethrough,
            Some("directsync") => CacheMode::Directsync,
            Some("unsafe") => CacheMode::Unsafe,
            _ => CacheMode::None,
        };
        let index = out.disks.len();
        if usage == FileUse::HostDevice {
            self.warn(format!(
                "disk {index} is a host device ({path}); attach it as a host disk through the qemu-ui helper"
            ));
        }
        let mut name = suggest_image_name(&path);
        let taken: Vec<String> = self
            .files
            .iter()
            .map(|f| f.suggested_name.clone())
            .collect();
        if taken.contains(&name) {
            let mut n = 2;
            while taken.contains(&format!("{n}-{name}")) {
                n += 1;
            }
            name = format!("{n}-{name}");
        }
        let boot: Option<u32> = attr(child(d, "boot"), "order").and_then(|o| o.parse().ok());
        if boot.is_some() {
            out.explicit_boot = true;
        }
        self.files.push(FileToMap {
            path,
            usage,
            disk: index,
            suggested_name: name.clone(),
            format: attr(driver, "type"),
            mapped: None,
        });
        out.disks.push(DiskSpec {
            image: ImageRef {
                pool: PLACEHOLDER_POOL.into(),
                name,
            },
            bus,
            media,
            readonly: media == Media::Cdrom || child(d, "readonly").is_some(),
            boot_index: boot,
            cache,
            aio: attr(driver, "io"),
            discard: attr(driver, "discard").as_deref() != Some("ignore"),
            slot: None,
        });
    }

    fn interface(&mut self, i: Node, out: &mut Devices) {
        let kind = i.attribute("type").unwrap_or("user");
        let n = out.nics.len();
        match kind {
            "user" => {}
            "network" => {
                let net = attr(child(i, "source"), "network").unwrap_or_default();
                self.warn(format!(
                    "NIC {n} was on libvirt network '{net}'; attached to NAT — pick a qemu-ui network in the editor"
                ));
            }
            "bridge" => {
                let br = attr(child(i, "source"), "bridge").unwrap_or_default();
                self.warn(format!(
                    "NIC {n} was bridged to '{br}'; attached to NAT — create a bridged network for it"
                ));
            }
            other => self.warn(format!(
                "NIC {n} used a '{other}' interface; attached to NAT — pick a network in the editor"
            )),
        }
        let mut forwards = vec![];
        for pf in children(i, "portForward") {
            let proto = pf.attribute("proto").unwrap_or("tcp").to_string();
            let addr = pf.attribute("address").unwrap_or("127.0.0.1").to_string();
            for r in children(pf, "range") {
                let Some(start) = r.attribute("start").and_then(|s| s.parse::<u16>().ok()) else {
                    continue;
                };
                if r.attribute("end").is_some() {
                    self.warn(
                        "port ranges in <portForward> are imported as their first port only".into(),
                    );
                }
                let guest = r
                    .attribute("to")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(start);
                forwards.push(PortForward {
                    proto: proto.clone(),
                    host_addr: addr.clone(),
                    host_port: start,
                    guest_port: guest,
                });
            }
        }
        let model = match attr(child(i, "model"), "type").as_deref() {
            Some("virtio") | None => "virtio-net-pci".to_string(),
            Some(m) => m.to_string(),
        };
        let mac = attr(child(i, "mac"), "address")
            .map(|m| m.to_ascii_lowercase())
            .filter(|m| {
                let ok = valid_mac(m);
                if !ok {
                    self.warnings.push(format!(
                        "MAC {m} is not a valid unicast address; a new one is assigned"
                    ));
                }
                ok
            });
        out.nics.push(NicSpec {
            backend: NicBackend::User { forwards },
            model,
            mac,
            queues: None,
            virtio: None,
        });
    }

    fn hostdev(&mut self, h: Node, out: &mut Devices) {
        match h.attribute("type") {
            Some("usb") => {
                let src = child(h, "source");
                let hex = |n: &str| {
                    attr(src.and_then(|s| child(s, n)), "id").and_then(|x| {
                        let x = x.trim_start_matches("0x").to_ascii_lowercase();
                        (x.len() <= 4 && u16::from_str_radix(&x, 16).is_ok())
                            .then(|| format!("{x:0>4}"))
                    })
                };
                match (hex("vendor"), hex("product")) {
                    (Some(vendor_id), Some(product_id)) => out.usb.push(UsbPassthrough {
                        vendor_id,
                        product_id,
                        host_bus: None,
                        host_port: None,
                        label: None,
                    }),
                    _ => self.warn(
                        "a USB device matched only by bus/address was not imported; pick it in the USB shelf".into(),
                    ),
                }
            }
            Some("pci") => self.warn("PCI passthrough (VFIO) not imported (not supported)".into()),
            other => self.warn(format!("host device {other:?} not imported")),
        }
    }
}

/// A generic device with QEMU's defaults.
fn device(driver: &str) -> crate::devices::DeviceSpec {
    crate::devices::DeviceSpec {
        driver: driver.into(),
        bus: None,
        props: Default::default(),
    }
}

#[derive(Default)]
struct Devices {
    devices: Vec<crate::devices::DeviceSpec>,
    disks: Vec<DiskSpec>,
    nics: Vec<NicSpec>,
    display: Option<String>,
    display_seen: bool,
    tablet: bool,
    usb: Vec<UsbPassthrough>,
    explicit_boot: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q35_UEFI: &str = r#"
<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <name>win11-lab</name>
  <memory unit='GiB'>8</memory>
  <vcpu placement='static'>4</vcpu>
  <os firmware='efi'>
    <type arch='x86_64' machine='pc-q35-8.2'>hvm</type>
    <firmware><feature enabled='yes' name='secure-boot'/></firmware>
    <nvram>/var/lib/libvirt/qemu/nvram/win11-lab_VARS.fd</nvram>
    <boot dev='hd'/>
  </os>
  <features>
    <acpi/><apic/><smm state='on'/>
    <hyperv mode='custom'><relaxed state='on'/><vapic state='on'/><spinlocks state='on' retries='8191'/></hyperv>
  </features>
  <cpu mode='host-passthrough'><topology sockets='1' dies='1' cores='2' threads='2'/></cpu>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' cache='none' discard='unmap'/>
      <source file='/var/lib/libvirt/images/win11-lab.qcow2'/>
      <target dev='sda' bus='sata'/>
      <boot order='1'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='/srv/iso/virtio-win.iso'/>
      <target dev='sdb' bus='sata'/>
      <readonly/>
    </disk>
    <controller type='usb' model='qemu-xhci'/>
    <interface type='network'>
      <mac address='52:54:00:aa:bb:cc'/>
      <source network='default'/>
      <model type='e1000e'/>
    </interface>
    <input type='tablet' bus='usb'/>
    <graphics type='spice' autoport='yes'/>
    <video><model type='qxl'/></video>
    <tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>
    <hostdev mode='subsystem' type='usb'><source><vendor id='0x046d'/><product id='0xc52b'/></source></hostdev>
  </devices>
  <qemu:commandline><qemu:arg value='-rtc'/><qemu:arg value='base=localtime'/></qemu:commandline>
</domain>"#;

    const I440FX_BIOS: &str = r#"
<domain type='qemu'>
  <name>old box</name>
  <memory>1048576</memory>
  <vcpu>2</vcpu>
  <os><type arch='x86_64' machine='pc-i440fx-6.2'>hvm</type><boot dev='cdrom'/><boot dev='hd'/></os>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='raw' cache='writeback'/>
      <source file='/data/vms/old.img'/>
      <target dev='hda' bus='ide'/>
    </disk>
    <disk type='file' device='cdrom'>
      <source file='/data/iso/dos.iso'/>
      <target dev='hdc' bus='ide'/>
    </disk>
    <disk type='block' device='disk'>
      <source dev='/dev/sdz'/>
      <target dev='vdb' bus='virtio'/>
    </disk>
    <interface type='user'>
      <backend type='passt'/>
      <portForward proto='tcp'><range start='2222' to='22'/></portForward>
      <model type='rtl8139'/>
    </interface>
    <sound model='ich9'/>
    <panic model='isa'/>
  </devices>
</domain>"#;

    const AARCH64_VIRT: &str = r#"
<domain type='qemu'>
  <name>arm-dev</name>
  <memory unit='MiB'>2048</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch='aarch64' machine='virt-9.0'>hvm</type>
    <loader readonly='yes' type='pflash'>/usr/share/AAVMF/AAVMF_CODE.fd</loader>
    <nvram>/var/lib/libvirt/qemu/nvram/arm-dev_VARS.fd</nvram>
  </os>
  <cpu mode='custom'><model>cortex-a72</model><feature policy='disable' name='pmu'/></cpu>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='/vm/arm-dev.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <interface type='bridge'><source bridge='br0'/><model type='virtio'/></interface>
    <filesystem type='mount'><source dir='/srv/share'/><target dir='share'/></filesystem>
  </devices>
</domain>"#;

    #[test]
    fn memory_units() {
        assert_eq!(memory_mib("1048576", None), Some(1024));
        assert_eq!(memory_mib("8", Some("GiB")), Some(8192));
        assert_eq!(memory_mib("2048", Some("MiB")), Some(2048));
        assert_eq!(memory_mib("x", None), None);
    }

    #[test]
    fn imports_q35_uefi_secure_boot() {
        let r = import_libvirt(Q35_UEFI).unwrap();
        let s = &r.spec;
        assert_eq!(s.name, "win11-lab");
        assert_eq!(
            (
                s.accel.as_str(),
                s.arch.as_str(),
                s.machine.type_name.as_str()
            ),
            ("kvm", "x86_64", "pc-q35-8.2")
        );
        assert_eq!(s.memory.size_mib, 8192);
        assert_eq!(s.cpu.count, 4);
        assert_eq!(s.cpu.model.as_deref(), Some("host"));
        assert_eq!(
            s.cpu.topology,
            Some(Topology {
                sockets: 1,
                cores: 2,
                threads: 2
            })
        );
        assert_eq!(
            s.cpu.props.get("hv-relaxed").map(String::as_str),
            Some("on")
        );
        assert_eq!(
            s.cpu.props.get("hv-spinlocks").map(String::as_str),
            Some("0x1fff")
        );
        assert_eq!(s.machine.props.get("smm").map(String::as_str), Some("on"));
        assert_eq!(r.firmware_hint, Some(FirmwareHint::UefiSecureBoot));
        assert_eq!(s.disks.len(), 2);
        assert_eq!(
            (s.disks[0].bus, s.disks[0].boot_index),
            (DiskBus::Ide, Some(1))
        );
        assert_eq!(
            (s.disks[1].media, s.disks[1].readonly),
            (Media::Cdrom, true)
        );
        assert_eq!(r.files_to_map[0].format.as_deref(), Some("qcow2"));
        assert_eq!(s.nics[0].model, "e1000e");
        assert_eq!(s.nics[0].mac.as_deref(), Some("52:54:00:aa:bb:cc"));
        assert_eq!(s.display.device.as_deref(), Some("qxl-vga"));
        assert!(s.input.usb_tablet);
        assert_eq!(s.usb[0].product_id, "c52b");
        assert_eq!(s.passthrough, ["-rtc", "base=localtime"]);
        for w in ["NVRAM", "TPM", "libvirt network 'default'", "passthrough"] {
            assert!(
                r.warnings.iter().any(|x| x.contains(w)),
                "missing warning about {w}: {:?}",
                r.warnings
            );
        }
    }

    #[test]
    fn imports_i440fx_bios_with_boot_order_and_host_device() {
        let r = import_libvirt(I440FX_BIOS).unwrap();
        let s = &r.spec;
        assert_eq!(s.name, "old-box");
        assert_eq!(s.accel, "tcg");
        assert_eq!(s.memory.size_mib, 1024);
        assert_eq!(r.firmware_hint, None);
        assert_eq!(s.disks[0].cache, CacheMode::Writeback);
        assert_eq!(s.disks[1].boot_index, Some(1), "<boot dev='cdrom'/> first");
        assert_eq!(s.disks[0].boot_index, Some(2));
        assert_eq!(r.files_to_map[2].usage, FileUse::HostDevice);
        assert_eq!(s.nics[0].model, "rtl8139");
        assert_eq!(s.nics[0].backend.forwards()[0].guest_port, 22);
        assert_eq!(s.display.device.as_deref(), Some("VGA"));
        assert!(r.warnings.iter().any(|w| w.contains("sound")));
        assert_eq!(s.devices.len(), 1, "the ISA panic device");
        assert_eq!(s.devices[0].driver, "pvpanic");
    }

    #[test]
    fn imports_aarch64_virt() {
        let r = import_libvirt(AARCH64_VIRT).unwrap();
        let s = &r.spec;
        assert_eq!(
            (s.arch.as_str(), s.machine.type_name.as_str()),
            ("aarch64", "virt-9.0")
        );
        assert_eq!(r.firmware_hint, Some(FirmwareHint::Uefi));
        assert_eq!(s.cpu.model.as_deref(), Some("cortex-a72"));
        assert_eq!(s.cpu.props.get("pmu").map(String::as_str), Some("off"));
        assert_eq!(s.disks[0].bus, DiskBus::Virtio);
        assert_eq!(s.display.device, None);
        assert!(r.warnings.iter().any(|w| w.contains("br0")));
        assert!(r.warnings.iter().any(|w| w.contains("folder share")));
    }

    #[test]
    fn rejects_other_documents() {
        assert!(
            import_libvirt("<network/>")
                .unwrap_err()
                .contains("<domain>")
        );
        assert!(import_libvirt("not xml").is_err());
    }
}
