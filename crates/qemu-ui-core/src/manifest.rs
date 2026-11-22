//! Config-as-code manifests (plan.md §12, M7-05).
//!
//! A manifest declares networks and VMs by **name**. `qemu-ui export` writes
//! the daemon's state as one; `qemu-ui apply` computes a [`Plan`] that makes
//! the daemon match it, and running the same apply twice changes nothing.
//!
//! Rules:
//! - NICs name their network (`"network": "lab"`), not its id, so a manifest
//!   moves between hosts. Ids are resolved when the plan is applied.
//! - A manifest may be partial. Fields it leaves out (or sets to `null`)
//!   keep their current value, or the daemon's default for a new VM; so
//!   daemon-assigned values such as MAC addresses never show up as drift.
//! - Things not in the manifest are left alone unless pruning is asked for.
//! - Disk images are referenced, not created: a VM's disks must already exist.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::network::{Network, NetworkConfig};
use crate::spec::{NicBackend, VmSpec};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub manifest_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<NetworkEntry>,
    /// VM specs, possibly partial; each needs `name` (and `arch` to create).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vms: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkEntry {
    pub name: String,
    pub config: NetworkConfig,
}

/// What the daemon has now.
#[derive(Debug, Clone, Default)]
pub struct Current {
    pub networks: Vec<Network>,
    /// `(id, spec)` per VM.
    pub vms: Vec<(String, VmSpec)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    CreateNetwork(NetworkEntry),
    UpdateNetwork {
        id: String,
        entry: NetworkEntry,
    },
    DeleteNetwork {
        id: String,
        name: String,
    },
    /// `spec` is the full desired spec with network *names* in NICs; the
    /// applier overlays it on the daemon's defaults for `arch`.
    CreateVm {
        name: String,
        arch: String,
        spec: Value,
    },
    /// `spec` is the full new spec (network names), `fields` the top-level
    /// fields that change.
    UpdateVm {
        id: String,
        name: String,
        spec: Value,
        fields: Vec<String>,
    },
    DeleteVm {
        id: String,
        name: String,
    },
}

impl Change {
    /// One line for `apply --dry-run`.
    pub fn describe(&self) -> String {
        match self {
            Change::CreateNetwork(e) => format!("+ network {}", e.name),
            Change::UpdateNetwork { entry, .. } => format!("~ network {}", entry.name),
            Change::DeleteNetwork { name, .. } => format!("- network {name}"),
            Change::CreateVm { name, .. } => format!("+ vm {name}"),
            Change::UpdateVm { name, fields, .. } => {
                format!("~ vm {name} ({})", fields.join(", "))
            }
            Change::DeleteVm { name, .. } => format!("- vm {name}"),
        }
    }
}

/// Changes in the order they must run: networks are created before the VMs
/// that use them, and VMs deleted before the networks they used.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub changes: Vec<Change>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Swap every NIC's network reference through `map`; `None` from the map is
/// an error naming the missing reference.
fn map_networks(spec: &mut Value, map: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    let Some(nics) = spec.get_mut("nics").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for nic in nics {
        let Some(backend) = nic.get_mut("backend") else {
            continue;
        };
        if backend.get("kind").and_then(Value::as_str) != Some("network") {
            continue;
        }
        let Some(r) = backend.get("network").and_then(Value::as_str) else {
            continue;
        };
        let to = map(r).ok_or_else(|| format!("no network named '{r}'"))?;
        backend["network"] = Value::String(to);
    }
    Ok(())
}

/// The spec as JSON with network ids replaced by names.
fn spec_by_name(spec: &VmSpec, networks: &[Network]) -> Value {
    let mut s = spec.clone();
    for nic in &mut s.nics {
        if let NicBackend::Network { network, .. } = &mut nic.backend
            && let Some(n) = networks.iter().find(|n| &n.id == network)
        {
            *network = n.name.clone();
        }
    }
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// Replace network names with ids; `ids` maps name → id.
pub fn resolve_networks(spec: &mut Value, ids: &[(String, String)]) -> Result<(), String> {
    map_networks(spec, &|name| {
        ids.iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| id.clone())
    })
}

