//! `qemu-ui export` / `qemu-ui apply` (M7-05): the HTTP side of
//! `qemu_ui_core::manifest`.

use anyhow::{Context, Result, anyhow};
use qemu_ui_core::manifest::{self, Change, Current, Manifest, Plan};
use qemu_ui_core::network::Network;
use qemu_ui_core::spec::VmSpec;
use serde_json::{Value, json};

use crate::client::Client;

pub fn current(c: &Client) -> Result<Current> {
    let networks: Vec<Network> = c
        .get("/networks")?
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| serde_json::from_value(n.clone()))
        .collect::<Result<_, _>>()
        .context("reading networks")?;
    let mut vms = Vec::new();
    for v in c.get("/vms")?.as_array().into_iter().flatten() {
        let id = v["id"].as_str().context("VM without id")?.to_string();
        let d = c.get(&format!("/vms/{id}"))?;
        let spec: VmSpec = serde_json::from_value(d["spec"].clone())
            .with_context(|| format!("reading the spec of VM {id}"))?;
        vms.push((id, spec));
    }
    Ok(Current { networks, vms })
}

pub fn export(c: &Client, toml: bool) -> Result<String> {
    let m = manifest::export(&current(c)?);
    if toml {
        // TOML has no null: drop nulls (they mean "unset" in a manifest).
        let v = strip_nulls(serde_json::to_value(&m)?);
        toml::to_string_pretty(&v).context("writing TOML")
    } else {
        Ok(serde_json::to_string_pretty(&m)? + "\n")
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        v => v,
    }
}

pub fn load(path: &std::path::Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let is_toml = path.extension().is_some_and(|e| e == "toml");
    manifest::parse(&text, is_toml).map_err(|e| anyhow!("{}: {e}", path.display()))
}

pub fn plan(c: &Client, m: &Manifest, prune: bool) -> Result<Plan> {
    manifest::plan(&current(c)?, m, prune).map_err(|e| anyhow!(e))
}

/// Run the plan. Stops at the first failure; what ran is printed.
pub fn execute(c: &Client, plan: &Plan) -> Result<()> {
    // name → id for networks, filled as networks are created.
    let mut ids: Vec<(String, String)> = c
        .get("/networks")?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            Some((
                n["name"].as_str()?.to_string(),
                n["id"].as_str()?.to_string(),
            ))
        })
        .collect();
    for ch in &plan.changes {
        let what = ch.describe();
        let r: Result<()> = (|| {
            match ch {
                Change::CreateNetwork(e) => {
                    let n = c.post(
                        "/networks",
                        Some(&json!({"name": e.name, "config": e.config})),
                    )?;
                    let id = n["id"].as_str().context("no id in reply")?.to_string();
                    ids.push((e.name.clone(), id));
                }
                Change::UpdateNetwork { id, entry } => {
                    c.put(
                        &format!("/networks/{id}"),
                        &json!({"name": entry.name, "config": entry.config}),
                    )?;
                }
                Change::DeleteNetwork { id, .. } => {
                    c.delete(&format!("/networks/{id}"))?;
                }
                Change::CreateVm { name, arch, spec } => {
                    let defaults = c.get(&format!(
                        "/vms/defaults?arch={}&name={}",
                        enc(arch),
                        enc(name)
                    ))?;
                    let base = defaults.get("spec").unwrap_or(&defaults);
                    let mut s = manifest::overlay(base, spec);
                    manifest::resolve_networks(&mut s, &ids).map_err(|e| anyhow!(e))?;
                    c.post("/vms", Some(&json!({ "spec": s })))?;
                }
                Change::UpdateVm { id, spec, .. } => {
                    let mut s = spec.clone();
                    manifest::resolve_networks(&mut s, &ids).map_err(|e| anyhow!(e))?;
                    c.put(&format!("/vms/{id}"), &json!({ "spec": s }))?;
                }
                Change::DeleteVm { id, .. } => {
                    c.delete(&format!("/vms/{id}"))?;
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => println!("{what}"),
            Err(e) => return Err(e.context(format!("{what} failed"))),
        }
    }
    Ok(())
}

/// Percent-encode a query value (names are `[A-Za-z0-9._-]`, arches too, but
/// be safe).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_strips() {
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
        assert_eq!(
            strip_nulls(json!({"a": null, "b": [{"c": null, "d": 1}]})),
            json!({"b": [{"d": 1}]})
        );
    }
}
