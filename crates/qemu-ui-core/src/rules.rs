//! L2 curated semantic rules (plan.md §6.2, M4-03/04).
//!
//! Rules are data: `data/rules/*.toml`, embedded at build time and
//! overridable from a directory. Each rule matches a small, typed condition
//! language against the spec plus a few derived *facts* (resolved
//! accelerator, machine limits, firmware features, …), produces a warning by
//! default, and may carry a declarative auto-fix. Every rule ships its own
//! test cases; [`RuleSet::self_test`] runs them (and checks that each
//! auto-fix makes its rule stop firing).
//!
//! Paths are dotted, rooted at `spec.` or `facts.`; `[*]` matches every
//! element of an array (a condition then holds if it holds for any element),
//! `[n]` one element. A rule's `field` and its auto-fix paths may use `[*]`,
//! which is replaced by the indices of the element that matched.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use ts_rs::TS;

use crate::caps::BinaryCaps;
use crate::compile::{Diagnostic, Severity};
use crate::firmware::Firmware;
use crate::spec::{FirmwareSpec, VmSpec};

/// The rule files shipped with qemu-ui.
const BUILTIN: &[(&str, &str)] = &[
    ("cpu.toml", include_str!("../../../data/rules/cpu.toml")),
    (
        "devices.toml",
        include_str!("../../../data/rules/devices.toml"),
    ),
    (
        "firmware.toml",
        include_str!("../../../data/rules/firmware.toml"),
    ),
    (
        "guests.toml",
        include_str!("../../../data/rules/guests.toml"),
    ),
    (
        "storage.toml",
        include_str!("../../../data/rules/storage.toml"),
    ),
];

/// One `data/rules/*.toml` file.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum RuleSeverity {
    #[default]
    Warn,
    /// Only with a `citation` and a `lint_reproduction`.
    Error,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable id, `l2.<area>.<name>`.
    pub id: String,
    #[serde(default)]
    pub severity: RuleSeverity,
    /// QEMU versions the rule was checked against, e.g. `["11.1"]`.
    pub verified_against: Vec<String>,
    /// Where the behaviour is documented (required for `error`).
    #[serde(default)]
    pub citation: Option<String>,
    /// How to reproduce the failure with "Check config" (required for `error`).
    #[serde(default)]
    pub lint_reproduction: Option<String>,
    #[serde(default)]
    pub applies_to: AppliesTo,
    pub when: Cond,
    /// Spec field the finding is pinned to (no `spec.` prefix).
    pub field: String,
    pub message: String,
    #[serde(default)]
    pub docs: Option<String>,
    #[serde(default)]
    pub autofix: Option<Autofix>,
    /// Built-in (L1) diagnostics this rule is the curated counterpart of:
    /// when one of them is reported this rule stays quiet, and its
    /// auto-fix is offered for that diagnostic instead.
    #[serde(default)]
    pub covers: Vec<String>,
    #[serde(rename = "test", default)]
    pub tests: Vec<RuleTest>,
}

/// Shorthand filters, all optional; every given list must match.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppliesTo {
    /// `spec.arch` is one of these.
    #[serde(default)]
    pub arch: Vec<String>,
    /// `spec.machine.type` starts with one of these.
    #[serde(default)]
    pub machine: Vec<String>,
    /// The resolved accelerator is one of these.
    #[serde(default)]
    pub accel: Vec<String>,
    /// The host OS (`linux`, `windows`, `macos`) is one of these.
    #[serde(default)]
    pub host_os: Vec<String>,
}

