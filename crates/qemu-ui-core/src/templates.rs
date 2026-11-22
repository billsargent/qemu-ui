//! Template gallery (M7-04) and the Windows 11 lane (M7-07).
//!
//! Templates are data (`data/templates/*.toml`): sizing and the firmware,
//! TPM and driver needs of a guest OS. [`plan`] turns one into a spec for a
//! given binary and host, saying what it could not give the guest and why;
//! [`attach`] adds the new system disk and installer media. The daemon does
//! the I/O (creating the disk image) in between.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::caps::BinaryCaps;
use crate::firmware::Firmware;
use crate::spec::{DiskBus, DiskSpec, FirmwareSpec, ImageRef, Media, TpmSpec, VmSpec};

/// Template files shipped with qemu-ui.
const BUILTIN: &[(&str, &str)] = &[
    (
        "linux.toml",
        include_str!("../../../data/templates/linux.toml"),
    ),
    (
        "windows.toml",
        include_str!("../../../data/templates/windows.toml"),
    ),
    (
        "other.toml",
        include_str!("../../../data/templates/other.toml"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum GuestOs {
    Linux,
    Windows,
    Bsd,
    Other,
}

/// What firmware the guest needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FirmwareNeed {
    /// The machine's default (SeaBIOS on x86, built-in elsewhere).
    #[default]
    Default,
    Uefi,
    UefiSecureBoot,
}

/// Whether the guest wants a TPM 2.0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TpmNeed {
    #[default]
    None,
    /// Used when the host can provide one.
    Preferred,
    /// The OS installer refuses to run without one (Windows 11).
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct Template {
    /// Stable id, e.g. `windows-11`.
    pub id: String,
    pub name: String,
    pub description: String,
    pub os: GuestOs,
    /// Architectures it applies to; empty means any.
    #[serde(default)]
    pub arch: Vec<String>,
    #[ts(type = "number")]
    pub memory_mib: u64,
    pub cpus: u32,
    #[ts(type = "number")]
    pub disk_gib: u64,
    #[serde(default)]
    pub firmware: FirmwareNeed,
    #[serde(default)]
    pub tpm: TpmNeed,
    /// Paravirtual drivers the installer needs from a separate ISO
    /// (`virtio-win`); without that ISO emulated devices are used instead.
    #[serde(default)]
    #[ts(optional)]
    pub drivers: Option<String>,
    /// Where to get the drivers ISO.
    #[serde(default)]
    #[ts(optional)]
    pub drivers_url: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    #[ts(optional)]
    pub docs: Option<String>,
}

impl Template {
    pub fn applies_to(&self, arch: &str) -> bool {
        self.arch.is_empty() || self.arch.iter().any(|a| a == arch)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    template: Vec<Template>,
}

/// Parse one template file.
pub fn parse(text: &str) -> Result<Vec<Template>, String> {
    toml::from_str::<TemplateFile>(text)
        .map(|f| f.template)
        .map_err(|e| e.to_string())
}

/// The templates shipped with qemu-ui. Panics only on a broken shipped file,
/// which the tests catch.
pub fn builtin() -> Vec<Template> {
    let mut out: Vec<Template> = Vec::new();
    for (name, text) in BUILTIN {
        match parse(text) {
            Ok(t) => out.extend(t),
            Err(e) => panic!("built-in template file {name}: {e}"),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum NoteLevel {
    Info,
    Warning,
}

/// Something the plan could not give the guest, or a choice it made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateNote {
    pub level: NoteLevel,
    pub text: String,
}

/// A template turned into a spec for one binary and host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplatePlan {
    pub template: String,
    /// The spec without its system disk and media; see [`attach`].
    pub spec: VmSpec,
    /// Size and bus of the system disk to create.
    #[ts(type = "number")]
    pub disk_gib: u64,
    pub disk_bus: DiskBus,
    pub notes: Vec<TemplateNote>,
    /// Set when the template needs a TPM this host cannot provide: the
    /// reason. Creating the VM then needs the user's explicit consent.
    #[ts(optional)]
    pub tpm_missing: Option<String>,
}

/// A template with what this host offers it (`GET /templates`).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TemplateInfo {
    pub template: Template,
    /// Installed architectures it applies to.
    pub arches: Vec<String>,
    /// Why no TPM can be attached here, when the template wants one.
    #[ts(optional)]
    pub tpm_unavailable: Option<String>,
}

/// The system disk to create for a VM from a template.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NewDisk {
    pub pool: String,
    #[ts(type = "number")]
    pub size_gib: u64,
    pub bus: DiskBus,
}

/// `POST /vms/from-template`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FromTemplate {
    pub template: String,
    /// The plan's spec, possibly edited (without the system disk).
    pub spec: VmSpec,
    pub disk: NewDisk,
    #[serde(default)]
    #[ts(optional)]
    pub install: Option<ImageRef>,
    #[serde(default)]
    #[ts(optional)]
    pub drivers: Option<ImageRef>,
    /// The template requires a TPM this host cannot provide, and the user
    /// accepts creating the VM without one (M7-07).
    #[serde(default)]
    pub accept_without_tpm: bool,
}

/// What the host offers a template besides the binary.
pub struct Host<'a> {
    pub firmware: &'a [Firmware],
    /// `Err(reason)` when no TPM can be attached: the binary lacks a TPM
    /// backend or `swtpm` is not installed.
    pub tpm: Result<(), String>,
}

