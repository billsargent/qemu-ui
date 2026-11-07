//! Golden tests for the spec → argv compiler (X-01 seed corpus).
//!
//! Each case compiles a spec against a fixed capability snapshot and compares
//! the argv, one token per line, with `tests/golden/<case>.argv`.
//! Regenerate after an intentional change with:
//!
//!     UPDATE_GOLDEN=1 cargo test -p qemu-ui-core --test compile_golden

use std::collections::BTreeMap;
use std::path::PathBuf;

use qemu_ui_core::caps::*;
use qemu_ui_core::compile::*;
use qemu_ui_core::spec::*;

fn machine(name: &str, alias: Option<&str>, default: bool) -> MachineInfo {
    MachineInfo {
        name: name.into(),
        description: String::new(),
        alias: alias.map(Into::into),
        is_default: default,
        deprecated: false,
        cpu_max: Some(288),
        default_cpu_type: None,
        hotpluggable_cpus: Some(true),
        numa_mem_supported: Some(false),
    }
}

fn caps(monitor_objects: bool) -> BinaryCaps {
    let devices = [
        "virtio-blk-pci",
        "virtio-scsi-pci",
        "scsi-hd",
        "scsi-cd",
        "nvme",
        "ide-hd",
        "ide-cd",
        "usb-storage",
        "qemu-xhci",
        "usb-tablet",
        "virtio-net-pci",
        "e1000e",
        "virtio-vga",
        "VGA",
        "virtio-serial-pci",
        "virtserialport",
        "virtio-9p-pci",
        "vhost-user-fs-pci",
    ]
    .into_iter()
    .map(|n| DeviceInfo {
        name: n.into(),
        category: String::new(),
        bus: None,
        alias: None,
        description: None,
        no_user: false,
    })
    .collect();
    let mut objects = vec!["secret".to_string(), "memory-backend-memfd".to_string()];
    if monitor_objects {
        objects.push("monitor-qmp".into());
    }
    BinaryCaps {
        binary: Binary {
            id: "0123456789abcdef".into(),
            path: "/usr/bin/qemu-system-x86_64".into(),
            arch: Some("x86_64".into()),
            version: "11.1.0".into(),
            sha256: String::new(),
            source: "path".into(),
        },
        arch: "x86_64".into(),
        qemu_version: QemuVersion {
            major: 11,
            minor: 1,
            micro: 0,
            package: String::new(),
        },
        probe_version: PROBE_VERSION,
        probed_at: 0,
        native: true,
        machines: vec![
            machine("pc-q35-11.1", Some("q35"), false),
            machine("pc-i440fx-11.1", Some("pc"), true),
        ],
        cpus: ["qemu64", "max", "Skylake-Client"]
            .into_iter()
            .map(|n| CpuModel {
                name: n.into(),
                deprecated: false,
                unavailable_features: vec![],
            })
            .collect(),
        devices,
        accels: vec![
            AccelInfo {
                name: "tcg".into(),
                available: true,
                hardware: false,
                reason: None,
                remediation: None,
            },
            AccelInfo {
                name: "kvm".into(),
                available: true,
                hardware: true,
                reason: None,
                remediation: None,
            },
        ],
        backends: Backends {
            netdev: Some(vec!["user".into()]),
            chardev: Some(vec!["socket".into(), "qemu-vdagent".into()]),
            display: Some(vec!["none".into()]),
            audiodev: None,
            tpmdev: None,
            object: Some(objects),
        },
        qmp_schema_hash: String::new(),
        warnings: vec![],
    }
}

fn run_paths(vnc: VncListen) -> RunPaths {
    RunPaths {
        qmp: "/run/qui/vm1/qmp.sock".into(),
        qmp_user: "/run/qui/vm1/qmp-user.sock".into(),
        serial: "/run/qui/vm1/serial.sock".into(),
        serial_log: "/var/lib/qemu-ui/vms/vm1/serial.log".into(),
        pidfile: "/run/qui/vm1/qemu.pid".into(),
        vnc,
    }
}