/// `desired` laid over `base`: objects merge key by key, `null` (or a
/// missing key) keeps the base value, arrays of the same length merge
/// element-wise, anything else is replaced.
pub fn overlay(base: &Value, desired: &Value) -> Value {
    match (base, desired) {
        (_, Value::Null) => base.clone(),
        (Value::Object(b), Value::Object(d)) => {
            let mut out: Map<String, Value> = b.clone();
            for (k, v) in d {
                let merged = match b.get(k) {
                    Some(bv) => overlay(bv, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (Value::Array(b), Value::Array(d)) if b.len() == d.len() => {
            Value::Array(b.iter().zip(d).map(|(b, d)| overlay(b, d)).collect())
        }
        _ => desired.clone(),
    }
}

/// Export the current state as a manifest (networks and VMs sorted by name).
pub fn export(cur: &Current) -> Manifest {
    let mut networks: Vec<NetworkEntry> = cur
        .networks
        .iter()
        .map(|n| NetworkEntry {
            name: n.name.clone(),
            config: n.config.clone(),
        })
        .collect();
    networks.sort_by(|a, b| a.name.cmp(&b.name));
    let mut vms: Vec<Value> = cur
        .vms
        .iter()
        .map(|(_, s)| spec_by_name(s, &cur.networks))
        .collect();
    vms.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Manifest {
        manifest_version: MANIFEST_VERSION,
        networks,
        vms,
    }
}

/// What to change so the daemon matches `want`. With `prune`, networks and
/// VMs missing from the manifest are deleted.
pub fn plan(cur: &Current, want: &Manifest, prune: bool) -> Result<Plan, String> {
    if want.manifest_version != MANIFEST_VERSION {
        return Err(format!(
            "manifest_version {} is not supported (expected {MANIFEST_VERSION})",
            want.manifest_version
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for n in &want.networks {
        if !seen.insert(("net", n.name.clone())) {
            return Err(format!("network '{}' is declared twice", n.name));
        }
    }
    let mut changes = Vec::new();
    for n in &want.networks {
        match cur.networks.iter().find(|c| c.name == n.name) {
            None => changes.push(Change::CreateNetwork(n.clone())),
            Some(c) if c.config != n.config => changes.push(Change::UpdateNetwork {
                id: c.id.clone(),
                entry: n.clone(),
            }),
            Some(_) => {}
        }
    }

    // Network names a VM may use: current ones plus those being created.
    let known: Vec<&str> = cur
        .networks
        .iter()
        .map(|n| n.name.as_str())
        .chain(want.networks.iter().map(|n| n.name.as_str()))
        .collect();
    for v in &want.vms {
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .ok_or("every VM in the manifest needs a \"name\"")?
            .to_string();
        if !v.is_object() {
            return Err(format!("VM '{name}' is not an object"));
        }
        if !seen.insert(("vm", name.clone())) {
            return Err(format!("VM '{name}' is declared twice"));
        }
        let mut probe = v.clone();
        map_networks(&mut probe, &|r| known.contains(&r).then(|| r.to_string()))
            .map_err(|e| format!("VM '{name}': {e}"))?;
        match cur.vms.iter().find(|(_, s)| s.name == name) {
            None => {
                let arch = v
                    .get("arch")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("VM '{name}' is new and needs an \"arch\""))?
                    .to_string();
                changes.push(Change::CreateVm {
                    name,
                    arch,
                    spec: v.clone(),
                });
            }
            Some((id, spec)) => {
                let now = spec_by_name(spec, &cur.networks);
                let next = overlay(&now, v);
                if next != now {
                    let fields = changed_fields(&now, &next);
                    changes.push(Change::UpdateVm {
                        id: id.clone(),
                        name,
                        spec: next,
                        fields,
                    });
                }
            }
        }
    }

    if prune {
        for (id, s) in &cur.vms {
            if !want.vms.iter().any(|v| v["name"] == s.name.as_str()) {
                changes.push(Change::DeleteVm {
                    id: id.clone(),
                    name: s.name.clone(),
                });
            }
        }
        for n in &cur.networks {
            if !want.networks.iter().any(|w| w.name == n.name) {
                changes.push(Change::DeleteNetwork {
                    id: n.id.clone(),
                    name: n.name.clone(),
                });
            }
        }
    }
    Ok(Plan { changes })
}

fn changed_fields(a: &Value, b: &Value) -> Vec<String> {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return vec![];
    };
    let mut f: Vec<String> = b
        .iter()
        .filter(|(k, v)| a.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .collect();
    f.sort();
    f
}

/// Parse a manifest from JSON or TOML (`is_toml`).
pub fn parse(text: &str, is_toml: bool) -> Result<Manifest, String> {
    if is_toml {
        let v: Value = toml::from_str(text).map_err(|e| e.to_string())?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    } else {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::NicSpec;
    use serde_json::json;

    fn spec(name: &str, net: Option<&str>) -> VmSpec {
        let mut s: VmSpec = serde_json::from_value(json!({
            "spec_version": 1, "name": name, "arch": "x86_64",
            "machine": {"type": "pc-q35-9.2"}, "accel": "auto",
            "cpu": {"model": "max", "count": 1},
            "memory": {"size_mib": 512}
        }))
        .unwrap_or_else(|e| panic!("fixture: {e}"));
        if let Some(n) = net {
            s.nics.push(NicSpec {
                backend: NicBackend::Network {
                    network: n.into(),
                    forwards: vec![],
                },
                model: "virtio-net-pci".into(),
                mac: Some("52:54:00:00:00:01".into()),
                queues: None,
                virtio: None,
            });
        }
        s
    }

    fn current() -> Current {
        Current {
            networks: vec![Network {
                id: "n1".into(),
                name: "lab".into(),
                config: NetworkConfig::Nat { subnet: None },
            }],
            vms: vec![("v1".into(), spec("web", Some("n1")))],
        }
    }

    #[test]
    fn export_then_plan_is_empty_and_uses_names() {
        let cur = current();
        let m = export(&cur);
        assert_eq!(m.vms[0]["nics"][0]["backend"]["network"], "lab");
        assert!(plan(&cur, &m, true).unwrap().is_empty());
        // Round trip through text.
        let text = serde_json::to_string_pretty(&m).unwrap();
        assert!(
            plan(&cur, &parse(&text, false).unwrap(), true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn partial_specs_only_change_what_they_name() {
        let cur = current();
        let m = Manifest {
            manifest_version: 1,
            networks: vec![],
            vms: vec![json!({"name": "web", "memory": {"size_mib": 1024}})],
        };
        let p = plan(&cur, &m, false).unwrap();
        let [Change::UpdateVm { spec, fields, .. }] = p.changes.as_slice() else {
            panic!("{p:?}")
        };
        assert_eq!(fields, &["memory"]);
        assert_eq!(spec["memory"]["size_mib"], 1024);
        // The daemon-assigned MAC and the network survive.
        assert_eq!(spec["nics"][0]["mac"], "52:54:00:00:00:01");
        assert_eq!(spec["nics"][0]["backend"]["network"], "lab");
        // Nothing is deleted without prune.
        let m = Manifest {
            manifest_version: 1,
            networks: vec![],
            vms: vec![json!({"name": "web"})],
        };
        assert!(plan(&cur, &m, false).unwrap().is_empty());
        let pruned = plan(&cur, &m, true).unwrap();
        assert_eq!(
            pruned.changes,
            [Change::DeleteNetwork {
                id: "n1".into(),
                name: "lab".into()
            }]
        );
    }

    #[test]
    fn creates_in_order_and_resolves_names() {
        let m = Manifest {
            manifest_version: 1,
            networks: vec![NetworkEntry {
                name: "lab".into(),
                config: NetworkConfig::Nat { subnet: None },
            }],
            vms: vec![json!({"name": "db", "arch": "x86_64",
                "nics": [{"backend": {"kind": "network", "network": "lab"}, "model": "virtio-net-pci"}]})],
        };
        let p = plan(&Current::default(), &m, false).unwrap();
        let d: Vec<String> = p.changes.iter().map(Change::describe).collect();
        assert_eq!(d, ["+ network lab", "+ vm db"]);
        let Change::CreateVm { spec, .. } = &p.changes[1] else {
            panic!()
        };
        let mut s = spec.clone();
        resolve_networks(&mut s, &[("lab".into(), "id9".into())]).unwrap();
        assert_eq!(s["nics"][0]["backend"]["network"], "id9");
    }

    #[test]
    fn errors_are_specific() {
        let cur = current();
        let m = |vms| Manifest {
            manifest_version: 1,
            networks: vec![],
            vms,
        };
        let e = |m: Manifest| plan(&cur, &m, false).unwrap_err();
        assert!(e(m(vec![json!({"arch": "x86_64"})])).contains("name"));
        assert!(e(m(vec![json!({"name": "new"})])).contains("arch"));
        assert!(
            e(m(vec![
                json!({"name": "a", "arch": "x86_64"}),
                json!({"name": "a", "arch": "x86_64"})
            ]))
            .contains("twice")
        );
        assert!(e(m(vec![json!({"name": "web", "nics": [{"backend": {"kind": "network", "network": "nope"}}]})])).contains("nope"));
        let mut bad = m(vec![]);
        bad.manifest_version = 2;
        assert!(e(bad).contains("manifest_version"));
    }

    #[test]
    fn toml_manifests_parse() {
        let t = r#"
manifest_version = 1
[[networks]]
name = "lab"
config = { kind = "nat" }
[[vms]]
name = "web"
memory = { size_mib = 2048 }
"#;
        let m = parse(t, true).unwrap();
        assert_eq!(m.networks[0].name, "lab");
        assert_eq!(m.vms[0]["memory"]["size_mib"], 2048);
    }

    #[test]
    fn overlay_rules() {
        let b = json!({"a": 1, "o": {"x": 1, "y": 2}, "l": [1, 2], "k": [{"m": 1, "n": 2}]});
        let d = json!({"a": null, "o": {"y": 3}, "l": [5], "k": [{"n": 9}]});
        assert_eq!(
            overlay(&b, &d),
            json!({"a": 1, "o": {"x": 1, "y": 3}, "l": [5], "k": [{"m": 1, "n": 9}]})
        );
    }
}
