//! Hardware view and device palette (M4-05).

use super::*;
use qemu_ui_core::devices::{self, HardwareTree, PaletteEntry};

impl Supervisor {
    /// Devices that can be added to `machine` on `arch`. When the machine
    /// cannot be probed every device counts as available.
    pub async fn device_palette(&self, arch: &str, machine: &str) -> OpResult<Vec<PaletteEntry>> {
        let caps = self.caps_for(arch).await?;
        let machine = compile::pin_machine(machine, &caps).map_err(OpError::Conflict)?;
        let buses = self.machine_buses(&caps, &machine).await;
        Ok(devices::palette(&caps.devices, buses.as_ref()))
    }

    /// The devices `spec` gives QEMU, by bus. Built from the same argv a
    /// start would use (preview mode: nothing on the host is touched).
    pub async fn hardware_tree(&self, id: Option<&str>, spec: VmSpec) -> OpResult<HardwareTree> {
        let (spec, caps) = self.normalise(spec).await?;
        let (c, _) = self
            .build(id.unwrap_or("new"), &spec, &caps, &BuildMode::Preview)
            .await
            .map_err(OpError::Invalid)?;
        let buses = self.machine_buses(&caps, &spec.machine.type_name).await;
        let passthrough_from = c.args.len().saturating_sub(spec.passthrough.len());
        Ok(devices::device_tree(
            &c.args,
            passthrough_from,
            &spec,
            &caps.devices,
            buses.as_ref(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use qemu_ui_core::devices::DeviceSpec;

    /// A palette device compiles, QEMU accepts it, and the tree shows it.
    #[test]
    fn generic_device_reaches_qemu_and_the_tree() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("hw");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "hw").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;

            let palette = sv
                .device_palette(&spec.arch, &spec.machine.type_name)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let rng = palette
                .iter()
                .find(|e| e.name == "virtio-rng-pci")
                .expect("in the palette");
            assert!(rng.available, "{rng:?}");
            assert!(
                !palette.iter().any(|e| e.name == "ICH9-LPC"),
                "built-ins are not addable"
            );

            let mut props = std::collections::BTreeMap::new();
            props.insert("max-bytes".to_string(), serde_json::json!(4096));
            spec.devices.push(DeviceSpec {
                driver: "virtio-rng-pci".into(),
                bus: None,
                props,
            });
            sv.update(&id, spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            let tree = sv
                .hardware_tree(Some(&id), spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let node = tree
                .devices
                .iter()
                .find(|d| d.id == "qui-xdev0")
                .expect("in the tree")
                .clone();
            assert_eq!(node.generic, Some(0));
            assert!(node.bus.is_some(), "{node:?}");

            let r = tokio::time::timeout(Duration::from_secs(120), async {
                sv.start(&id).await.map_err(|e| format!("{e:?}"))?;
                let qmp = sv.live_qmp(&id).ok_or("no QMP")?;
                qmp.execute(
                    "qom-get",
                    Some(serde_json::json!({"path": "/machine/peripheral/qui-xdev0", "property": "max-bytes"})),
                )
                .await
                .map_err(|e| format!("{e:#}"))
            })
            .await;
            // Always stop, whatever happened.
            let _ = sv.force_stop(&id).await;
            let _ = tokio::time::timeout(
                Duration::from_secs(60),
                wait_state(&sv, &id, VmState::Stopped),
            )
            .await;
            r
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
        let v = outcome
            .expect("start timed out")
            .expect("QEMU accepted the device");
        assert_eq!(v, serde_json::json!(4096));
    }
}