/// A condition: exactly one of `all`/`any`/`not`/`path`; with `path`,
/// exactly one operator.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cond {
    #[serde(default)]
    pub all: Option<Vec<Cond>>,
    #[serde(default)]
    pub any: Option<Vec<Cond>>,
    #[serde(default)]
    pub not: Option<Box<Cond>>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub eq: Option<Value>,
    #[serde(default)]
    pub ne: Option<Value>,
    #[serde(rename = "in", default)]
    pub in_: Option<Vec<Value>>,
    #[serde(default)]
    pub not_in: Option<Vec<Value>>,
    #[serde(default)]
    pub starts_with: Option<String>,
    #[serde(default)]
    pub ends_with: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub gt: Option<f64>,
    #[serde(default)]
    pub ge: Option<f64>,
    #[serde(default)]
    pub lt: Option<f64>,
    #[serde(default)]
    pub le: Option<f64>,
    /// With `true`: some non-null value is at `path`; `false`: none is.
    #[serde(default)]
    pub exists: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Autofix {
    /// What the fix does, shown on its button.
    pub description: String,
    pub set: Vec<SetOp>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetOp {
    /// Spec path (no `spec.` prefix); may use `[*]`.
    pub path: String,
    /// The new value; leave it out to clear an optional field (TOML has
    /// no null).
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleTest {
    pub name: String,
    /// JSON merge patch applied to the harness's base spec.
    #[serde(default)]
    pub spec: Option<Value>,
    /// Facts to override (the rest are derived from the patched spec).
    #[serde(default)]
    pub facts: Option<Value>,
    /// Should the rule fire?
    pub fires: bool,
}

/// What a rule can see besides the spec. Derived by the daemon from the
/// binary's capabilities and the host; tests set them directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Facts {
    /// `linux`, `windows`, `macos`.
    pub host_os: String,
    /// The accelerator `auto` resolves to (or the one asked for).
    pub accel: Option<String>,
    /// Is that accelerator hardware-assisted?
    pub hardware_accel: bool,
    /// The machine's `cpu-max`.
    pub max_cpus: Option<u32>,
    /// sockets × cores × threads, when a topology is set.
    pub topology_product: Option<u32>,
    /// The topology asks for more vCPUs than the machine's `cpu-max`.
    pub cpus_over_max: bool,
    /// The firmware descriptor is UEFI / has Secure Boot.
    pub firmware_uefi: bool,
    pub firmware_secure_boot: bool,
    /// Hyper-V enlightenments are set: almost certainly a Windows guest.
    pub windows_guest: bool,
    /// This QEMU can attach an emulated TPM (`-tpmdev emulator`, M7-07).
    #[serde(default)]
    pub tpm_backend: bool,
}

impl Facts {
    /// Facts for `spec` on this binary and host.
    pub fn derive(
        spec: &VmSpec,
        caps: Option<&BinaryCaps>,
        host_os: &str,
        firmware: Option<&Firmware>,
    ) -> Facts {
        let accel = caps.and_then(|c| {
            if spec.accel == "auto" {
                c.best_accel()
            } else {
                c.accels.iter().find(|a| a.name == spec.accel)
            }
        });
        let fw = match &spec.firmware {
            FirmwareSpec::Descriptor { .. } => firmware,
            FirmwareSpec::Default => None,
        };
        let max_cpus = caps.and_then(|c| {
            c.machines
                .iter()
                .find(|m| {
                    m.name == spec.machine.type_name
                        || m.alias.as_deref() == Some(&spec.machine.type_name)
                })
                .and_then(|m| m.cpu_max)
        });
        let topology_product = spec.cpu.topology.map(|t| t.sockets * t.cores * t.threads);
        Facts {
            host_os: host_os.into(),
            accel: accel
                .map(|a| a.name.clone())
                .or_else(|| (spec.accel != "auto").then(|| spec.accel.clone())),
            hardware_accel: accel.is_some_and(|a| a.hardware),
            max_cpus,
            topology_product,
            cpus_over_max: false,
            firmware_uefi: fw.is_some_and(|f| f.interface_types.iter().any(|t| t == "uefi")),
            firmware_secure_boot: fw.is_some_and(Firmware::secure_boot),
            windows_guest: spec.cpu.props.keys().any(|k| k.starts_with("hv-")),
            tpm_backend: caps.is_some_and(|c| crate::compile::tpm_unsupported_reason(c).is_none()),
        }
        .settled()
    }

    /// Recompute the facts that follow from other facts.
    pub fn settled(mut self) -> Facts {
        self.cpus_over_max =
            matches!((self.topology_product, self.max_cpus), (Some(p), Some(m)) if p > m);
        self
    }
}

/// A rule as the UI sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct RuleInfo {
    pub id: String,
    pub severity: RuleSeverity,
    pub message: String,
    pub docs: Option<String>,
    /// The auto-fix's description, when the rule has one.
    pub autofix: Option<String>,
    /// Built-in diagnostics the auto-fix also applies to.
    pub covers: Vec<String>,
    pub verified_against: Vec<String>,
}