fn base_spec() -> VmSpec {
    VmSpec {
        spec_version: SPEC_VERSION,
        name: "debian".into(),
        arch: "x86_64".into(),
        machine: MachineSpec {
            type_name: "pc-q35-11.1".into(),
            props: BTreeMap::new(),
        },
        accel: "auto".into(),
        cpu: CpuSpec {
            model: Some("host".into()),
            props: BTreeMap::new(),
            count: 4,
            topology: None,
        },
        memory: MemorySpec { size_mib: 4096 },
        firmware: FirmwareSpec::Default,
        disks: vec![DiskSpec {
            image: ImageRef {
                pool: "default".into(),
                name: "debian.qcow2".into(),
            },
            bus: DiskBus::Virtio,
            media: Media::Disk,
            readonly: false,
            boot_index: Some(1),
            cache: CacheMode::None,
            aio: None,
            discard: true,
        }],
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
            mac: Some("52:54:00:12:34:56".into()),
        }],
        display: DisplaySpec {
            device: Some("virtio-vga".into()),
            vnc: true,
            vdagent: false,
        },
        input: InputSpec { usb_tablet: true },
        shares: vec![],
        passthrough: vec![],
    }
}

fn images() -> BTreeMap<ImageRef, ResolvedImage> {
    let mut m = BTreeMap::new();
    m.insert(
        ImageRef {
            pool: "default".into(),
            name: "debian.qcow2".into(),
        },
        ResolvedImage {
            path: "/var/lib/qemu-ui/pools/default/debian.qcow2".into(),
            format: "qcow2".into(),
        },
    );
    m.insert(
        ImageRef {
            pool: "default".into(),
            name: "install, disc 1.iso".into(),
        },
        ResolvedImage {
            path: "/var/lib/qemu-ui/pools/default/install, disc 1.iso".into(),
            format: "raw".into(),
        },
    );
    m
}

fn golden(case: &str, compiled: &Compiled) {
    let actual = std::iter::once(compiled.program.as_str())
        .chain(compiled.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{case}.argv"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            panic!(
                "missing golden file {}; run with UPDATE_GOLDEN=1",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    assert_eq!(
        actual, expected,
        "golden mismatch for {case}; run with UPDATE_GOLDEN=1 if intended"
    );
}

#[test]
fn linux_q35_virtio() {
    let caps = caps(true);
    let images = images();
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/run/qui/vm1/vnc.sock".into())),
        images: &images,
        firmware: None,
    };
    let c = compile(&base_spec(), &ctx).unwrap();
    assert_eq!(c.accel, "kvm");
    golden("linux-q35-virtio", &c);
}

#[test]
fn windows_uefi_secure_boot_cdrom_with_comma() {
    let caps = caps(true);
    let images = images();
    let mut spec = base_spec();
    spec.cpu.model = Some("max".into());
    spec.cpu.topology = Some(Topology {
        sockets: 1,
        cores: 2,
        threads: 2,
    });
    spec.firmware = FirmwareSpec::Descriptor {
        id: "50-edk2-x86_64-secure".into(),
    };
    spec.disks.push(DiskSpec {
        image: ImageRef {
            pool: "default".into(),
            name: "install, disc 1.iso".into(),
        },
        bus: DiskBus::Ide,
        media: Media::Cdrom,
        readonly: true,
        boot_index: Some(0),
        cache: CacheMode::Writeback,
        aio: None,
        discard: false,
    });
    spec.display.vdagent = true;
    spec.passthrough = vec!["-global".into(), "ICH9-LPC.disable_s3=1".into()];
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "windows",
        run: RunPaths {
            qmp: r"C:\qui\r\vm1\qmp.sock".into(),
            qmp_user: r"C:\qui\r\vm1\qmp-user.sock".into(),
            serial: r"C:\qui\r\vm1\serial.sock".into(),
            serial_log: r"C:\qui\vms\vm1\serial.log".into(),
            pidfile: r"C:\qui\r\vm1\qemu.pid".into(),
            vnc: VncListen::Tcp { display: 7 },
        },
        images: &images,
        firmware: Some(ResolvedFirmware {
            code: r"C:\qemu\share\edk2-x86_64-secure-code.fd".into(),
            code_format: "raw".into(),
            vars: Some(r"C:\qui\vms\vm1\VARS.qcow2".into()),
            vars_format: "qcow2".into(),
            secure_boot: true,
            requires_smm: true,
        }),
    };
    let c = compile(&spec, &ctx).unwrap();
    // Direct I/O is unavailable on Windows hosts: buffered fallback + warning.
    assert!(
        c.warnings
            .iter()
            .any(|w| w.rule == "disk.no-direct-io" && w.field == "disks[0].cache")
    );
    assert!(!c.args.iter().any(|a| a.contains(r#""direct":true"#)));
    // The comma in the ISO name lives inside JSON, never in keyval syntax.
    assert!(
        c.args
            .iter()
            .any(|a| a
                .contains(r#""filename":"/var/lib/qemu-ui/pools/default/install, disc 1.iso""#))
    );
    golden("windows-uefi-sb-cdrom", &c);
}

#[test]
fn old_qemu_uses_mon() {
    let caps = caps(false);
    let images = images();
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/run/qui/vm1/vnc.sock".into())),
        images: &images,
        firmware: None,
    };
    let c = compile(&base_spec(), &ctx).unwrap();
    assert!(
        c.args
            .windows(2)
            .any(|w| w[0] == "-mon" && w[1] == "chardev=qui-qmp,mode=control")
    );
    assert!(!c.args.iter().any(|a| a.starts_with("monitor-qmp")));
}

#[test]
fn keyval_paths_double_commas() {
    let caps = caps(true);
    let images = images();
    let mut run = run_paths(VncListen::Unix("/run/a,b/vnc.sock".into()));
    run.serial = "/run/a,b/serial.sock".into();
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "linux",
        run,
        images: &images,
        firmware: None,
    };
    let c = compile(&base_spec(), &ctx).unwrap();
    assert!(c.args.contains(&"unix:/run/a,,b/vnc.sock".to_string()));
    assert!(
        c.args
            .iter()
            .any(|a| a.starts_with("socket,id=qui-ser0,path=/run/a,,b/serial.sock,"))
    );
}

fn errors(spec: &VmSpec) -> Vec<String> {
    let caps = caps(true);
    let images = images();
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/x".into())),
        images: &images,
        firmware: None,
    };
    match compile(spec, &ctx) {
        Ok(_) => vec![],
        Err(d) => d
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.rule)
            .collect(),
    }
}

