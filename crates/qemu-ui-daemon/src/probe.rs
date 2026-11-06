//! Capability probe for one QEMU binary (plan.md §4.2–4.4).
//!
//! One long-lived `-machine none` process answers QMP queries; help-text
//! fallbacks run in parallel; hardware accelerators are tested by actually
//! starting the arch's default machine with `-S` and quitting.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use qemu_ui_core::caps::{
    AccelInfo, Backends, Binary, BinaryCaps, CpuModel, DeviceProperty, HostInfo, MachineInfo,
    QemuVersion,
};
use qemu_ui_core::{arch, help};
use qemu_ui_qmp::Qmp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

const QMP_TIMEOUT: Duration = Duration::from_secs(30);
const HELP_TIMEOUT: Duration = Duration::from_secs(20);
const ACCEL_TIMEOUT: Duration = Duration::from_secs(20);

/// A `Command` that never opens a console window and dies with its handle.
pub fn command(path: &Path) -> tokio::process::Command {
    #[allow(unused_mut)]
    let mut cmd = tokio::process::Command::new(path);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.kill_on_drop(true).stdin(Stdio::null());
    cmd
}

/// Start `qemu-system-* <args> -qmp stdio` and connect. Returns the session,
/// the child, and the child's stderr handle for diagnostics.
async fn spawn_qmp(path: &Path, args: &[&str]) -> Result<(Qmp, tokio::process::Child)> {
    let mut child = command(path)
        .args(args)
        .args(["-nodefaults", "-display", "none", "-S", "-qmp", "stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", path.display()))?;
    match tokio::time::timeout(QMP_TIMEOUT, Qmp::from_child(&mut child)).await {
        Ok(Ok(qmp)) => Ok((qmp, child)),
        Ok(Err(e)) => Err(anyhow!(
            "{}",
            failure_reason(&mut child, &e.to_string()).await
        )),
        Err(_) => Err(anyhow!("timed out waiting for the QMP greeting")),
    }
}

/// Best explanation for a failed start: QEMU's stderr if it said anything.
async fn failure_reason(child: &mut tokio::process::Child, fallback: &str) -> String {
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let mut err = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_string(&mut err)).await;
    }
    let err = err.trim();
    if err.is_empty() {
        fallback.to_string()
    } else {
        tidy(err)
    }
}

/// Keep the useful part of a QEMU error: drop the binary path prefix and
/// build-tree paths, cap the length.
fn tidy(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.contains("warning:"))
        .unwrap_or(s);
    let line = match line.find(".exe: ").or_else(|| {
        line.find(": ")
            .filter(|_| line.starts_with('/') || line.starts_with("qemu"))
    }) {
        Some(i) => line[i..]
            .trim_start_matches(".exe")
            .trim_start_matches(": ")
            .trim(),
        None => line,
    };
    line.chars().take(400).collect()
}

async fn quit(qmp: &Qmp, mut child: tokio::process::Child) {
    let _ = qmp
        .execute_with_timeout("quit", None, Duration::from_secs(5))
        .await;
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

enum Help {
    Text(String),
    NotCompiled,
    Failed(String),
}

async fn help_text(path: &Path, option: &str) -> Help {
    let mut cmd = command(path);
    // Some targets (aarch64, riscv, …) have no default machine, and several
    // help options refuse to run without one.
    if option != "-machine" {
        cmd.args(["-machine", "none"]);
    }
    let out = tokio::time::timeout(HELP_TIMEOUT, cmd.args([option, "help"]).output()).await;
    match out {
        Err(_) => Help::Failed(format!("{option} help timed out")),
        Ok(Err(e)) => Help::Failed(e.to_string()),
        Ok(Ok(o)) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if help::is_invalid_option(&stderr) {
                Help::NotCompiled
            } else if o.status.success() {
                Help::Text(String::from_utf8_lossy(&o.stdout).into_owned())
            } else if let Some(text) = listing(&o.stdout, &stderr) {
                // Some options print a valid listing but exit non-zero
                // (`-tpmdev help` on Linux).
                Help::Text(text)
            } else {
                Help::Failed(format!("{option} help: {}", tidy(&stderr)))
            }
        }
    }
}