/// Pick firmware for `need` on `arch`/`machine`. Secure Boot prefers images
/// with Microsoft keys enrolled (what Windows needs).
pub fn pick_firmware<'a>(
    need: FirmwareNeed,
    arch: &str,
    machine: &str,
    firmware: &'a [Firmware],
) -> Option<&'a Firmware> {
    let uefi = |f: &&Firmware| {
        f.complete
            && f.device == "flash"
            && f.interface_types.iter().any(|t| t == "uefi")
            && f.supports(arch, machine)
    };
    let enrolled = |f: &&Firmware| f.features.iter().any(|x| x == "enrolled-keys");
    match need {
        FirmwareNeed::Default => None,
        FirmwareNeed::Uefi => {
            let all: Vec<&Firmware> = firmware.iter().filter(uefi).collect();
            all.iter()
                .find(|f| !f.secure_boot())
                .or_else(|| all.first())
                .copied()
        }
        FirmwareNeed::UefiSecureBoot => {
            let sb: Vec<&Firmware> = firmware
                .iter()
                .filter(uefi)
                .filter(|f| f.secure_boot())
                .collect();
            sb.iter()
                .find(|f| enrolled(f))
                .or_else(|| sb.first())
                .copied()
        }
    }
}

/// Turn `t` into a spec named `name` on top of `base` (the binary's
/// defaults, [`crate::vm::default_spec_on`]). `with_drivers`: the user
/// supplies the paravirtual drivers ISO the template asks for.
pub fn plan(
    t: &Template,
    mut base: VmSpec,
    caps: &BinaryCaps,
    host: &Host,
    with_drivers: bool,
) -> TemplatePlan {
    let mut notes = Vec::new();
    let mut note = |level, text: String| notes.push(TemplateNote { level, text });
    let has = |d: &str| caps.devices.iter().any(|x| x.name == d && !x.no_user);
    let hardware = caps.best_accel().is_some_and(|a| a.hardware);

    base.memory.size_mib = t.memory_mib;
    let cpu_max = caps
        .machines
        .iter()
        .find(|m| m.name == base.machine.type_name)
        .and_then(|m| m.cpu_max)
        .unwrap_or(u32::MAX);
    base.cpu.count = t.cpus.min(cpu_max).max(1);
    if !hardware {
        note(
            NoteLevel::Warning,
            "No hardware accelerator is available for this architecture: the guest runs under TCG emulation and will be slow.".into(),
        );
    }

    // Firmware.
    if t.firmware != FirmwareNeed::Default {
        match pick_firmware(
            t.firmware,
            &caps.arch,
            &base.machine.type_name,
            host.firmware,
        ) {
            Some(fw) => {
                base.firmware = FirmwareSpec::Descriptor { id: fw.id.clone() };
                if fw.features.iter().any(|f| f == "requires-smm") {
                    base.machine.props.insert("smm".into(), "on".into());
                }
            }
            None => note(
                NoteLevel::Warning,
                match t.firmware {
                    FirmwareNeed::UefiSecureBoot => format!(
                        "{} needs UEFI firmware with Secure Boot, and none was found for {} / {}. Install edk2/OVMF (the '-secure' variant); the VM uses the default firmware until then.",
                        t.name, caps.arch, base.machine.type_name
                    ),
                    _ => format!(
                        "{} expects UEFI firmware, and none was found for {} / {}; the VM uses the default firmware.",
                        t.name, caps.arch, base.machine.type_name
                    ),
                },
            ),
        }
    }
    let uefi = matches!(base.firmware, FirmwareSpec::Descriptor { .. });

    // Devices: paravirtual where the guest has drivers.
    let windows = t.os == GuestOs::Windows;
    let paravirt = !windows || (with_drivers && t.drivers.is_some());
    let disk_bus = if paravirt && has("virtio-blk-pci") {
        DiskBus::Virtio
    } else if has("ide-hd") {
        DiskBus::Ide
    } else {
        DiskBus::Virtio
    };
    if windows {
        // Hyper-V enlightenments: big speedups, and how the rules recognise
        // a Windows guest. Only KVM is known to take them.
        if caps.best_accel().is_some_and(|a| a.name == "kvm") {
            for (k, v) in [
                ("hv-relaxed", "on"),
                ("hv-vapic", "on"),
                ("hv-spinlocks", "0x1fff"),
                ("hv-time", "on"),
            ] {
                base.cpu.props.insert(k.into(), v.into());
            }
        }
        if !paravirt {
            for nic in &mut base.nics {
                if nic.model.starts_with("virtio") && has("e1000e") {
                    nic.model = "e1000e".into();
                }
            }
            if let Some(drivers) = &t.drivers {
                note(
                    NoteLevel::Info,
                    format!(
                        "Without the {drivers} drivers ISO the VM uses emulated SATA and e1000e devices, which Windows supports out of the box but which are slower. Attach the ISO to use virtio devices{}.",
                        t.drivers_url
                            .as_deref()
                            .map(|u| format!(" (download: {u})"))
                            .unwrap_or_default()
                    ),
                );
            }
        } else if let Some(drivers) = &t.drivers {
            note(
                NoteLevel::Info,
                format!(
                    "The system disk is virtio: in Windows setup choose 'Load driver' and browse the {drivers} CD (viostor for your Windows version)."
                ),
            );
        }
        // Windows' basic display driver handles the standard VGA adapter.
        if has("VGA") {
            base.display.device = Some("VGA".into());
        }
    }

    // TPM.
    let mut tpm_missing = None;
    if t.tpm != TpmNeed::None {
        match &host.tpm {
            Ok(()) => {
                let model = if uefi && has("tpm-crb") {
                    "tpm-crb"
                } else {
                    "tpm-tis"
                };
                base.tpm = Some(TpmSpec {
                    model: model.into(),
                });
            }
            Err(why) => {
                if t.tpm == TpmNeed::Required {
                    note(
                        NoteLevel::Warning,
                        format!(
                            "{} requires a TPM 2.0 and this host cannot provide one: {why}. Setup will stop at its hardware check. Microsoft documents a registry setting that lets it continue (in setup press Shift+F10 and run: reg add HKLM\\SYSTEM\\Setup\\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1). Whether to use it is your choice; the result is an unsupported installation.",
                            t.name
                        ),
                    );
                    tpm_missing = Some(why.clone());
                } else {
                    note(NoteLevel::Info, format!("No TPM is attached: {why}."));
                }
            }
        }
    }
    for n in &t.notes {
        note(NoteLevel::Info, n.clone());
    }

    TemplatePlan {
        template: t.id.clone(),
        spec: base,
        disk_gib: t.disk_gib,
        disk_bus,
        notes,
        tpm_missing,
    }
}