#[test]
fn validation_errors_are_pinned_to_rules() {
    let mut s = base_spec();
    s.machine.type_name = "q35".into();
    assert_eq!(errors(&s), ["machine.unpinned-alias"]);

    let mut s = base_spec();
    s.cpu.topology = Some(Topology {
        sockets: 2,
        cores: 2,
        threads: 2,
    });
    assert_eq!(errors(&s), ["cpu.topology-mismatch"]);

    let mut s = base_spec();
    s.accel = "tcg".into();
    assert_eq!(errors(&s), ["cpu.host-needs-accel"]);

    let mut s = base_spec();
    s.disks[0].media = Media::Cdrom;
    assert_eq!(errors(&s), ["disk.bus-media"]);

    let mut s = base_spec();
    s.disks[0].image.name = "missing.qcow2".into();
    assert_eq!(errors(&s), ["disk.image-missing"]);

    let mut s = base_spec();
    s.passthrough = vec![
        "-machine".into(),
        "pc".into(),
        "-device".into(),
        "e1000,id=qui-x".into(),
    ];
    assert_eq!(
        errors(&s),
        ["passthrough.owned-option", "passthrough.reserved-id"]
    );

    let mut s = base_spec();
    s.passthrough = vec!["-daemonize".into()];
    assert_eq!(errors(&s), ["passthrough.owned-option"]);

    let mut s = base_spec();
    s.nics[0].mac = Some("01:00:00:00:00:00".into());
    assert_eq!(errors(&s), ["nic.mac-invalid"]);
}

#[test]
fn exposed_forward_is_a_warning_not_an_error() {
    let caps = caps(true);
    let images = images();
    let mut s = base_spec();
    let NicBackend::User { forwards } = &mut s.nics[0].backend;
    forwards[0].host_addr = "0.0.0.0".into();
    let ctx = Context {
        shares: vec![],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/x".into())),
        images: &images,
        firmware: None,
    };
    let c = compile(&s, &ctx).unwrap();
    assert!(c.warnings.iter().any(|w| w.rule == "forward.exposed"));
}

#[test]
fn pins_aliases() {
    let caps = caps(true);
    assert_eq!(pin_machine("q35", &caps).unwrap(), "pc-q35-11.1");
    assert_eq!(pin_machine("pc-q35-11.1", &caps).unwrap(), "pc-q35-11.1");
    assert!(pin_machine("nope", &caps).is_err());
}

#[test]
fn spec_roundtrips_through_json_and_migrate() {
    let s = base_spec();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(migrate(v).unwrap(), s);
}

fn share(root: &str, path: &str, tag: &str, readonly: bool) -> ShareSpec {
    ShareSpec {
        root: root.into(),
        path: path.into(),
        tag: tag.into(),
        readonly,
    }
}