/// A help listing from either stream, if one is there despite a failure exit.
fn listing(stdout: &[u8], stderr: &str) -> Option<String> {
    [
        String::from_utf8_lossy(stdout).into_owned(),
        stderr.to_string(),
    ]
    .into_iter()
    .find(|t| !help::parse_name_list(t).is_empty())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Probe one system emulator.
pub async fn probe_binary(mut binary: Binary, host: &HostInfo) -> Result<BinaryCaps> {
    let path = Path::new(&binary.path).to_path_buf();
    let target = binary
        .arch
        .clone()
        .ok_or_else(|| anyhow!("not a system emulator"))?;
    let mut warnings = Vec::new();

    // 1. QMP queries on a `-machine none` instance.
    let (qmp, child) = spawn_qmp(&path, &["-machine", "none"]).await?;
    let g = qmp.greeting().clone();
    let qemu_version = QemuVersion {
        major: g.version.major,
        minor: g.version.minor,
        micro: g.version.micro,
        package: g.package.clone(),
    };
    binary.version = qemu_version.to_string();

    let machines_v = qmp
        .execute("query-machines", None)
        .await
        .context("query-machines")?;
    let cpus = match qmp.execute("query-cpu-definitions", None).await {
        Ok(v) => parse_cpus(&v),
        // Normal for targets without named CPU models (alpha, hppa, m68k, …).
        Err(e) if e.to_string().contains("not supported on this target") => Vec::new(),
        Err(e) => {
            warnings.push(format!("query-cpu-definitions: {e}"));
            Vec::new()
        }
    };
    let qmp_schema_hash = match qmp.execute("query-qmp-schema", None).await {
        Ok(v) => hex::encode(Sha256::digest(v.to_string().as_bytes())),
        Err(e) => {
            warnings.push(format!("query-qmp-schema: {e}"));
            String::new()
        }
    };
    quit(&qmp, child).await;

    // 2. Help-text enrichment and backend lists, in parallel.
    let (mh, dh, nd, cd, disp, aud, acc, tpm, obj) = tokio::join!(
        help_text(&path, "-machine"),
        help_text(&path, "-device"),
        help_text(&path, "-netdev"),
        help_text(&path, "-chardev"),
        help_text(&path, "-display"),
        help_text(&path, "-audiodev"),
        help_text(&path, "-accel"),
        help_text(&path, "-tpmdev"),
        help_text(&path, "-object"),
    );
    let mut list = |h: Help, what: &str| match h {
        Help::Text(t) => Some(help::parse_name_list(&t)),
        Help::NotCompiled => None,
        Help::Failed(e) => {
            warnings.push(format!("{what}: {e}"));
            None
        }
    };
    let backends = Backends {
        netdev: list(nd, "netdev"),
        chardev: list(cd, "chardev"),
        display: list(disp, "display"),
        audiodev: list(aud, "audiodev"),
        tpmdev: list(tpm, "tpmdev"),
        object: list(obj, "object"),
    };
    let accel_names = list(acc, "accel").unwrap_or_else(|| vec!["tcg".into()]);
    let help_machines = match mh {
        Help::Text(t) => help::parse_machine_help(&t),
        _ => Vec::new(),
    };
    let devices = match dh {
        Help::Text(t) => help::parse_device_help(&t),
        Help::NotCompiled => Vec::new(),
        Help::Failed(e) => {
            warnings.push(format!("device: {e}"));
            Vec::new()
        }
    };
    let machines = merge_machines(&machines_v, &help_machines);

    // 3. Accelerators.
    let native = arch::is_native(&target, &host.arch);
    let default_machine = machines
        .iter()
        .find(|m| m.is_default)
        .or_else(|| machines.iter().find(|m| m.name != "none"))
        .map(|m| m.name.clone());
    let mut accels = Vec::new();
    for name in accel_names {
        accels.push(test_accel(&path, &name, native, &host.arch, default_machine.as_deref()).await);
    }

    Ok(BinaryCaps {
        binary,
        arch: target,
        qemu_version,
        probe_version: qemu_ui_core::caps::PROBE_VERSION,
        probed_at: now(),
        native,
        machines,
        cpus,
        devices,
        accels,
        backends,
        qmp_schema_hash,
        warnings,
    })
}

fn parse_cpus(v: &Value) -> Vec<CpuModel> {
    let mut cpus: Vec<CpuModel> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(CpuModel {
                name: c["name"].as_str()?.to_string(),
                deprecated: c["deprecated"].as_bool().unwrap_or(false),
                unavailable_features: c["unavailable-features"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|f| f.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();
    cpus.sort_by(|a, b| a.name.cmp(&b.name));
    cpus
}

/// `query-machines` is authoritative for the list and flags; `-machine help`
/// supplies descriptions.
fn merge_machines(qmp: &Value, from_help: &[MachineInfo]) -> Vec<MachineInfo> {
    let mut out: Vec<MachineInfo> = qmp
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m["name"].as_str()?.to_string();
            let h = from_help.iter().find(|h| h.name == name);
            Some(MachineInfo {
                description: h.map(|h| h.description.clone()).unwrap_or_default(),
                alias: m["alias"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| h.and_then(|h| h.alias.clone())),
                is_default: m["is-default"].as_bool().unwrap_or(false),
                deprecated: m["deprecated"].as_bool().unwrap_or(false),
                cpu_max: m["cpu-max"].as_u64().map(|n| n as u32),
                default_cpu_type: m["default-cpu-type"].as_str().map(String::from),
                hotpluggable_cpus: m["hotpluggable-cpus"].as_bool(),
                numa_mem_supported: m["numa-mem-supported"].as_bool(),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    out
}

/// Sort `pc-q35-10.0` after `pc-q35-9.2`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| {
        s.split(['-', '.'])
            .map(|p| {
                p.parse::<u64>()
                    .map_or_else(|_| (1, 0, p.to_string()), |n| (0, n, String::new()))
            })
            .collect::<Vec<_>>()
    };
    key(a).cmp(&key(b))
}

async fn test_accel(
    path: &Path,
    name: &str,
    native: bool,
    host_arch: &str,
    machine: Option<&str>,
) -> AccelInfo {
    let hardware = name != "tcg" && name != "qtest";
    let mut info = AccelInfo {
        name: name.into(),
        available: true,
        hardware,
        reason: None,
        remediation: None,
    };
    if !hardware {
        return info;
    }
    if !native {
        info.available = false;
        info.reason = Some(format!(
            "Hardware acceleration only runs guests of the host architecture ({host_arch})."
        ));
        return info;
    }
    if let Some(reason) = precheck(name) {
        info.available = false;
        info.remediation = remediation(name, &reason);
        info.reason = Some(reason);
        return info;
    }
    let Some(machine) = machine else {
        info.available = false;
        info.reason = Some("no machine type available to test with".into());
        return info;
    };
    let attempt = tokio::time::timeout(
        ACCEL_TIMEOUT,
        spawn_qmp(path, &["-accel", name, "-machine", machine, "-m", "64"]),
    )
    .await;
    match attempt {
        Ok(Ok((qmp, child))) => quit(&qmp, child).await,
        Ok(Err(e)) => {
            info.available = false;
            info.reason = Some(e.to_string());
            info.remediation = remediation(name, &e.to_string());
        }
        Err(_) => {
            info.available = false;
            info.reason = Some("timed out".into());
        }
    }
    info
}

/// Cheap checks that give better reasons than QEMU's error message.
fn precheck(accel: &str) -> Option<String> {
    if accel == "kvm" && cfg!(target_os = "linux") {
        let dev = Path::new("/dev/kvm");
        if !dev.exists() {
            return Some("/dev/kvm does not exist".into());
        }
        if let Err(e) = std::fs::OpenOptions::new().read(true).write(true).open(dev) {
            return Some(format!("cannot open /dev/kvm: {e}"));
        }
    }
    None
}

fn remediation(accel: &str, reason: &str) -> Option<String> {
    let r = reason.to_lowercase();
    Some(match accel {
        "kvm" if r.contains("permission") => {
            "Add your user to the 'kvm' group (sudo usermod -aG kvm $USER) and log in again.".into()
        }
        "kvm" => "Enable virtualisation (VT-x/AMD-V) in firmware settings and load the kvm module.".into(),
        "whpx" => "Enable the Windows Hypervisor Platform optional feature (Turn Windows features on or off → \
                   Windows Hypervisor Platform) and reboot. Older VMware/VirtualBox releases may conflict."
            .into(),
        "hvf" => "Hypervisor.framework requires a signed QEMU with the hypervisor entitlement.".into(),
        _ => return None,
    })
}

/// `device-list-properties` for one device type (lazy, cached by the caller).
pub async fn device_properties(path: &Path, device: &str) -> Result<Vec<DeviceProperty>> {
    let (qmp, child) = spawn_qmp(path, &["-machine", "none"]).await?;
    let res = qmp
        .execute(
            "device-list-properties",
            Some(json!({ "typename": device })),
        )
        .await;
    quit(&qmp, child).await;
    // QMP's shape (`default-value`) differs from our API's snake_case.
    #[derive(serde::Deserialize)]
    struct Raw {
        name: String,
        #[serde(rename = "type")]
        type_name: String,
        description: Option<String>,
        #[serde(rename = "default-value")]
        default_value: Option<Value>,
    }
    let raw: Vec<Raw> = serde_json::from_value(res?)?;
    let mut props: Vec<DeviceProperty> = raw
        .into_iter()
        .map(|r| DeviceProperty {
            name: r.name,
            type_name: r.type_name,
            description: r.description,
            default_value: r.default_value,
        })
        .collect();
    props.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(props)
}

/// `qemu-img --version` → `11.1.0`.
pub async fn qemu_img_version(path: &Path) -> Result<String> {
    let o = tokio::time::timeout(HELP_TIMEOUT, command(path).arg("--version").output()).await??;
    let s = String::from_utf8_lossy(&o.stdout);
    s.split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)
        .map(String::from)
        .ok_or_else(|| anyhow!("unexpected qemu-img --version output"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_ordering() {
        let mut v = vec!["pc-q35-9.2", "pc-q35-10.0", "pc-q35-9.10", "none"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["none", "pc-q35-9.2", "pc-q35-9.10", "pc-q35-10.0"]);
    }

    #[test]
    fn tidies_errors() {
        assert_eq!(
            tidy(
                "C:\\qemu\\qemu-system-x86_64.exe: -accel whpx: WHPX: No accelerator found, hr=00000000"
            ),
            "-accel whpx: WHPX: No accelerator found, hr=00000000"
        );
        assert_eq!(
            tidy("qemu-system-x86_64: warning: x\nqemu-system-x86_64: real error"),
            "real error"
        );
    }
}

/// Probe smoke test against a real QEMU, if one is installed (CI installs one
/// on both Windows and Linux). Passes vacuously otherwise.
#[cfg(test)]
mod smoke {
    use super::*;
    use std::path::PathBuf;

    fn find(exe: &str) -> Option<PathBuf> {
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

    #[tokio::test]
    async fn probes_x86_64() {
        let Some(path) = find("qemu-system-x86_64") else {
            eprintln!("skipping: qemu-system-x86_64 not found");
            return;
        };
        let host = crate::scan::host_info();
        let binary = Binary {
            id: "test".into(),
            path: path.display().to_string(),
            arch: Some("x86_64".into()),
            version: String::new(),
            sha256: String::new(),
            source: "path".into(),
        };
        let caps = probe_binary(binary, &host).await.unwrap();
        assert!(caps.machines.iter().any(|m| m.name.starts_with("pc-q35-")));
        assert!(caps.machines.iter().any(|m| m.is_default));
        assert!(
            caps.devices
                .iter()
                .any(|d| d.name == "virtio-blk-pci" && d.bus.as_deref() == Some("PCI"))
        );
        assert!(caps.cpus.iter().any(|c| c.name == "qemu64"));
        assert!(caps.accels.iter().any(|a| a.name == "tcg" && a.available));
        assert!(
            caps.backends
                .netdev
                .as_ref()
                .unwrap()
                .contains(&"user".to_string())
        );
        assert!(!caps.qmp_schema_hash.is_empty());
        assert!(caps.warnings.is_empty(), "{:?}", caps.warnings);

        let props = device_properties(&path, "virtio-blk-pci").await.unwrap();
        assert!(props.iter().any(|p| p.name == "drive"));
    }
}