/// The result of applying an auto-fix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct FixResult {
    pub spec: VmSpec,
    /// Spec paths the fix changed, e.g. `display.device`.
    pub changed: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// The rules shipped with qemu-ui. Panics only if a shipped file is
    /// invalid, which the harness test catches.
    pub fn builtin() -> RuleSet {
        let mut rules = Vec::new();
        for (name, text) in BUILTIN {
            match parse_file(text) {
                Ok(f) => rules.extend(f.rules),
                Err(e) => panic!("built-in rule file {name}: {e}"),
            }
        }
        let set = RuleSet { rules };
        if let Err(e) = set.validate() {
            panic!("built-in rules: {e}");
        }
        set
    }

    /// Built-in rules plus `*.toml` files from `dir`; a rule there with an
    /// existing id replaces the built-in one.
    pub fn with_overrides(dir: &std::path::Path) -> Result<RuleSet, String> {
        let mut set = RuleSet::builtin();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(set);
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for p in paths {
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let f = parse_file(&text).map_err(|e| format!("{}: {e}", p.display()))?;
            for r in f.rules {
                set.rules.retain(|x| x.id != r.id);
                set.rules.push(r);
            }
        }
        set.validate()?;
        Ok(set)
    }

    /// Structural checks beyond the file format.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for r in &self.rules {
            let ctx = |m: &str| format!("rule {}: {m}", r.id);
            let id_ok = r.id.starts_with("l2.")
                && r.id.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-')
                });
            if !id_ok {
                return Err(ctx(
                    "ids look like `l2.area.name` (lowercase, digits, '.', '-')",
                ));
            }
            if !seen.insert(r.id.clone()) {
                return Err(ctx("duplicate id"));
            }
            if r.verified_against.is_empty() {
                return Err(ctx("`verified_against` lists no QEMU version"));
            }
            if r.severity == RuleSeverity::Error
                && (r.citation.is_none() || r.lint_reproduction.is_none())
            {
                return Err(ctx("`error` needs a `citation` and a `lint_reproduction`"));
            }
            check_cond(&r.when).map_err(|e| ctx(&e))?;
            if !r.tests.iter().any(|t| t.fires) || !r.tests.iter().any(|t| !t.fires) {
                return Err(ctx("needs at least one firing and one non-firing test"));
            }
            if let Some(f) = &r.autofix {
                if f.set.is_empty() {
                    return Err(ctx("autofix sets nothing"));
                }
                for s in &f.set {
                    if s.path.starts_with("spec.") || s.path.starts_with("facts.") {
                        return Err(ctx("autofix paths are spec paths without a prefix"));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    pub fn info(&self) -> Vec<RuleInfo> {
        self.rules
            .iter()
            .map(|r| RuleInfo {
                id: r.id.clone(),
                severity: r.severity,
                message: r.message.clone(),
                docs: r.docs.clone(),
                autofix: r.autofix.as_ref().map(|f| f.description.clone()),
                covers: r.covers.clone(),
                verified_against: r.verified_against.clone(),
            })
            .collect()
    }

    /// Every rule that fires for `spec`, as diagnostics.
    pub fn evaluate(&self, spec: &VmSpec, facts: &Facts) -> Vec<Diagnostic> {
        let ctx = context(spec, facts);
        self.rules
            .iter()
            .filter_map(|r| {
                let witness = r.fires(&ctx)?;
                Some(Diagnostic {
                    severity: match r.severity {
                        RuleSeverity::Warn => Severity::Warning,
                        RuleSeverity::Error => Severity::Error,
                    },
                    field: substitute(&r.field, &witness),
                    rule: r.id.clone(),
                    message: r.message.clone(),
                })
            })
            .collect()
    }

    /// Add L2 findings to `diags` (the built-in ones), leaving out rules
    /// whose `covers` already fired.
    pub fn extend(&self, diags: &mut Vec<Diagnostic>, spec: &VmSpec, facts: &Facts) {
        let fired: Vec<String> = diags.iter().map(|d| d.rule.clone()).collect();
        for d in self.evaluate(spec, facts) {
            let covered = self
                .get(&d.rule)
                .is_some_and(|r| r.covers.iter().any(|c| fired.contains(c)));
            if !covered {
                diags.push(d);
            }
        }
    }

    /// Apply the auto-fix of rule `id` (an L2 id, or a built-in id an L2
    /// rule `covers`) to `spec`. Fails when there is no fix or it does not
    /// apply.
    pub fn fix(&self, spec: &VmSpec, facts: &Facts, id: &str) -> Result<FixResult, String> {
        let rule = self
            .get(id)
            .or_else(|| {
                self.rules
                    .iter()
                    .find(|r| r.autofix.is_some() && r.covers.iter().any(|c| c == id))
            })
            .ok_or_else(|| format!("no automatic fix for '{id}'"))?;
        let id = rule.id.as_str();
        let fix = rule
            .autofix
            .as_ref()
            .ok_or_else(|| format!("rule '{id}' has no automatic fix"))?;
        let ctx = context(spec, facts);
        let witness = rule
            .fires(&ctx)
            .ok_or_else(|| format!("rule '{id}' does not apply to this spec"))?;
        let mut v = serde_json::to_value(spec).map_err(|e| e.to_string())?;
        let mut changed = Vec::new();
        for s in &fix.set {
            let path = substitute(&s.path, &witness);
            let value = s.value.clone().unwrap_or(Value::Null);
            let before = get_path(&v, &path).cloned().unwrap_or(Value::Null);
            set_path(&mut v, &path, value.clone())?;
            if before != value {
                changed.push(path);
            }
        }
        let spec: VmSpec = serde_json::from_value(v)
            .map_err(|e| format!("rule '{id}': the fixed spec is invalid: {e}"))?;
        Ok(FixResult {
            spec,
            changed,
            description: fix.description.clone(),
        })
    }

    /// Run every rule's own test cases against `base` (a full spec as
    /// JSON). Each auto-fix must also make its rule stop firing.
    pub fn self_test(&self, base: &Value) -> Result<usize, Vec<String>> {
        let mut failures = Vec::new();
        let mut n = 0;
        for r in &self.rules {
            for t in &r.tests {
                n += 1;
                let name = format!("{} / {}", r.id, t.name);
                let mut v = base.clone();
                if let Some(p) = &t.spec {
                    merge_patch(&mut v, p);
                }
                let spec: VmSpec = match serde_json::from_value(v) {
                    Ok(s) => s,
                    Err(e) => {
                        failures.push(format!("{name}: the patched spec is invalid: {e}"));
                        continue;
                    }
                };
                let mut facts = serde_json::to_value(Facts::derive(&spec, None, "linux", None))
                    .unwrap_or(Value::Null);
                if let Some(f) = &t.facts {
                    merge_patch(&mut facts, f);
                }
                let facts: Facts = match serde_json::from_value::<Facts>(facts) {
                    Ok(f) => f.settled(),
                    Err(e) => {
                        failures.push(format!("{name}: bad facts: {e}"));
                        continue;
                    }
                };
                let fired = r.fires(&context(&spec, &facts)).is_some();
                if fired != t.fires {
                    failures.push(format!(
                        "{name}: expected the rule {}",
                        if t.fires { "to fire" } else { "not to fire" }
                    ));
                    continue;
                }
                if fired && r.autofix.is_some() {
                    match self.fix(&spec, &facts, &r.id) {
                        Ok(res) => {
                            let mut after = facts.clone();
                            after.topology_product = res
                                .spec
                                .cpu
                                .topology
                                .map(|t| t.sockets * t.cores * t.threads);
                            let after = after.settled();
                            if r.fires(&context(&res.spec, &after)).is_some() {
                                failures.push(format!("{name}: still fires after its autofix"));
                            }
                        }
                        Err(e) => failures.push(format!("{name}: autofix failed: {e}")),
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(n)
        } else {
            Err(failures)
        }
    }
}

impl Rule {
    /// The witness indices if the rule fires.
    fn fires(&self, ctx: &Value) -> Option<Vec<usize>> {
        let a = &self.applies_to;
        let s = |p: &str| get_path(ctx, p).and_then(Value::as_str).unwrap_or_default();
        if (!a.arch.is_empty() && !a.arch.iter().any(|x| x == s("spec.arch")))
            || (!a.machine.is_empty()
                && !a
                    .machine
                    .iter()
                    .any(|m| s("spec.machine.type").starts_with(m.as_str())))
            || (!a.accel.is_empty() && !a.accel.iter().any(|x| x == s("facts.accel")))
            || (!a.host_os.is_empty() && !a.host_os.iter().any(|x| x == s("facts.host_os")))
        {
            return None;
        }
        eval(&self.when, ctx)
    }
}

/// Parse one rule file.
pub fn parse_file(text: &str) -> Result<RuleFile, String> {
    toml::from_str::<RuleFile>(text).map_err(|e| e.to_string())
}

/// JSON Schema for rule files (`data/schema/rules.v1.json`).
pub fn rule_file_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(RuleFile)).unwrap_or(Value::Null)
}

/// A spec good enough to patch in rule tests: x86_64 q35, one virtio disk
/// and NIC, VGA.
pub fn test_base_spec() -> Value {
    json!({
        "spec_version": 1,
        "name": "rule-test",
        "arch": "x86_64",
        "machine": {"type": "pc-q35-11.1", "props": {}},
        "accel": "auto",
        "cpu": {"model": "max", "props": {}, "count": 2, "topology": null},
        "memory": {"size_mib": 2048},
        "firmware": {"kind": "default"},
        "disks": [{
            "image": {"pool": "default", "name": "disk.qcow2"},
            "bus": "virtio", "media": "disk", "readonly": false, "boot_index": 1,
            "cache": "none", "aio": null, "discard": false, "slot": null
        }],
        "nics": [{
            "backend": {"kind": "user", "forwards": []},
            "model": "virtio-net-pci", "mac": "52:54:00:12:34:56"
        }],
        "display": {"device": "virtio-vga", "vnc": true, "vdagent": false},
        "input": {"usb_tablet": true},
        "shares": [],
        "usb": [],
        "passthrough": []
    })
}

fn context(spec: &VmSpec, facts: &Facts) -> Value {
    json!({
        "spec": serde_json::to_value(spec).unwrap_or(Value::Null),
        "facts": serde_json::to_value(facts).unwrap_or(Value::Null),
    })
}

fn check_cond(c: &Cond) -> Result<(), String> {
    let shapes = [
        c.all.is_some(),
        c.any.is_some(),
        c.not.is_some(),
        c.path.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if shapes != 1 {
        return Err("a condition has exactly one of all/any/not/path".into());
    }
    let ops = [
        c.eq.is_some(),
        c.ne.is_some(),
        c.in_.is_some(),
        c.not_in.is_some(),
        c.starts_with.is_some(),
        c.ends_with.is_some(),
        c.contains.is_some(),
        c.gt.is_some(),
        c.ge.is_some(),
        c.lt.is_some(),
        c.le.is_some(),
        c.exists.is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if let Some(p) = &c.path {
        if !(p.starts_with("spec.") || p.starts_with("facts.")) {
            return Err(format!("path '{p}' must start with spec. or facts."));
        }
        if ops != 1 {
            return Err(format!("condition on '{p}' needs exactly one operator"));
        }
    } else if ops != 0 {
        return Err("operators belong on a condition with a path".into());
    }
    for x in c.all.iter().chain(c.any.iter()).flatten() {
        check_cond(x)?;
    }
    if let Some(n) = &c.not {
        check_cond(n)?;
    }
    Ok(())
}

/// `Some(witness)` when the condition holds.
fn eval(c: &Cond, ctx: &Value) -> Option<Vec<usize>> {
    if let Some(all) = &c.all {
        let mut witness = Vec::new();
        for x in all {
            let w = eval(x, ctx)?;
            if witness.is_empty() {
                witness = w;
            }
        }
        return Some(witness);
    }
    if let Some(any) = &c.any {
        return any.iter().find_map(|x| eval(x, ctx));
    }
    if let Some(n) = &c.not {
        return eval(n, ctx).is_none().then(Vec::new);
    }
    let path = c.path.as_deref()?;
    let found = resolve(ctx, path);
    if let Some(want) = c.exists {
        let any = found.iter().find(|(_, v)| !v.is_null());
        return match (want, any) {
            (true, Some((w, _))) => Some(w.clone()),
            (false, None) => Some(Vec::new()),
            _ => None,
        };
    }
    found.into_iter().find(|(_, v)| leaf(c, v)).map(|(w, _)| w)
}

fn leaf(c: &Cond, v: &Value) -> bool {
    let num = v.as_f64();
    let s = v.as_str();
    if let Some(x) = &c.eq {
        return v == x;
    }
    if let Some(x) = &c.ne {
        return v != x;
    }
    if let Some(xs) = &c.in_ {
        return xs.contains(v);
    }
    if let Some(xs) = &c.not_in {
        return !xs.contains(v);
    }
    if let Some(p) = &c.starts_with {
        return s.is_some_and(|s| s.starts_with(p.as_str()));
    }
    if let Some(p) = &c.ends_with {
        return s.is_some_and(|s| s.ends_with(p.as_str()));
    }
    if let Some(p) = &c.contains {
        return s.is_some_and(|s| s.contains(p.as_str()))
            || v.as_array()
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(p)));
    }
    let cmp =
        |f: fn(f64, f64) -> bool, x: Option<f64>| x.is_some_and(|x| num.is_some_and(|n| f(n, x)));
    cmp(|n, x| n > x, c.gt)
        || cmp(|n, x| n >= x, c.ge)
        || cmp(|n, x| n < x, c.lt)
        || cmp(|n, x| n <= x, c.le)
}

/// `a.b[*].c` → `[("a", None), ("b", Some(Wild)), ("c", None)]`.
enum Index {
    None,
    Wild,
    At(usize),
}

fn segments(path: &str) -> Vec<(&str, Index)> {
    path.split('.')
        .map(|seg| match seg.split_once('[') {
            Some((name, rest)) => {
                let inner = rest.trim_end_matches(']');
                let idx = if inner == "*" {
                    Index::Wild
                } else {
                    inner.parse().map(Index::At).unwrap_or(Index::None)
                };
                (name, idx)
            }
            None => (seg, Index::None),
        })
        .collect()
}

/// Every value at `path`, with the indices `[*]` took to reach it.
fn resolve<'a>(root: &'a Value, path: &str) -> Vec<(Vec<usize>, &'a Value)> {
    let mut cur: Vec<(Vec<usize>, &Value)> = vec![(Vec::new(), root)];
    for (name, idx) in segments(path) {
        let mut next = Vec::new();
        for (w, v) in cur {
            let Some(child) = v.get(name) else { continue };
            match idx {
                Index::None => next.push((w, child)),
                Index::At(i) => {
                    if let Some(x) = child.get(i) {
                        next.push((w, x));
                    }
                }
                Index::Wild => {
                    for (i, x) in child.as_array().into_iter().flatten().enumerate() {
                        let mut w = w.clone();
                        w.push(i);
                        next.push((w, x));
                    }
                }
            }
        }
        cur = next;
    }
    cur
}

/// Replace each `[*]` with the next witness index (`0` when none is left).
fn substitute(path: &str, witness: &[usize]) -> String {
    let mut out = String::new();
    let mut rest = path;
    let mut i = 0;
    while let Some(pos) = rest.find("[*]") {
        out.push_str(&rest[..pos]);
        out.push_str(&format!("[{}]", witness.get(i).copied().unwrap_or(0)));
        i += 1;
        rest = &rest[pos + 3..];
    }
    out.push_str(rest);
    out
}

fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    resolve(root, path).into_iter().next().map(|(_, v)| v)
}

/// Set a concrete path, creating objects for missing map keys.
fn set_path(root: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let segs = segments(path);
    let mut cur = root;
    for (k, (name, idx)) in segs.iter().enumerate() {
        let last = k + 1 == segs.len();
        let obj = cur
            .as_object_mut()
            .ok_or_else(|| format!("'{path}': '{name}' is not inside an object"))?;
        match idx {
            Index::Wild => return Err(format!("'{path}' still has a wildcard")),
            Index::None if last => {
                obj.insert((*name).into(), value);
                return Ok(());
            }
            Index::None => {
                cur = obj
                    .entry((*name).to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            Index::At(i) => {
                let arr = obj
                    .get_mut(*name)
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| format!("'{path}': '{name}' is not a list"))?;
                let slot = arr
                    .get_mut(*i)
                    .ok_or_else(|| format!("'{path}': no element {i}"))?;
                if last {
                    *slot = value;
                    return Ok(());
                }
                cur = slot;
            }
        }
    }
    Ok(())
}

/// RFC 7386 JSON merge patch.
fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(p) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let t = target.as_object_mut().expect("object");
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        _ => *target = patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(patch: Value) -> VmSpec {
        let mut v = test_base_spec();
        merge_patch(&mut v, &patch);
        serde_json::from_value(v).unwrap()
    }

    fn rule(text: &str) -> RuleSet {
        let set = RuleSet {
            rules: parse_file(text).unwrap().rules,
        };
        set.validate().unwrap();
        set
    }

    const SAMPLE: &str = r#"
[[rule]]
id = "l2.test.unsafe-cache"
verified_against = ["11.1"]
when = { path = "spec.disks[*].cache", eq = "unsafe" }
field = "disks[*].cache"
message = "unsafe"
autofix = { description = "use none", set = [{ path = "disks[*].cache", value = "none" }] }
[[rule.test]]
name = "fires"
spec = { disks = [{ image = { pool = "p", name = "a" }, bus = "virtio", cache = "none" }, { image = { pool = "p", name = "b" }, bus = "virtio", cache = "unsafe" }] }
fires = true
[[rule.test]]
name = "quiet"
fires = false
"#;

    #[test]
    fn harness_runs_every_builtin_rule() {
        let set = RuleSet::builtin();
        assert!(set.rules.len() >= 8, "ships the plan's examples");
        match set.self_test(&test_base_spec()) {
            Ok(n) => assert!(n >= 2 * set.rules.len()),
            Err(f) => panic!("rule tests failed:\n{}", f.join("\n")),
        }
    }

    #[test]
    fn wildcard_witness_pins_the_field_and_the_fix() {
        let set = rule(SAMPLE);
        set.self_test(&test_base_spec()).unwrap();
        let s = spec(json!({"disks": [
            {"image": {"pool": "p", "name": "a"}, "bus": "virtio", "cache": "none"},
            {"image": {"pool": "p", "name": "b"}, "bus": "virtio", "cache": "unsafe"}
        ]}));
        let facts = Facts::default();
        let d = set.evaluate(&s, &facts);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].field, "disks[1].cache");
        assert_eq!(d[0].severity, Severity::Warning);
        let fixed = set.fix(&s, &facts, "l2.test.unsafe-cache").unwrap();
        assert_eq!(fixed.changed, ["disks[1].cache"]);
        assert!(set.evaluate(&fixed.spec, &facts).is_empty());
        assert!(
            set.fix(&fixed.spec, &facts, "l2.test.unsafe-cache")
                .is_err(),
            "no longer applies"
        );
    }

    #[test]
    fn operators() {
        let ctx = json!({"spec": {"n": 4, "s": "virtio-net-pci", "a": ["x", "y"], "m": {"k": "v"}, "z": null}});
        let c = |t: &str| -> Cond { toml::from_str(t).unwrap() };
        let holds = |t: &str| eval(&c(t), &ctx).is_some();
        assert!(holds(
            r#"path = "spec.n"
gt = 3.0"#
        ));
        assert!(!holds(
            r#"path = "spec.n"
lt = 4.0"#
        ));
        assert!(holds(
            r#"path = "spec.s"
ends_with = "-pci""#
        ));
        assert!(holds(
            r#"path = "spec.a"
contains = "y""#
        ));
        assert!(holds(
            r#"path = "spec.a[*]"
in = ["y"]"#
        ));
        assert!(holds(
            r#"path = "spec.m.k"
exists = true"#
        ));
        assert!(holds(
            r#"path = "spec.z"
exists = false"#
        ));
        assert!(holds(
            r#"path = "spec.missing"
exists = false"#
        ));
        assert!(!holds(
            r#"path = "spec.missing"
eq = 1"#
        ));
        assert!(holds(r#"not = { path = "spec.s", starts_with = "e1000" }"#));
        assert!(holds(
            r#"any = [{ path = "spec.n", eq = 1 }, { path = "spec.n", eq = 4 }]"#
        ));
    }

    #[test]
    fn validation_rejects_bad_rules() {
        let bad = |t: &str| {
            let rules = parse_file(t).map(|f| f.rules);
            rules.map_or(true, |rules| RuleSet { rules }.validate().is_err())
        };
        // error without citation
        assert!(bad(&SAMPLE.replace(
            "verified_against",
            "severity = \"error\"\nverified_against"
        )));
        // two operators
        assert!(bad(
            &SAMPLE.replace("eq = \"unsafe\"", "eq = \"unsafe\", ne = \"x\"")
        ));
        // unknown key
        assert!(bad(&SAMPLE.replace("message =", "mesage =")));
        // bad id
        assert!(bad(&SAMPLE.replace("l2.test.unsafe-cache", "Unsafe")));
        // no non-firing test
        assert!(bad(&SAMPLE.replace("fires = false", "fires = true")));
    }

    #[test]
    fn overrides_replace_by_id() {
        let dir = std::env::temp_dir().join(format!("qui-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = RuleSet::builtin().rules[0].id.clone();
        std::fs::write(
            dir.join("site.toml"),
            SAMPLE.replace("l2.test.unsafe-cache", &id),
        )
        .unwrap();
        let set = RuleSet::with_overrides(&dir).unwrap();
        assert_eq!(set.rules.len(), RuleSet::builtin().rules.len());
        assert_eq!(set.get(&id).unwrap().message, "unsafe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_schema_file_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/schema/rules.v1.json");
        let want = serde_json::to_string_pretty(&rule_file_schema()).unwrap() + "\n";
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            have,
            want,
            "stale {}; run with UPDATE_GOLDEN=1",
            path.display()
        );
    }
}
