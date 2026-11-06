//! Dry lint support (plan.md §6.3, M1-10): the pure parts.
//!
//! "Check config" runs the VM's real argv through QEMU with every side
//! effect rewritten away (scratch disks, scratch NVRAM, no host port
//! forwards, no VNC listener, sockets in a temp dir). It is authoritative for
//! the device/machine model, not for host resources. QEMU's error text is
//! unstructured; we map it back to spec fields through the reserved `qui-`
//! ids the compiler assigns, which QEMU echoes in its messages. That mapping
//! is a heuristic and is labelled as such.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::compile::{Diagnostic, Severity};
use crate::spec::{NicBackend, VmSpec};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct LintResult {
    /// QEMU accepted the configuration.
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// QEMU version that checked it.
    pub qemu_version: String,
    #[ts(type = "number")]
    pub duration_ms: u64,
    /// Served from the cache (same spec, same binary).
    pub cached: bool,
}

/// The spec as linted: no side effects on the host.
pub fn lint_spec(spec: &VmSpec) -> VmSpec {
    let mut s = spec.clone();
    for nic in &mut s.nics {
        let NicBackend::User { forwards } = &mut nic.backend;
        forwards.clear();
    }
    s.display.vnc = false;
    s
}

/// Which spec field a reserved id belongs to.
fn field_for_id(id: &str) -> Option<String> {
    let num = |prefix: &str| {
        id.strip_prefix(prefix).and_then(|r| {
            let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<usize>().ok()
        })
    };
    if let Some(n) = num("qui-dev").or_else(|| num("qui-disk")) {
        return Some(format!("disks[{n}]"));
    }
    if let Some(n) = num("qui-nic").or_else(|| num("qui-net")) {
        return Some(format!("nics[{n}]"));
    }
    Some(
        match id {
            i if i.starts_with("qui-gpu") => "display.device",
            i if i.starts_with("qui-vdagent") || i.starts_with("qui-vser") => "display.vdagent",
            i if i.starts_with("qui-tablet") || i == "qui-usb" => "input.usb_tablet",
            i if i.starts_with("qui-fw") => "firmware",
            i if i.starts_with("qui-scsi") => "disks",
            _ => return None,
        }
        .to_string(),
    )
}

/// Map one line of QEMU stderr to a spec field.
fn field_for_line(line: &str, spec: &VmSpec) -> String {
    // Reserved ids first: the most precise signal.
    let mut rest = line;
    while let Some(i) = rest.find("qui-") {
        let id: String = rest[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if let Some(f) = field_for_id(&id) {
            return f;
        }
        rest = &rest[i + 4..];
    }
    // Then the option QEMU names at the start of the message.
    let opt = line.trim_start();
    let by_option = [
        ("-machine ", "machine.type"),
        ("-cpu ", "cpu.model"),
        ("-smp ", "cpu.count"),
        ("-m ", "memory.size_mib"),
        ("-accel ", "accel"),
    ];
    if let Some((_, f)) = by_option.iter().find(|(o, _)| opt.starts_with(o)) {
        return f.to_string();
    }
    // Finally passthrough tokens quoted in the message; the longest match is
    // the most specific (a value like `ICH9-LPC.x=1` beats its `-global`).
    if let Some((i, _)) = spec
        .passthrough
        .iter()
        .enumerate()
        .filter(|(_, t)| t.len() > 2 && line.contains(t.as_str()))
        .max_by_key(|(_, t)| t.len())
    {
        return format!("passthrough[{i}]");
    }
    String::new()
}

/// Strip QEMU's `/path/qemu-system-x86_64[.exe]: ` prefix.
fn strip_program(line: &str) -> &str {
    for marker in [".exe: ", "qemu-system-"] {
        if let Some(i) = line.find(marker) {
            let after = &line[i..];
            if let Some(j) = after.find(": ") {
                return after[j + 2..].trim();
            }
        }
    }
    line.trim()
}

/// Turn QEMU's stderr into diagnostics pinned to spec fields.
pub fn diagnostics_from_stderr(stderr: &str, spec: &VmSpec) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|raw| {
            let line = strip_program(raw);
            let warning = line.contains("warning:");
            Diagnostic {
                severity: if warning {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                field: field_for_line(line, spec),
                rule: "qemu.lint".into(),
                message: line.chars().take(600).collect(),
            }
        })
        .collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;
    use std::collections::BTreeMap;

    fn spec() -> VmSpec {
        VmSpec {
            spec_version: SPEC_VERSION,
            name: "t".into(),
            arch: "x86_64".into(),
            machine: MachineSpec {
                type_name: "pc-q35-11.1".into(),
                props: BTreeMap::new(),
            },
            accel: "auto".into(),
            cpu: CpuSpec {
                model: None,
                props: BTreeMap::new(),
                count: 1,
                topology: None,
            },
            memory: MemorySpec { size_mib: 128 },
            firmware: FirmwareSpec::Default,
            disks: vec![],
            nics: vec![NicSpec {
                backend: NicBackend::User {
                    forwards: vec![PortForward {
                        proto: "tcp".into(),
                        host_addr: "127.0.0.1".into(),
                        host_port: 2222,
                        guest_port: 22,
                    }],
                },
                model: "e1000".into(),
                mac: Some("52:54:00:00:00:01".into()),
            }],
            display: DisplaySpec::default(),
            input: InputSpec::default(),
            passthrough: vec!["-global".into(), "ICH9-LPC.bogus=1".into()],
        }
    }

    #[test]
    fn lint_spec_removes_host_side_effects() {
        let s = lint_spec(&spec());
        let NicBackend::User { forwards } = &s.nics[0].backend;
        assert!(forwards.is_empty());
        assert!(!s.display.vnc);
    }

    #[test]
    fn maps_real_qemu_messages() {
        let s = spec();
        let cases = [
            (
                r#"C:\q\qemu-system-x86_64.exe: -device {"bootindex":1,"drive":"qui-disk1","driver":"virtio-blk-pci","id":"qui-dev1"}: Bus 'pcie.0' does not support hotplugging"#,
                "disks[1]",
            ),
            (
                r#"qemu-system-x86_64: -device {"driver":"e1000","id":"qui-nic0","netdev":"qui-net0"}: oops"#,
                "nics[0]",
            ),
            (
                "qemu-system-x86_64: -machine pc-q35-11.1,smm=maybe: Parameter 'smm' expects 'on' or 'off'",
                "machine.type",
            ),
            (
                "qemu-system-x86_64: -smp 999: Invalid SMP CPUs 999. The max CPUs supported by machine 'pc-q35-11.1' is 288",
                "cpu.count",
            ),
            (
                "qemu-system-x86_64: -global ICH9-LPC.bogus=1: can't apply global ICH9-LPC.bogus=1: Property 'ICH9-LPC.bogus' not found",
                "passthrough[1]",
            ),
            ("qemu-system-x86_64: something unrelated", ""),
        ];
        for (stderr, field) in cases {
            let d = diagnostics_from_stderr(stderr, &s);
            assert_eq!(d.len(), 1, "{stderr}");
            assert_eq!(d[0].field, field, "{stderr}");
            assert!(
                !d[0].message.contains("qemu-system-x86_64.exe"),
                "program prefix stripped"
            );
        }
    }

    #[test]
    fn warnings_are_warnings() {
        let d = diagnostics_from_stderr(
            "qemu-system-x86_64: warning: host doesn't support feature X",
            &spec(),
        );
        assert_eq!(d[0].severity, Severity::Warning);
    }
}
