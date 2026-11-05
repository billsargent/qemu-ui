//! Parsers for QEMU `-<option> help` output.
//!
//! These are fallbacks for information QMP does not expose (backend lists) and
//! enrichment for information it exposes only partially (device bus, category
//! and description; machine descriptions). Input may use CRLF line endings.

use crate::caps::{DeviceInfo, MachineInfo};

/// Parse a simple list such as `-netdev help`, `-display help`, `-accel help`,
/// `-audiodev help`: a header line ending in `:` followed by one name per line,
/// terminated by a blank line or end of input.
pub fn parse_name_list(text: &str) -> Vec<String> {
    let mut lines = text.lines().map(str::trim_end);
    // Skip to the header line.
    for line in lines.by_ref() {
        if line.trim_end().ends_with(':') {
            break;
        }
    }
    let mut out = Vec::new();
    for line in lines {
        let name = line.trim();
        if name.is_empty() {
            break;
        }
        out.push(name.to_string());
    }
    out
}

/// True when QEMU rejected the option itself, i.e. the feature is not compiled
/// into this binary (for example `-tpmdev` on Windows builds).
pub fn is_invalid_option(stderr: &str) -> bool {
    stderr.contains("invalid option")
}

/// Parse `-device help`.
///
/// ```text
/// Storage devices:
/// name "virtio-blk-pci", bus PCI, alias "virtio-blk", desc "Virtio block device"
/// ```
pub fn parse_device_help(text: &str) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    let mut category = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cat) = line.strip_suffix(" devices:") {
            category = cat.to_string();
            continue;
        }
        let Some(rest) = line.strip_prefix("name ") else {
            continue;
        };
        let fields = split_fields(rest);
        let Some(name) = fields.first().map(|(_, v)| v.clone()) else {
            continue;
        };
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        out.push(DeviceInfo {
            name,
            category: category.clone(),
            bus: get("bus"),
            alias: get("alias"),
            description: get("desc"),
            no_user: line.contains("no-user"),
        });
    }
    out
}

/// Split `"virtio-blk-pci", bus PCI, alias "virtio-blk", desc "a, b"` into
/// `[("", "virtio-blk-pci"), ("bus", "PCI"), ("alias", "virtio-blk"), ("desc", "a, b")]`,
/// respecting quotes.
fn split_fields(s: &str) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| {
            let p = p.trim();
            match p.split_once(' ') {
                Some((k, v)) if !p.starts_with('"') && k != p => {
                    (k.to_string(), v.trim().to_string())
                }
                _ => (String::new(), p.to_string()),
            }
        })
        .map(|(k, v)| (k, v.trim_matches('"').to_string()))
        .collect()
}

/// Parse `-machine help` into name/description/alias/default/deprecated.
///
/// ```text
/// pc                   Standard PC (i440FX + PIIX, 1996) (alias of pc-i440fx-11.1)
/// pc-i440fx-11.1       Standard PC (i440FX + PIIX, 1996) (default)
/// pc-q35-8.1           Standard PC (Q35 + ICH9, 2009) (deprecated)
/// ```
/// Alias lines are folded into the machine they point at.
pub fn parse_machine_help(text: &str) -> Vec<MachineInfo> {
    let mut machines: Vec<MachineInfo> = Vec::new();
    let mut aliases: Vec<(String, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.ends_with(':') {
            continue;
        }
        let Some((name, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let mut desc = rest.trim().to_string();
        if let Some(idx) = desc.find(" (alias of ") {
            let target = desc[idx + " (alias of ".len()..]
                .trim_end_matches(')')
                .to_string();
            aliases.push((name.to_string(), target));
            continue;
        }
        let mut is_default = false;
        let mut deprecated = false;
        loop {
            if let Some(d) = desc.strip_suffix(" (default)") {
                is_default = true;
                desc = d.to_string();
            } else if let Some(d) = desc.strip_suffix(" (deprecated)") {
                deprecated = true;
                desc = d.to_string();
            } else {
                break;
            }
        }
        machines.push(MachineInfo {
            name: name.to_string(),
            description: desc,
            alias: None,
            is_default,
            deprecated,
            cpu_max: None,
            default_cpu_type: None,
            hotpluggable_cpus: None,
            numa_mem_supported: None,
        });
    }
    for (alias, target) in aliases {
        if let Some(m) = machines.iter_mut().find(|m| m.name == target) {
            m.alias = Some(alias);
        }
    }
    machines
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = include_str!("../tests/fixtures/x86_64-device-help.txt");
    const MACHINE: &str = include_str!("../tests/fixtures/x86_64-machine-help.txt");
    const DISPLAY: &str = include_str!("../tests/fixtures/x86_64-display-help.txt");
    const NETDEV: &str = include_str!("../tests/fixtures/x86_64-netdev-help.txt");
    const ACCEL: &str = include_str!("../tests/fixtures/x86_64-accel-help.txt");

    #[test]
    fn name_lists() {
        assert_eq!(parse_name_list(ACCEL), ["tcg", "whpx"]);
        let nd = parse_name_list(NETDEV);
        assert!(nd.contains(&"user".to_string()) && nd.contains(&"dgram".to_string()));
        let disp = parse_name_list(DISPLAY);
        assert!(disp.contains(&"none".to_string()));
        assert!(
            !disp.iter().any(|d| d.contains(' ')),
            "prose leaked: {disp:?}"
        );
    }

    #[test]
    fn devices() {
        let devs = parse_device_help(DEVICE);
        assert!(devs.len() > 300);
        let vblk = devs.iter().find(|d| d.name == "virtio-blk-pci").unwrap();
        assert_eq!(vblk.category, "Storage");
        assert_eq!(vblk.bus.as_deref(), Some("PCI"));
        assert_eq!(vblk.alias.as_deref(), Some("virtio-blk"));
        assert!(devs.iter().all(|d| !d.name.contains('"')));
    }

    #[test]
    fn desc_with_commas() {
        let d = parse_device_help("Misc devices:\nname \"x\", bus ISA, desc \"a, b (c)\"\n");
        assert_eq!(d[0].description.as_deref(), Some("a, b (c)"));
    }

    #[test]
    fn machines() {
        let ms = parse_machine_help(MACHINE);
        let pc = ms
            .iter()
            .find(|m| m.alias.as_deref() == Some("pc"))
            .unwrap();
        assert!(pc.name.starts_with("pc-i440fx-"));
        assert!(pc.is_default);
        assert!(ms.iter().any(|m| m.deprecated));
        assert!(
            ms.iter()
                .all(|m| !m.description.ends_with(')') || m.description.contains('('))
        );
    }

    #[test]
    fn invalid_option() {
        assert!(is_invalid_option(
            "qemu-system-x86_64.exe: -tpmdev: invalid option"
        ));
    }
}