/// Add the system disk (boots first) and optional installer and driver CDs
/// to a planned spec.
pub fn attach(
    spec: &VmSpec,
    disk_bus: DiskBus,
    disk: ImageRef,
    install: Option<ImageRef>,
    drivers: Option<ImageRef>,
) -> VmSpec {
    let mut spec = spec.clone();
    // x86 machines have IDE/AHCI CD-ROMs; virt-style machines use scsi-cd
    // on virtio-scsi.
    let cd_bus = if spec.arch == "x86_64" || spec.arch == "i386" {
        DiskBus::Ide
    } else {
        DiskBus::Scsi
    };
    let disk_spec = |image, bus, media, boot: Option<u32>| DiskSpec {
        image,
        bus,
        media,
        readonly: media == Media::Cdrom,
        boot_index: boot,
        cache: Default::default(),
        aio: None,
        discard: media == Media::Disk,
        slot: None,
    };
    spec.disks
        .push(disk_spec(disk, disk_bus, Media::Disk, Some(1)));
    if let Some(i) = install {
        spec.disks.push(disk_spec(i, cd_bus, Media::Cdrom, Some(2)));
    }
    if let Some(d) = drivers {
        spec.disks.push(disk_spec(d, cd_bus, Media::Cdrom, None));
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{
        AccelInfo, Backends, Binary, CpuModel, DeviceInfo, MachineInfo, QemuVersion,
    };

    fn caps(arch: &str, accel: &str, hardware: bool) -> BinaryCaps {
        BinaryCaps {
            binary: Binary {
                id: String::new(),
                path: String::new(),
                arch: Some(arch.into()),
                version: String::new(),
                sha256: String::new(),
                source: String::new(),
            },
            arch: arch.into(),
            qemu_version: QemuVersion {
                major: 11,
                minor: 1,
                micro: 0,
                package: String::new(),
            },
            probe_version: crate::caps::PROBE_VERSION,
            probed_at: 0,
            native: hardware,
            machines: vec![MachineInfo {
                name: "pc-q35-11.1".into(),
                description: String::new(),
                alias: Some("q35".into()),
                is_default: false,
                deprecated: false,
                cpu_max: Some(288),
                default_cpu_type: None,
                hotpluggable_cpus: None,
                numa_mem_supported: None,
            }],
            cpus: vec![CpuModel {
                name: "max".into(),
                deprecated: false,
                unavailable_features: vec![],
            }],
            devices: [
                "virtio-blk-pci",
                "ide-hd",
                "ide-cd",
                "virtio-net-pci",
                "e1000e",
                "VGA",
                "virtio-vga",
                "tpm-crb",
                "tpm-tis",
                "qemu-xhci",
                "usb-tablet",
            ]
            .iter()
            .map(|n| DeviceInfo {
                name: n.to_string(),
                category: String::new(),
                bus: None,
                alias: None,
                description: None,
                no_user: false,
            })
            .collect(),
            accels: vec![AccelInfo {
                name: accel.into(),
                available: true,
                hardware,
                reason: None,
                remediation: None,
            }],
            backends: Backends {
                tpmdev: Some(vec!["emulator".into()]),
                ..Default::default()
            },
            qmp_schema_hash: String::new(),
            warnings: vec![],
        }
    }

    fn fw(id: &str, features: &[&str]) -> Firmware {
        Firmware {
            id: id.into(),
            description: String::new(),
            interface_types: vec!["uefi".into()],
            device: "flash".into(),
            executable: None,
            nvram_template: None,
            targets: vec![crate::firmware::FirmwareTarget {
                architecture: "x86_64".into(),
                machines: vec!["pc-q35-*".into()],
            }],
            features: features.iter().map(|s| s.to_string()).collect(),
            complete: true,
        }
    }

    fn win11() -> Template {
        builtin()
            .into_iter()
            .find(|t| t.id == "windows-11")
            .unwrap()
    }

    fn base(c: &BinaryCaps) -> VmSpec {
        crate::vm::default_spec("win", c).0
    }

    #[test]
    fn shipped_templates_parse_and_have_unique_ids() {
        let all = builtin();
        assert!(all.len() >= 6);
        let mut ids: Vec<_> = all.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "duplicate template ids");
        for t in &all {
            assert!(
                t.memory_mib >= 128 && t.cpus >= 1 && t.disk_gib >= 1,
                "{}",
                t.id
            );
        }
    }

    #[test]
    fn windows_11_on_a_linux_kvm_host_gets_the_full_lane() {
        let c = caps("x86_64", "kvm", true);
        let firmware = [
            fw("60-edk2-x86_64", &[]),
            fw("50-edk2-x86_64-secure", &["secure-boot", "requires-smm"]),
            fw(
                "40-edk2-x86_64-secure-enrolled",
                &["secure-boot", "requires-smm", "enrolled-keys"],
            ),
        ];
        let host = Host {
            firmware: &firmware,
            tpm: Ok(()),
        };
        let p = plan(&win11(), base(&c), &c, &host, true);
        assert_eq!(
            p.spec.firmware,
            FirmwareSpec::Descriptor {
                id: "40-edk2-x86_64-secure-enrolled".into()
            }
        );
        assert_eq!(
            p.spec.machine.props.get("smm").map(String::as_str),
            Some("on")
        );
        assert_eq!(
            p.spec.tpm,
            Some(TpmSpec {
                model: "tpm-crb".into()
            })
        );
        assert!(p.tpm_missing.is_none());
        assert_eq!(p.disk_bus, DiskBus::Virtio, "virtio-win supplied");
        assert!(p.spec.cpu.props.contains_key("hv-relaxed"));
        assert!(p.spec.memory.size_mib >= 4096 && p.spec.cpu.count >= 2);
        assert!(p.disk_gib >= 64);

        // The compiled argv carries Secure Boot and the TPM.
        let spec = attach(
            &p.spec,
            p.disk_bus,
            ImageRef {
                pool: "default".into(),
                name: "win-disk0.qcow2".into(),
            },
            Some(ImageRef {
                pool: "default".into(),
                name: "win11.iso".into(),
            }),
            Some(ImageRef {
                pool: "default".into(),
                name: "virtio-win.iso".into(),
            }),
        );
        assert_eq!(spec.disks.len(), 3);
        assert_eq!(spec.disks[0].boot_index, Some(1));
        assert_eq!(spec.disks[1].media, Media::Cdrom);
        assert!(spec.disks[2].readonly);
    }

    #[test]
    fn windows_11_without_a_tpm_backend_explains_and_needs_consent() {
        // A Windows host: WHPX, no -tpmdev.
        let c = caps("x86_64", "whpx", true);
        let firmware = [fw(
            "50-edk2-x86_64-secure",
            &["secure-boot", "requires-smm"],
        )];
        let host = Host {
            firmware: &firmware,
            tpm: Err("this QEMU build has no TPM support".into()),
        };
        let p = plan(&win11(), base(&c), &c, &host, false);
        assert_eq!(p.spec.tpm, None);
        assert!(p.tpm_missing.is_some());
        let warning = p
            .notes
            .iter()
            .find(|n| n.level == NoteLevel::Warning && n.text.contains("TPM"))
            .expect("a TPM warning");
        assert!(
            warning.text.contains("BypassTPMCheck"),
            "offers the documented bypass"
        );
        // No virtio-win: devices Windows setup can use as-is.
        assert_eq!(p.disk_bus, DiskBus::Ide);
        assert!(p.spec.nics.iter().all(|n| n.model == "e1000e"));
        assert!(
            !p.spec.cpu.props.contains_key("hv-relaxed"),
            "hv-* only on KVM"
        );
        // Secure Boot still applies.
        assert!(matches!(p.spec.firmware, FirmwareSpec::Descriptor { .. }));
    }

    #[test]
    fn missing_secure_boot_firmware_is_a_warning() {
        let c = caps("x86_64", "kvm", true);
        let firmware = [fw("60-edk2-x86_64", &[])];
        let host = Host {
            firmware: &firmware,
            tpm: Ok(()),
        };
        let p = plan(&win11(), base(&c), &c, &host, true);
        assert_eq!(p.spec.firmware, FirmwareSpec::Default);
        assert!(
            p.notes
                .iter()
                .any(|n| n.level == NoteLevel::Warning && n.text.contains("Secure Boot"))
        );
        // A TPM on BIOS firmware uses the TIS interface.
        assert_eq!(
            p.spec.tpm,
            Some(TpmSpec {
                model: "tpm-tis".into()
            })
        );
    }

    #[test]
    fn linux_templates_use_virtio_and_no_tpm() {
        let c = caps("x86_64", "tcg", false);
        let host = Host {
            firmware: &[],
            tpm: Err("no".into()),
        };
        for t in builtin()
            .iter()
            .filter(|t| t.os == GuestOs::Linux && t.applies_to("x86_64"))
        {
            let p = plan(t, base(&c), &c, &host, false);
            assert_eq!(p.disk_bus, DiskBus::Virtio, "{}", t.id);
            assert!(p.tpm_missing.is_none(), "{}", t.id);
            assert!(
                p.notes.iter().any(|n| n.text.contains("TCG")),
                "TCG is called out"
            );
            let errors: Vec<_> = crate::compile::validate(&p.spec, &c)
                .into_iter()
                .filter(|d| {
                    d.severity == crate::compile::Severity::Error && d.rule != "nic.mac-missing"
                })
                .collect();
            assert!(errors.is_empty(), "{}: {errors:?}", t.id);
        }
    }

    #[test]
    fn firmware_picking() {
        let plain = fw("60-edk2-x86_64", &[]);
        let sb = fw("50-edk2-x86_64-secure", &["secure-boot"]);
        let all = [sb.clone(), plain.clone()];
        assert_eq!(
            pick_firmware(FirmwareNeed::Uefi, "x86_64", "pc-q35-11.1", &all).map(|f| &f.id),
            Some(&plain.id)
        );
        assert_eq!(
            pick_firmware(FirmwareNeed::UefiSecureBoot, "x86_64", "pc-q35-11.1", &all)
                .map(|f| &f.id),
            Some(&sb.id)
        );
        assert!(pick_firmware(FirmwareNeed::Uefi, "aarch64", "virt-11.1", &all).is_none());
        assert!(pick_firmware(FirmwareNeed::Default, "x86_64", "pc-q35-11.1", &all).is_none());
    }
}
