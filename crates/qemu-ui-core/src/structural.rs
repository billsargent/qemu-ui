//! L1 structural rules (plan.md §6.1, M4-01): high-confidence checks
//! derived from what the probed QEMU binary itself reports — its device
//! list (`-device help`), CPU models (`query-cpu-definitions`) and machine
//! properties (`query-machines`), plus the machine's bus tree (`info qtree`).
//!
//! Many structural checks already live next to the code they guard
//! (`compile::validate`, `compile::validate_buses`, `nic::validate_nic_buses`).
//! This module adds the ones that cut across the spec:
//!
//! * every device the spec names or implies exists, can be created by users,
//!   and has a bus on this machine to plug into;
//! * `-device` drivers in passthrough exist;
//! * CPU models: deprecated ones and features this host's TCG cannot emulate.
//!
//! Not a rule on purpose: `query-machines`' `numa-mem-supported` is about the
//! legacy `-numa node,mem=` form; memory backends (`memdev=`, used for
//! virtiofs) work on machines that report `false`.
//!
//! Every rule id is listed in `docs/rules/README.md`; [`docs_link`] gives the
//! anchor for a rule.

use crate::buses::MachineBuses;
use crate::caps::{BinaryCaps, DeviceInfo};
use crate::compile::{Context, Diagnostic, resolve_accel};
use crate::nic::bus_fits;
use crate::spec::VmSpec;

/// Where a rule is explained: an anchor in `docs/rules/README.md`.
pub fn docs_link(rule: &str) -> String {
    format!("docs/rules/README.md#{}", rule.replace('.', ""))
}

/// The device named `name` (or carrying it as an alias).
fn find<'a>(caps: &'a BinaryCaps, name: &str) -> Option<&'a DeviceInfo> {
    caps.devices.iter().find(|d| d.name == name).or_else(|| {
        caps.devices
            .iter()
            .find(|d| d.alias.as_deref() == Some(name))
    })
}

/// A device the VM will contain, and the spec field responsible for it.
struct Named {
    field: String,
    driver: String,
    /// Named explicitly by the user (vs implied by a feature).
    explicit: bool,
    /// Existence is already reported by an older rule; don't repeat it.
    missing_reported: bool,
}

/// `-device` drivers in passthrough tokens: `-device drv,…`, `-device
/// driver=drv,…` and the JSON form `-device {"driver":"drv",…}`.
pub fn passthrough_devices(tokens: &[String]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        let (value, at) = if t == "-device" || t == "--device" {
            match tokens.get(i + 1) {
                Some(v) => (v.as_str(), i + 1),
                None => break,
            }
        } else if let Some(v) = t
            .strip_prefix("-device=")
            .or_else(|| t.strip_prefix("--device="))
        {
            (v, i)
        } else {
            i += 1;
            continue;
        };
        let driver = if value.trim_start().starts_with('{') {
            serde_json::from_str::<serde_json::Value>(value)
                .ok()
                .and_then(|v| v["driver"].as_str().map(String::from))
        } else {
            let first = value.split(',').next().unwrap_or_default();
            Some(first.strip_prefix("driver=").unwrap_or(first).to_string())
        };
        if let Some(d) = driver.filter(|d| !d.is_empty() && d != "help" && d != "?") {
            out.push((at, d));
        }
        i = at + 1;
    }
    out
}

fn named_devices(spec: &VmSpec, has_usb_disk: bool) -> Vec<Named> {
    let mut v = Vec::new();
    let mut push = |field: String, driver: &str, explicit: bool, missing_reported: bool| {
        v.push(Named {
            field,
            driver: driver.to_string(),
            explicit,
            missing_reported,
        })
    };
    if let Some(dev) = &spec.display.device {
        push("display.device".into(), dev, true, true);
    }
    if spec.display.vdagent {
        push("display.vdagent".into(), "virtio-serial-pci", false, true);
    }
    if spec.input.usb_tablet || has_usb_disk || !spec.usb.is_empty() {
        let field = if spec.input.usb_tablet {
            "input.usb_tablet"
        } else if !spec.usb.is_empty() {
            "usb"
        } else {
            "disks"
        };
        push(field.into(), "qemu-xhci", false, true);
    }
    for (i, n) in spec.nics.iter().enumerate() {
        // Existence and bus fit: nic.model-missing / nic.model-bus.
        push(format!("nics[{i}].model"), &n.model, true, true);
    }
    for (i, drv) in passthrough_devices(&spec.passthrough) {
        push(format!("passthrough[{i}]"), &drv, true, false);
    }
    v
}

