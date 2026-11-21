//! Export a VM spec as a libvirt domain XML document (M4-07).
//!
//! The result is meant for `virsh define`. It is lossy where libvirt and
//! qemu-ui model things differently; every loss is listed in
//! [`LibvirtExport::lossy`] and repeated as a comment at the top of the XML.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::spec::{DiskBus, Media, NicBackend, VmSpec, is_host_disk};

/// Host-side facts the daemon resolves for the export.
#[derive(Debug, Clone, Default)]
pub struct ExportContext {
    /// Per `spec.disks` entry: the image's host path and format, when known.
    pub disks: Vec<Option<(String, String)>>,
    /// The VM's UEFI firmware, when it uses one.
    pub uefi: Option<ExportUefi>,
    /// Network object id → name, for NICs on qemu-ui networks.
    pub networks: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExportUefi {
    pub secure_boot: bool,
}

#[derive(Debug, Clone)]
pub struct LibvirtExport {
    pub xml: String,
    /// What the XML does not carry over, in plain words.
    pub lossy: Vec<String>,
}

/// Escape text for XML attribute values and character data.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// libvirt's `<video><model type=…>` for a qemu-ui display device.
fn video_model(device: Option<&str>) -> &'static str {
    match device {
        None => "none",
        Some(
            "virtio-vga" | "virtio-gpu-pci" | "virtio-gpu-device" | "virtio-vga-gl"
            | "virtio-gpu-gl-pci",
        ) => "virtio",
        Some("qxl-vga" | "qxl") => "qxl",
        Some("cirrus-vga") => "cirrus",
        Some("bochs-display") => "bochs",
        Some("ramfb") => "ramfb",
        Some("vmware-svga") => "vmvga",
        Some(_) => "vga",
    }
}

/// libvirt's `<model type=…>` for a NIC device.
fn nic_model(model: &str) -> &str {
    match model {
        "virtio-net-pci" | "virtio-net-device" | "virtio-net-ccw" => "virtio",
        other => other,
    }
}

