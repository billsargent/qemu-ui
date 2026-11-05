//! QEMU firmware interop descriptors (`docs/interop/firmware.json` in QEMU).
//!
//! Distros install them in `share/qemu/firmware/`, the upstream Windows build
//! in `share/firmware/`. Descriptors often carry build-time absolute paths
//! (e.g. `/usr/local/share//edk2-x86_64-code.fd`) that do not exist on the
//! host, so file references are resolved by the caller: first as given, then
//! by file name next to the descriptor's share directory.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct FirmwareTarget {
    pub architecture: String,
    /// Glob patterns, e.g. `pc-q35-*`.
    pub machines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct FirmwareFile {
    /// Path as written in the descriptor.
    pub declared: String,
    /// Resolved path on this host, if found.
    pub resolved: Option<String>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct Firmware {
    /// Descriptor file stem, e.g. `50-edk2-x86_64-secure`. Used as the spec's firmware id.
    pub id: String,
    pub description: String,
    /// `uefi`, `bios`, `openfirmware`, `uboot`, …
    pub interface_types: Vec<String>,
    /// `flash`, `memory`, `kernel`.
    pub device: String,
    pub executable: Option<FirmwareFile>,
    pub nvram_template: Option<FirmwareFile>,
    pub targets: Vec<FirmwareTarget>,
    pub features: Vec<String>,
    /// True when every referenced file was found.
    pub complete: bool,
}

impl Firmware {
    pub fn secure_boot(&self) -> bool {
        self.features.iter().any(|f| f == "secure-boot")
    }

    /// Does this firmware support `arch` + `machine`? Supports `*` globs.
    pub fn supports(&self, arch: &str, machine: &str) -> bool {
        self.targets
            .iter()
            .any(|t| t.architecture == arch && t.machines.iter().any(|g| glob_match(g, machine)))
    }
}

/// Minimal glob: `*` matches any run of characters. Enough for descriptor patterns.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || !s.ends_with(last) || s.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[derive(Debug, thiserror::Error)]
pub enum FirmwareError {
    #[error("invalid firmware descriptor: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawDescriptor {
    #[serde(default)]
    description: String,
    #[serde(default)]
    interface_types: Vec<String>,
    mapping: RawMapping,
    #[serde(default)]
    targets: Vec<FirmwareTarget>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawMapping {
    device: String,
    executable: Option<RawFile>,
    nvram_template: Option<RawFile>,
    // `memory` mappings use `filename` directly.
    filename: Option<String>,
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    format: Option<String>,
}

/// Parse a descriptor. `resolve` maps a declared path to an existing host path.
pub fn parse_descriptor(
    id: &str,
    json: &str,
    resolve: impl Fn(&str) -> Option<String>,
) -> Result<Firmware, FirmwareError> {
    let raw: RawDescriptor = serde_json::from_str(json)?;
    let file = |f: RawFile| FirmwareFile {
        resolved: resolve(&f.filename),
        declared: f.filename,
        format: f.format,
    };
    let executable = raw
        .mapping
        .executable
        .map(file)
        .or(raw.mapping.filename.map(|filename| {
            file(RawFile {
                filename,
                format: None,
            })
        }));
    let nvram_template = raw.mapping.nvram_template.map(file);
    let complete = executable.as_ref().is_none_or(|f| f.resolved.is_some())
        && nvram_template.as_ref().is_none_or(|f| f.resolved.is_some());
    Ok(Firmware {
        id: id.to_string(),
        description: raw.description,
        interface_types: raw.interface_types,
        device: raw.mapping.device,
        executable,
        nvram_template,
        targets: raw.targets,
        features: raw.features,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const X86_SECURE: &str = include_str!("../tests/fixtures/50-edk2-x86_64-secure.json");
    const AARCH64: &str = include_str!("../tests/fixtures/60-edk2-aarch64.json");

    #[test]
    fn parses_secure_boot_descriptor() {
        let fw = parse_descriptor("50-edk2-x86_64-secure", X86_SECURE, |p| {
            p.ends_with("code.fd")
                .then(|| format!("/share/{}", p.rsplit('/').next().unwrap()))
        })
        .unwrap();
        assert!(fw.secure_boot());
        assert!(fw.features.contains(&"requires-smm".into()));
        assert!(fw.supports("x86_64", "pc-q35-9.1"));
        assert!(!fw.supports("x86_64", "pc-i440fx-9.1"));
        assert_eq!(
            fw.executable.unwrap().resolved.as_deref(),
            Some("/share/edk2-x86_64-secure-code.fd")
        );
        assert!(!fw.complete, "vars template was not resolvable");
    }

    #[test]
    fn parses_aarch64() {
        let fw = parse_descriptor("60-edk2-aarch64", AARCH64, |p| Some(p.into())).unwrap();
        assert!(fw.complete);
        assert!(fw.interface_types.contains(&"uefi".into()));
        assert!(fw.targets.iter().any(|t| t.architecture == "aarch64"));
    }

    #[test]
    fn globs() {
        assert!(glob_match("pc-q35-*", "pc-q35-9.1"));
        assert!(glob_match("virt-*", "virt-11.1"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("pc-q35-*", "pc-i440fx-9.1"));
        assert!(glob_match("virt", "virt"));
    }
}