/// Rules that need the compile context (machine buses).
pub fn check(spec: &VmSpec, ctx: &Context) -> Vec<Diagnostic> {
    let caps = ctx.caps;
    let mut d = Vec::new();
    let has_usb_disk = spec
        .disks
        .iter()
        .any(|x| x.bus == crate::spec::DiskBus::Usb);

    // Devices: exist, user-creatable, pluggable on this machine.
    for n in named_devices(spec, has_usb_disk) {
        let Some(dev) = find(caps, &n.driver) else {
            if !n.missing_reported && !caps.devices.is_empty() {
                d.push(Diagnostic::error(
                    &n.field,
                    "device.unknown",
                    format!(
                        "this QEMU ({}) has no device '{}'; `{} -device help` lists the ones it has",
                        caps.arch, n.driver, caps.binary.id
                    ),
                ));
            }
            continue;
        };
        if dev.no_user {
            d.push(Diagnostic::error(
                &n.field,
                "device.no-user",
                format!(
                    "'{}' exists but QEMU only creates it internally (it is not user-creatable); \
                     {}",
                    n.driver,
                    if n.explicit {
                        "pick another model"
                    } else {
                        "turn this feature off"
                    }
                ),
            ));
            continue;
        }
        let is_nic = n.field.starts_with("nics[");
        if let (Some(m), false) = (ctx.machine, is_nic) {
            bus_check(&mut d, &n, dev, m);
        }
    }

    // CPU model facts from query-cpu-definitions.
    if let Some(model) = spec.cpu.model.as_deref().filter(|m| *m != "host")
        && let Some(cpu) = caps.cpus.iter().find(|c| c.name == model)
    {
        if cpu.deprecated {
            d.push(Diagnostic::warning(
                "cpu.model",
                "cpu.deprecated",
                format!(
                    "CPU model '{model}' is deprecated in QEMU {} and may be removed",
                    caps.binary.version
                ),
            ));
        }
        // The probe asks with TCG, so the list says what TCG can't emulate.
        let tcg = resolve_accel(spec, caps).is_ok_and(|a| a == "tcg");
        if tcg && !cpu.unavailable_features.is_empty() {
            let mut feats = cpu.unavailable_features.clone();
            let more = feats.len().saturating_sub(8);
            feats.truncate(8);
            d.push(Diagnostic::warning(
                "cpu.model",
                "cpu.unavailable-features",
                format!(
                    "under TCG, '{model}' lacks: {}{}; QEMU disables them (or refuses with \
                     enforce). A plainer model such as 'qemu64' or 'max' avoids this",
                    feats.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ),
            ));
        }
    }

    d
}

fn bus_check(d: &mut Vec<Diagnostic>, n: &Named, dev: &DeviceInfo, m: &MachineBuses) {
    if let Err(why) = bus_fits(dev.bus.as_deref(), m) {
        d.push(Diagnostic::error(
            &n.field,
            "device.bus-unavailable",
            format!(
                "'{}' plugs into a {} bus: {why}",
                n.driver,
                dev.bus.as_deref().unwrap_or("?")
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn passthrough_device_forms() {
        let t = toks(&[
            "-device",
            "virtio-rng-pci,id=rng",
            "-object",
            "rng-random,id=r",
            "-device",
            "driver=pvpanic,id=p",
            "-device",
            r#"{"driver":"i6300esb","id":"w"}"#,
            "--device=edu",
            "-device",
            "help",
        ]);
        let got: Vec<_> = passthrough_devices(&t);
        assert_eq!(
            got,
            vec![
                (1, "virtio-rng-pci".to_string()),
                (5, "pvpanic".to_string()),
                (7, r#"i6300esb"#.to_string()),
                (8, "edu".to_string())
            ]
        );
        assert!(
            passthrough_devices(&toks(&["-device"])).is_empty(),
            "dangling flag"
        );
    }

    #[test]
    fn docs_anchor() {
        assert_eq!(
            docs_link("device.no-user"),
            "docs/rules/README.md#deviceno-user"
        );
    }
}
