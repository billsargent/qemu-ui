//! NIC models per machine and registry-unique MACs (M3-07).

use std::collections::HashSet;

use super::*;
use qemu_ui_core::nic::{self, NicModel};
use qemu_ui_core::spec::NicSpec;

impl Supervisor {
    /// The NIC models offered for `machine` on `arch`, preferred first.
    /// When the machine cannot be probed every model counts as available.
    pub async fn nic_models(&self, arch: &str, machine: &str) -> OpResult<Vec<NicModel>> {
        let caps = self.caps_for(arch).await?;
        let machine = compile::pin_machine(machine, &caps).map_err(OpError::Conflict)?;
        let buses = self.machine_buses(&caps, &machine).await;
        Ok(nic::nic_models(&caps.devices, buses.as_ref()))
    }

    /// Every VM's (id, name, MACs), from the stored specs.
    fn registry_macs(&self) -> Vec<(String, String, Vec<String>)> {
        self.store
            .list_vms()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let spec: VmSpec = serde_json::from_str(&row.spec_json).ok()?;
                Some((row.id, row.name, nic::nic_macs(&spec.nics)))
            })
            .collect()
    }

    /// Give NICs without a MAC one no registered VM uses.
    pub(super) fn assign_macs(&self, nics: &mut [NicSpec]) {
        let used: HashSet<String> = self
            .registry_macs()
            .into_iter()
            .flat_map(|(_, _, m)| m)
            .collect();
        nic::assign_macs(nics, &used, rand::random);
    }

    /// Warnings for MACs another VM (not `id`) also uses.
    pub(super) fn shared_mac_warnings(&self, id: Option<&str>, spec: &VmSpec) -> Vec<Diagnostic> {
        let others = self.registry_macs();
        nic::shared_mac_warnings(
            &spec.nics,
            others
                .iter()
                .filter(|(vid, _, _)| Some(vid.as_str()) != id)
                .map(|(_, name, macs)| (name.as_str(), macs.clone())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn macs_are_unique_across_the_registry() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU tools not found");
            return;
        };
        let data = short_data_dir("mac");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let a = small_vm(&sv, &caps, "mac-a").await;
            let spec_a = sv.detail(&a).ok().unwrap().spec;
            let mac_a = spec_a.nics[0].mac.clone().unwrap();

            // Allocation never hands out an address already registered.
            let mut nics = vec![spec_a.nics[0].clone(); 3];
            nics.iter_mut().for_each(|n| n.mac = None);
            sv.assign_macs(&mut nics);
            let got: HashSet<String> = nics.iter().map(|n| n.mac.clone().unwrap()).collect();
            assert_eq!(got.len(), 3, "distinct within the VM");
            assert!(!got.contains(&mac_a), "not reused from another VM");

            // A copied MAC is saved but flagged in preview, against the
            // other VM only (not against itself).
            let mut spec_b = spec_a.clone();
            spec_b.name = "mac-b".into();
            let b = sv.create(spec_b.clone()).await.ok().unwrap().summary.id;
            let (_, w) = sv.preview(Some(&b), spec_b).await.ok().unwrap();
            let shared: Vec<_> = w.iter().filter(|d| d.rule == "nic.mac-shared").collect();
            assert_eq!(shared.len(), 1, "{w:?}");
            assert!(shared[0].message.contains("mac-a"));
            let (_, w) = sv.preview(Some(&a), spec_a.clone()).await.ok().unwrap();
            assert!(w.iter().any(|d| d.message.contains("mac-b")), "{w:?}");

            // Two NICs with one MAC in a single VM is refused.
            let mut dup = spec_a.clone();
            dup.nics.push(dup.nics[0].clone());
            assert!(matches!(sv.update(&a, dup).await, Err(OpError::Invalid(d))
                if d.iter().any(|x| x.rule == "nic.mac-duplicate")));
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