#[test]
fn linux_shares_9p() {
    let caps = caps(true);
    let images = images();
    let mut spec = base_spec();
    spec.shares = vec![
        share("src", "projects/web", "src", false),
        share("docs", "", "docs", true),
    ];
    let ctx = Context {
        shares: vec![
            ResolvedShare {
                path: "/srv/src/projects/web, v2".into(),
                backend: ShareBackend::NineP,
            },
            ResolvedShare {
                path: "/srv/docs".into(),
                backend: ShareBackend::NineP,
            },
        ],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/run/qui/vm1/vnc.sock".into())),
        images: &images,
        firmware: None,
    };
    let c = compile(&spec, &ctx).unwrap();
    // The comma in the folder is doubled in keyval syntax.
    assert!(c.args.iter().any(
        |a| a == "local,id=qui-fs0,path=/srv/src/projects/web,, v2,security_model=mapped-xattr"
    ));
    assert!(c.args.iter().any(|a| a == "local,id=qui-fs1,path=/srv/docs,security_model=mapped-xattr,readonly=on"));
    assert!(
        !c.args.iter().any(|a| a.contains("memory-backend-memfd")),
        "9p needs no shared memory"
    );
    golden("linux-shares-9p", &c);
}

#[test]
fn linux_shares_virtiofs() {
    let caps = caps(true);
    let images = images();
    let mut spec = base_spec();
    spec.shares = vec![share("src", "", "src", false)];
    let ctx = Context {
        shares: vec![ResolvedShare {
            path: "/srv/src".into(),
            backend: ShareBackend::VirtioFs {
                socket: "/run/qui/vm1/vfs0.sock".into(),
            },
        }],
        caps: &caps,
        os: "linux",
        run: run_paths(VncListen::Unix("/run/qui/vm1/vnc.sock".into())),
        images: &images,
        firmware: None,
    };
    let c = compile(&spec, &ctx).unwrap();
    assert!(
        c.args
            .iter()
            .any(|a| a == "memory-backend-memfd,id=qui-mem,size=4096M,share=on")
    );
    assert!(
        c.args
            .windows(2)
            .any(|w| w[0] == "-numa" && w[1] == "node,memdev=qui-mem")
    );
    golden("linux-shares-virtiofs", &c);
}

#[test]
fn shares_on_windows_shaped_qemu_are_refused_with_the_reason() {
    // The Windows build has no virtio-9p device and no fsdev support.
    let mut caps = caps(true);
    caps.devices
        .retain(|d| d.name != "virtio-9p-pci" && d.name != "vhost-user-fs-pci");
    let mut spec = base_spec();
    spec.shares = vec![
        share("src", "", "src", false),
        share("docs", "", "docs", true),
    ];
    let d = validate(&spec, &caps);
    let unsupported: Vec<_> = d.iter().filter(|x| x.rule == "share.unsupported").collect();
    assert_eq!(unsupported.len(), 2);
    assert_eq!(unsupported[0].field, "shares[0]");
    assert!(
        unsupported[0].message.contains("Windows"),
        "{}",
        unsupported[0].message
    );
    assert_eq!(unsupported[0].severity, Severity::Error);
}

#[test]
fn share_validation_rules() {
    let caps = caps(true);
    let mut spec = base_spec();
    spec.shares = vec![
        share("src", "../etc", "data", false),
        share("src", "", "data", false),
        share("", "", "x y", false),
    ];
    let rules: Vec<(String, String)> = validate(&spec, &caps)
        .into_iter()
        .filter(|d| d.rule.starts_with("share."))
        .map(|d| (d.field, d.rule))
        .collect();
    assert_eq!(
        rules,
        [
            (
                "shares[0].path".to_string(),
                "share.path-invalid".to_string()
            ),
            (
                "shares[1].tag".to_string(),
                "share.tag-duplicate".to_string()
            ),
            (
                "shares[2].root".to_string(),
                "share.root-invalid".to_string()
            ),
            ("shares[2].tag".to_string(), "share.tag-invalid".to_string()),
        ]
    );
    // The daemon must resolve every share before compiling.
    let mut spec = base_spec();
    spec.shares = vec![share("src", "", "src", false)];
    assert_eq!(errors(&spec), ["share.unresolved"]);
}

#[test]
fn display_command_quotes() {
    let s = display_command(
        "qemu",
        &["-device".into(), r#"{"a":"b c"}"#.into(), "-S".into()],
    );
    assert_eq!(s, r#"qemu -device '{"a":"b c"}' -S"#);
}