pub fn export_libvirt(spec: &VmSpec, ctx: &ExportContext) -> LibvirtExport {
    let mut lossy = vec![];
    let x86 = matches!(spec.arch.as_str(), "x86_64" | "i386");
    let q35 = spec.machine.type_name.contains("q35");
    let dom_type = match spec.accel.as_str() {
        "kvm" => "kvm",
        "tcg" | "auto" => "qemu",
        other => {
            lossy.push(format!(
                "accelerator '{other}' has no libvirt domain type here; exported as type='qemu'"
            ));
            "qemu"
        }
    };
    if spec.accel == "auto" {
        lossy.push(
            "accelerator 'auto' exported as type='qemu' (use 'kvm' for hardware acceleration)"
                .into(),
        );
    }
    let mut x = String::new();
    let qemu_ns = !spec.passthrough.is_empty()
        || spec.devices.iter().any(|d| {
            !matches!(
                d.driver.as_str(),
                "virtio-rng-pci"
                    | "virtio-rng-device"
                    | "virtio-rng-ccw"
                    | "i6300esb"
                    | "ib700"
                    | "diag288"
                    | "pvpanic"
                    | "pvpanic-pci"
            ) || (d.driver.starts_with("virtio-rng") && !d.props.is_empty())
        });
    let _ = write!(x, "<domain type='{dom_type}'");
    if qemu_ns {
        x.push_str(" xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'");
    }
    x.push_str(">\n");
    let _ = writeln!(x, "  <name>{}</name>", esc(&spec.name));
    let _ = writeln!(x, "  <memory unit='MiB'>{}</memory>", spec.memory.size_mib);
    let _ = writeln!(x, "  <vcpu>{}</vcpu>", spec.cpu.count);

    // <os>
    let arch = if spec.arch == "i386" {
        "i686"
    } else {
        spec.arch.as_str()
    };
    match &ctx.uefi {
        Some(_) => x.push_str("  <os firmware='efi'>\n"),
        None => x.push_str("  <os>\n"),
    }
    let _ = writeln!(
        x,
        "    <type arch='{}' machine='{}'>hvm</type>",
        esc(arch),
        esc(&spec.machine.type_name)
    );
    if let Some(u) = &ctx.uefi {
        let _ = writeln!(
            x,
            "    <firmware><feature enabled='{}' name='secure-boot'/></firmware>",
            if u.secure_boot { "yes" } else { "no" }
        );
    }
    x.push_str("  </os>\n");

    // <features>
    let mut feats = String::new();
    if x86 {
        feats.push_str("    <acpi/>\n    <apic/>\n");
    }
    if spec.machine.props.get("smm").map(String::as_str) == Some("on") {
        feats.push_str("    <smm state='on'/>\n");
    }
    let mut hyperv = String::new();
    let mut kvm_hidden = false;
    let mut cpu_feats = String::new();
    for (k, v) in &spec.cpu.props {
        let on = matches!(v.as_str(), "on" | "yes" | "true");
        if let Some(h) = k.strip_prefix("hv-") {
            match h {
                "spinlocks" => {
                    let retries =
                        u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(8191);
                    let _ = writeln!(hyperv, "      <spinlocks state='on' retries='{retries}'/>");
                }
                "vendor-id" => {
                    let _ = writeln!(hyperv, "      <vendor_id state='on' value='{}'/>", esc(v));
                }
                _ if on => {
                    let _ = writeln!(hyperv, "      <{} state='on'/>", esc(h));
                }
                _ => {}
            }
        } else if k == "kvm" && v == "off" {
            kvm_hidden = true;
        } else if on || v == "off" {
            let _ = writeln!(
                cpu_feats,
                "    <feature policy='{}' name='{}'/>",
                if on { "require" } else { "disable" },
                esc(k)
            );
        } else {
            lossy.push(format!("CPU property {k}={v} has no libvirt equivalent"));
        }
    }
    if !hyperv.is_empty() {
        let _ = write!(feats, "    <hyperv mode='custom'>\n{hyperv}    </hyperv>\n");
    }
    if kvm_hidden {
        feats.push_str("    <kvm><hidden state='on'/></kvm>\n");
    }
    for k in spec.machine.props.keys().filter(|k| k.as_str() != "smm") {
        lossy.push(format!("machine property '{k}' is not exported"));
    }
    if !feats.is_empty() {
        let _ = write!(x, "  <features>\n{feats}  </features>\n");
    }

    // <cpu>
    let (mode, model) = match spec.cpu.model.as_deref() {
        Some("host") => ("host-passthrough", None),
        Some("max") => ("maximum", None),
        Some(m) => ("custom", Some(m)),
        None => ("", None),
    };
    let topo = spec.cpu.topology.map(|t| {
        format!(
            "    <topology sockets='{}' dies='1' cores='{}' threads='{}'/>\n",
            t.sockets, t.cores, t.threads
        )
    });
    if !mode.is_empty() || topo.is_some() || !cpu_feats.is_empty() {
        if mode.is_empty() {
            x.push_str("  <cpu>\n");
        } else {
            let _ = writeln!(x, "  <cpu mode='{mode}'>");
        }
        if let Some(m) = model {
            let _ = writeln!(x, "    <model fallback='forbid'>{}</model>", esc(m));
        }
        if let Some(t) = topo {
            x.push_str(&t);
        }
        x.push_str(&cpu_feats);
        x.push_str("  </cpu>\n");
    }

    // <devices>
    x.push_str("  <devices>\n");
    let mut letters: BTreeMap<&str, u8> = BTreeMap::new();
    for (i, d) in spec.disks.iter().enumerate() {
        if is_host_disk(&d.image) {
            lossy.push(format!(
                "disk {i} is a host disk attached through the qemu-ui helper; add it as a <disk type='block'> by hand"
            ));
            continue;
        }
        let Some((path, format)) = ctx.disks.get(i).cloned().flatten() else {
            lossy.push(format!(
                "disk {i} ({}/{}) has no known host path and was left out",
                d.image.pool, d.image.name
            ));
            continue;
        };
        let (bus, prefix) = match d.bus {
            DiskBus::Virtio => ("virtio", "vd"),
            DiskBus::Scsi => ("scsi", "sd"),
            DiskBus::Usb => ("usb", "sd"),
            DiskBus::Ide if q35 => ("sata", "sd"),
            DiskBus::Ide => ("ide", "hd"),
            DiskBus::Nvme => {
                lossy.push(format!("disk {i} is NVMe; exported on virtio"));
                ("virtio", "vd")
            }
        };
        let n = letters.entry(prefix).or_insert(0);
        let dev = format!("{prefix}{}", (b'a' + *n) as char);
        *n += 1;
        let device = if d.media == Media::Cdrom {
            "cdrom"
        } else {
            "disk"
        };
        let _ = writeln!(x, "    <disk type='file' device='{device}'>");
        let cache = match d.cache {
            crate::spec::CacheMode::None => "none",
            crate::spec::CacheMode::Writeback => "writeback",
            crate::spec::CacheMode::Writethrough => "writethrough",
            crate::spec::CacheMode::Directsync => "directsync",
            crate::spec::CacheMode::Unsafe => "unsafe",
        };
        let mut drv = format!(
            "<driver name='qemu' type='{}' cache='{cache}'",
            esc(&format)
        );
        if d.media == Media::Disk {
            drv.push_str(if d.discard {
                " discard='unmap'"
            } else {
                " discard='ignore'"
            });
        }
        if let Some(io) = &d.aio {
            let _ = write!(drv, " io='{}'", esc(io));
        }
        let _ = writeln!(x, "      {drv}/>");
        let _ = writeln!(x, "      <source file='{}'/>", esc(&path));
        let _ = writeln!(x, "      <target dev='{dev}' bus='{bus}'/>");
        if d.readonly {
            x.push_str("      <readonly/>\n");
        }
        if let Some(b) = d.boot_index {
            // libvirt boot orders start at 1.
            let _ = writeln!(x, "      <boot order='{}'/>", b.max(1));
        }
        x.push_str("    </disk>\n");
    }
    for (i, nic) in spec.nics.iter().enumerate() {
        match &nic.backend {
            NicBackend::User { forwards } => {
                x.push_str("    <interface type='user'>\n");
                if !forwards.is_empty() {
                    x.push_str("      <backend type='passt'/>\n");
                    lossy.push(format!(
                        "NIC {i}'s port forwards are exported as passt <portForward> (libvirt 9.0 or newer)"
                    ));
                    for f in forwards {
                        let _ = writeln!(
                            x,
                            "      <portForward proto='{}' address='{}'><range start='{}' to='{}'/></portForward>",
                            esc(&f.proto),
                            esc(&f.host_addr),
                            f.host_port,
                            f.guest_port
                        );
                    }
                }
            }
            NicBackend::Network { network, forwards } => {
                let name = ctx
                    .networks
                    .get(network)
                    .cloned()
                    .unwrap_or_else(|| network.clone());
                lossy.push(format!(
                    "NIC {i} is on qemu-ui network '{name}'; exported as libvirt network '{name}', which must exist"
                ));
                if !forwards.is_empty() {
                    lossy.push(format!("NIC {i}'s port forwards are not exported"));
                }
                let _ = writeln!(
                    x,
                    "    <interface type='network'>\n      <source network='{}'/>",
                    esc(&name)
                );
            }
        }
        if let Some(mac) = &nic.mac {
            let _ = writeln!(x, "      <mac address='{}'/>", esc(mac));
        }
        let _ = writeln!(x, "      <model type='{}'/>", esc(nic_model(&nic.model)));
        if nic.queues.is_some() || nic.virtio.is_some() {
            lossy.push(format!(
                "NIC {i}'s queue and offload settings are not exported"
            ));
        }
        x.push_str("    </interface>\n");
    }
    if spec.input.usb_tablet {
        x.push_str("    <input type='tablet' bus='usb'/>\n");
    }
    if spec.display.vnc {
        x.push_str("    <graphics type='vnc' autoport='yes' listen='127.0.0.1'/>\n");
    }
    let _ = writeln!(
        x,
        "    <video><model type='{}'/></video>",
        video_model(spec.display.device.as_deref())
    );
    if spec.display.vdagent {
        lossy.push("the clipboard agent (qemu-vdagent) is not exported".into());
    }
    for u in &spec.usb {
        let _ = writeln!(
            x,
            "    <hostdev mode='subsystem' type='usb'><source><vendor id='0x{}'/><product id='0x{}'/></source></hostdev>",
            esc(&u.vendor_id),
            esc(&u.product_id)
        );
    }
    // Generic devices: libvirt elements where one exists, else -device args.
    let mut extra_args: Vec<String> = vec![];
    for (i, d) in spec.devices.iter().enumerate() {
        match d.driver.as_str() {
            "virtio-rng-pci" | "virtio-rng-device" | "virtio-rng-ccw" if d.props.is_empty() => {
                x.push_str("    <rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>\n")
            }
            m @ ("i6300esb" | "ib700" | "diag288") => {
                let _ = writeln!(x, "    <watchdog model='{m}' action='reset'/>");
            }
            "pvpanic" => x.push_str("    <panic model='isa'/>\n"),
            "pvpanic-pci" => x.push_str("    <panic model='pci'/>\n"),
            _ => {
                let mut arg = d.driver.replace(',', ",,");
                for (k, v) in &d.props {
                    let v = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Bool(b) => if *b { "on" } else { "off" }.to_string(),
                        other => other.to_string(),
                    };
                    let _ = write!(arg, ",{k}={}", v.replace(',', ",,"));
                }
                lossy.push(format!(
                    "device {i} ({}) has no libvirt element; exported as a <qemu:arg> -device",
                    d.driver
                ));
                extra_args.push("-device".into());
                extra_args.push(arg);
            }
        }
    }
    for s in &spec.shares {
        lossy.push(format!(
            "folder share '{}' is not exported; add a <filesystem> pointing at the folder",
            s.tag
        ));
    }
    x.push_str("  </devices>\n");
    if qemu_ns {
        x.push_str("  <qemu:commandline>\n");
        for a in extra_args.iter().chain(&spec.passthrough) {
            let _ = writeln!(x, "    <qemu:arg value='{}'/>", esc(a));
        }
        x.push_str("  </qemu:commandline>\n");
    }
    x.push_str("</domain>\n");

    let mut head = String::from("<!-- Exported from qemu-ui.");
    if lossy.is_empty() {
        head.push_str(" -->\n");
    } else {
        head.push_str(" Not carried over:\n");
        for l in &lossy {
            let _ = writeln!(head, "  - {}", l.replace("--", "- -"));
        }
        head.push_str("-->\n");
    }
    LibvirtExport {
        xml: head + &x,
        lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::FirmwareHint;
    use crate::import_libvirt::import_libvirt;
    use crate::spec::*;

    fn spec() -> VmSpec {
        let mut props = BTreeMap::new();
        props.insert("smm".to_string(), "on".to_string());
        let mut cpu_props = BTreeMap::new();
        cpu_props.insert("hv-relaxed".to_string(), "on".to_string());
        cpu_props.insert("invtsc".to_string(), "on".to_string());
        VmSpec {
            limits: Default::default(),
            spec_version: SPEC_VERSION,
            name: "round-trip".into(),
            arch: "x86_64".into(),
            machine: MachineSpec {
                type_name: "pc-q35-11.1".into(),
                props,
            },
            accel: "kvm".into(),
            cpu: CpuSpec {
                model: Some("host".into()),
                props: cpu_props,
                count: 4,
                topology: Some(Topology {
                    sockets: 1,
                    cores: 2,
                    threads: 2,
                }),
            },
            memory: MemorySpec { size_mib: 3072 },
            firmware: FirmwareSpec::Descriptor {
                id: "50-edk2-x86_64-secure".into(),
            },
            disks: vec![
                DiskSpec {
                    image: ImageRef {
                        pool: "default".into(),
                        name: "rt.qcow2".into(),
                    },
                    bus: DiskBus::Virtio,
                    media: Media::Disk,
                    readonly: false,
                    boot_index: Some(1),
                    cache: CacheMode::None,
                    aio: None,
                    discard: true,
                    slot: None,
                },
                DiskSpec {
                    image: ImageRef {
                        pool: "isos".into(),
                        name: "install.iso".into(),
                    },
                    bus: DiskBus::Ide,
                    media: Media::Cdrom,
                    readonly: true,
                    boot_index: Some(2),
                    cache: CacheMode::None,
                    aio: None,
                    discard: true,
                    slot: None,
                },
            ],
            nics: vec![NicSpec {
                backend: NicBackend::User {
                    forwards: vec![PortForward {
                        proto: "tcp".into(),
                        host_addr: "127.0.0.1".into(),
                        host_port: 2222,
                        guest_port: 22,
                    }],
                },
                model: "virtio-net-pci".into(),
                mac: Some("52:54:00:01:02:03".into()),
                queues: None,
                virtio: None,
            }],
            display: DisplaySpec {
                device: Some("virtio-vga".into()),
                vnc: true,
                vdagent: false,
                spice: false,
            },
            input: InputSpec { usb_tablet: true },
            shares: vec![],
            usb: vec![UsbPassthrough {
                vendor_id: "046d".into(),
                product_id: "c52b".into(),
                host_bus: None,
                host_port: None,
                label: None,
            }],
            devices: vec![
                crate::devices::DeviceSpec {
                    driver: "virtio-rng-pci".into(),
                    bus: None,
                    props: Default::default(),
                },
                crate::devices::DeviceSpec {
                    driver: "i6300esb".into(),
                    bus: None,
                    props: Default::default(),
                },
            ],
            passthrough: vec!["-rtc".into(), "base=utc".into()],
        }
    }

    fn ctx() -> ExportContext {
        ExportContext {
            disks: vec![
                Some(("/pools/default/rt.qcow2".into(), "qcow2".into())),
                Some(("/pools/isos/install & setup.iso".into(), "raw".into())),
            ],
            uefi: Some(ExportUefi { secure_boot: true }),
            networks: BTreeMap::new(),
        }
    }

    #[test]
    fn exports_well_formed_xml() {
        let e = export_libvirt(&spec(), &ctx());
        roxmltree::Document::parse(&e.xml).expect("well-formed");
        assert!(e.xml.contains("<os firmware='efi'>"));
        assert!(e.xml.contains("<target dev='vda' bus='virtio'/>"));
        assert!(
            e.xml.contains("<target dev='sda' bus='sata'/>"),
            "IDE on q35 is SATA"
        );
        assert!(e.xml.contains("install &amp; setup.iso"));
        assert!(e.xml.contains("<qemu:arg value='-rtc'/>"));
        assert!(e.lossy.iter().any(|l| l.contains("portForward")));
    }

    #[test]
    fn round_trips_what_libvirt_can_express() {
        let s = spec();
        let e = export_libvirt(&s, &ctx());
        let r = import_libvirt(&e.xml).unwrap();
        let t = &r.spec;
        assert_eq!(t.name, s.name);
        assert_eq!(
            (
                t.arch.as_str(),
                t.machine.type_name.as_str(),
                t.accel.as_str()
            ),
            ("x86_64", "pc-q35-11.1", "kvm")
        );
        assert_eq!(t.machine.props, s.machine.props);
        assert_eq!(t.cpu, s.cpu);
        assert_eq!(t.memory, s.memory);
        assert_eq!(r.firmware_hint, Some(FirmwareHint::UefiSecureBoot));
        assert_eq!(t.disks.len(), 2);
        for (a, b) in t.disks.iter().zip(&s.disks) {
            assert_eq!(
                (a.bus, a.media, a.readonly, a.boot_index, a.cache, a.discard),
                (b.bus, b.media, b.readonly, b.boot_index, b.cache, b.discard)
            );
        }
        assert_eq!(r.files_to_map[0].path, "/pools/default/rt.qcow2");
        assert_eq!(r.files_to_map[1].path, "/pools/isos/install & setup.iso");
        assert_eq!(t.nics, s.nics);
        assert_eq!(t.display, s.display);
        assert_eq!(t.input, s.input);
        assert_eq!(t.usb, s.usb);
        assert_eq!(t.devices, s.devices);
        assert_eq!(t.passthrough, s.passthrough);
    }

    #[test]
    fn notes_what_it_cannot_export() {
        let mut s = spec();
        s.disks[0].image.pool = HOST_DISK_POOL.into();
        s.nics[0].backend = NicBackend::Network {
            network: "n1".into(),
            forwards: vec![],
        };
        s.accel = "whpx".into();
        let mut c = ctx();
        c.networks.insert("n1".into(), "lab".into());
        let e = export_libvirt(&s, &c);
        roxmltree::Document::parse(&e.xml).expect("well-formed");
        assert!(e.xml.contains("<source network='lab'/>"));
        assert!(e.lossy.iter().any(|l| l.contains("host disk")));
        assert!(e.lossy.iter().any(|l| l.contains("whpx")));
        assert!(
            e.xml
                .starts_with("<!-- Exported from qemu-ui. Not carried over:")
        );
    }
}
