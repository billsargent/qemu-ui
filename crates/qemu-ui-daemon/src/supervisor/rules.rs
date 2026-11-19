//! L2 curated rules (M4-03) and their auto-fixes (M4-04) for the supervisor.
//!
//! Rules are the built-in `data/rules/*.toml` plus any `<data_dir>/rules/*.toml`
//! (same id replaces). Facts come from the binary's capabilities, the host
//! and the firmware descriptor.

use std::sync::OnceLock;

use qemu_ui_core::caps::BinaryCaps;
use qemu_ui_core::compile::{self, Diagnostic};
use qemu_ui_core::rules::{Facts, FixResult, RuleInfo, RuleSet};
use qemu_ui_core::spec::{FirmwareSpec, VmSpec};

use super::{OpError, OpResult, Supervisor};

static RULES: OnceLock<RuleSet> = OnceLock::new();

impl Supervisor {
    /// The rule set, loaded once.
    pub(crate) fn rules(&self) -> &'static RuleSet {
        RULES.get_or_init(|| {
            let dir = self.data_dir.join("rules");
            RuleSet::with_overrides(&dir).unwrap_or_else(|e| {
                tracing::warn!("ignoring site rules in {}: {e}", dir.display());
                RuleSet::builtin()
            })
        })
    }

    pub fn rule_info(&self) -> Vec<RuleInfo> {
        self.rules().info()
    }

    /// Facts for `spec` (already normalised against `caps`).
    pub(crate) async fn rule_facts(&self, spec: &VmSpec, caps: &BinaryCaps) -> Facts {
        let firmware = match &spec.firmware {
            FirmwareSpec::Descriptor { id } => self
                .scanner
                .caps
                .read()
                .await
                .as_ref()
                .and_then(|c| c.firmware.iter().find(|f| &f.id == id).cloned()),
            FirmwareSpec::Default => None,
        };
        Facts::derive(spec, Some(caps), std::env::consts::OS, firmware.as_ref())
    }

    /// Add the L2 findings for `spec` to `diags`.
    pub(crate) async fn add_rule_diagnostics(
        &self,
        diags: &mut Vec<Diagnostic>,
        spec: &VmSpec,
        caps: &BinaryCaps,
    ) {
        let facts = self.rule_facts(spec, caps).await;
        self.rules().extend(diags, spec, &facts);
    }

    /// Apply rule `rule`'s auto-fix (an L2 id, or a built-in diagnostic an
    /// L2 rule covers). Only the machine alias is pinned besides the fix, so
    /// the editor's other fields come back as sent.
    pub async fn apply_fix(&self, mut spec: VmSpec, rule: &str) -> OpResult<FixResult> {
        let caps = self.caps_for(&spec.arch).await?;
        if let Ok(pinned) = compile::pin_machine(&spec.machine.type_name, &caps) {
            spec.machine.type_name = pinned;
        }
        let facts = self.rule_facts(&spec, &caps).await;
        self.rules()
            .fix(&spec, &facts, rule)
            .map_err(OpError::Conflict)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use qemu_ui_core::spec::CacheMode;

    #[test]
    fn preview_reports_l2_and_fix_clears_it() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("rules");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "ruled").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            // A Windows guest (Hyper-V enlightenment) without a tablet.
            spec.cpu.props.insert("hv-relaxed".into(), "on".into());
            spec.input.usb_tablet = false;
            spec.disks[0].cache = CacheMode::Unsafe;

            let (_, warnings) = sv.preview(Some(&id), spec.clone()).await.ok().unwrap();
            let rules: Vec<&str> = warnings.iter().map(|d| d.rule.as_str()).collect();
            assert!(rules.contains(&"l2.guest.windows-usb-tablet"), "{rules:?}");
            assert!(rules.contains(&"l2.guest.windows-virtio-disk"), "{rules:?}");
            // The built-in cache warning covers the L2 one: reported once.
            assert!(rules.contains(&"disk.cache-unsafe"), "{rules:?}");
            assert!(!rules.contains(&"l2.disk.unsafe-cache"), "{rules:?}");

            let fixed = sv
                .apply_fix(spec.clone(), "l2.guest.windows-usb-tablet")
                .await
                .ok()
                .unwrap();
            assert_eq!(fixed.changed, ["input.usb_tablet"]);
            assert!(fixed.spec.input.usb_tablet);
            // A fix offered for a covered built-in diagnostic.
            let fixed = sv
                .apply_fix(fixed.spec, "disk.cache-unsafe")
                .await
                .ok()
                .unwrap();
            assert_eq!(fixed.changed, ["disks[0].cache"]);
            let (_, warnings) = sv.preview(Some(&id), fixed.spec).await.ok().unwrap();
            assert!(
                !warnings
                    .iter()
                    .any(|d| d.rule == "l2.guest.windows-usb-tablet"
                        || d.rule == "disk.cache-unsafe")
            );
            // Nothing to fix.
            assert!(
                sv.apply_fix(spec, "l2.firmware.uefi-low-memory")
                    .await
                    .is_err()
            );
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
