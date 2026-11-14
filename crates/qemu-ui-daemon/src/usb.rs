//! USB passthrough shelf (plan.md §9.6, M2-12).
//!
//! * `GET /host/usb` lists host USB devices with a usability verdict.
//! * `POST /vms/{id}/usb` adds a passthrough rule to the VM's spec and, when
//!   the VM runs, hot-plugs a `usb-host` device into it.
//! * `DELETE /vms/{id}/usb/{index}` removes the rule and hot-unplugs the
//!   device, waiting for QEMU's `DEVICE_DELETED`.
//!
//! Rules live in the spec (`VmSpec::usb`), so they are remembered across
//! restarts: QEMU's `usb-host` waits for a matching device and attaches it
//! whenever it is plugged in.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use axum::extract::{Path as UrlPath, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use qemu_ui_core::compile::usb_host_device;
use qemu_ui_core::spec::{UsbPassthrough, VmSpec};
use qemu_ui_core::usb::{HostUsb, UsbDevice, device_id, normalize_id};
use qemu_ui_core::vm::VmDetail;
use qemu_ui_qmp::Qmp;
use serde_json::json;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};
use crate::supervisor::{OpError, Supervisor};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/host/usb", get(host_usb))
        .route("/vms/{id}/usb", post(attach))
        .route("/vms/{id}/usb/{index}", delete(detach))
}

// -------------------------------------------------------------- enumeration

/// Enumerate host USB devices on Linux from sysfs.
#[cfg(target_os = "linux")]
async fn enumerate() -> Result<Vec<UsbDevice>> {
    use qemu_ui_core::usb::{SysfsDevice, parse_sysfs};
    use std::path::Path;
    tokio::task::spawn_blocking(|| {
        let root = Path::new("/sys/bus/usb/devices");
        let mut devs = Vec::new();
        let entries: Vec<String> = std::fs::read_dir(root)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        for name in entries.iter().filter(|n| !n.contains(':')) {
            let dir = root.join(name);
            let mut d = SysfsDevice {
                name: name.clone(),
                ..Default::default()
            };
            for attr in [
                "idVendor",
                "idProduct",
                "busnum",
                "devnum",
                "manufacturer",
                "product",
                "bDeviceClass",
            ] {
                if let Ok(v) = std::fs::read_to_string(dir.join(attr)) {
                    d.attrs.insert(attr.into(), v.trim().to_string());
                }
            }
            for iface in entries
                .iter()
                .filter(|n| n.starts_with(&format!("{name}:")))
            {
                if let Ok(target) = std::fs::read_link(root.join(iface).join("driver"))
                    && let Some(drv) = target.file_name().and_then(|f| f.to_str())
                {
                    d.interface_drivers.push(drv.to_string());
                }
            }
            if let (Some(b), Some(n)) = (
                d.attrs.get("busnum").and_then(|v| v.parse::<u32>().ok()),
                d.attrs.get("devnum").and_then(|v| v.parse::<u32>().ok()),
            ) {
                let node = format!("/dev/bus/usb/{b:03}/{n:03}");
                d.node_accessible = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(node)
                        .is_ok(),
                );
            }
            devs.push(d);
        }
        Ok(parse_sysfs(&devs))
    })
    .await?
}

/// Enumerate host USB devices on Windows from `Win32_PnPEntity`.
#[cfg(windows)]
async fn enumerate() -> Result<Vec<UsbDevice>> {
    const QUERY: &str = "Get-CimInstance Win32_PnPEntity -Filter \"PNPDeviceID LIKE 'USB\\\\VID%'\" | \
                         Select-Object PNPDeviceID,Name,Manufacturer,Service,PNPClass,Status | \
                         ConvertTo-Json -Compress";
    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", QUERY])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| anyhow!("USB enumeration timed out"))??;
    if !out.status.success() {
        bail!(
            "USB enumeration failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    qemu_ui_core::usb::parse_windows_pnp(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| anyhow!(e))
}

#[cfg(not(any(target_os = "linux", windows)))]
async fn enumerate() -> Result<Vec<UsbDevice>> {
    bail!("listing host USB devices is not implemented on this platform")
}

async fn qemu_has_usb_host(app: &App) -> bool {
    let caps = app.scanner.caps.read().await;
    caps.as_ref().is_some_and(|c| {
        c.binaries
            .iter()
            .any(|b| b.devices.iter().any(|d| d.name == "usb-host"))
    })
}

async fn host_usb(State(app): State<Arc<App>>) -> Json<HostUsb> {
    let qemu_supported = qemu_has_usb_host(&app).await;
    match enumerate().await {
        Ok(devices) => Json(HostUsb {
            devices,
            qemu_supported,
            error: None,
        }),
        Err(e) => Json(HostUsb {
            devices: vec![],
            qemu_supported,
            error: Some(format!("{e:#}")),
        }),
    }
}

// ---------------------------------------------------------- attach/detach

/// Apply the same rule change to the running instance's recorded spec, so
/// a hotplug does not show up as "restart to apply changes".
fn sync_run_spec(sv: &Supervisor, vm_id: &str, change: impl FnOnce(&mut VmSpec)) {
    let Ok(Some(run)) = sv.store.last_run(vm_id) else {
        return;
    };
    if run.ended_at.is_some() {
        return;
    }
    if let Ok(mut spec) = serde_json::from_str::<VmSpec>(&run.spec_json) {
        change(&mut spec);
        if let Ok(json) = serde_json::to_string(&spec) {
            let _ = sv.store.update_run_spec(run.id, &json);
        }
    }
}

/// Hot-plug rule `index` of `spec` into a running VM.
pub async fn hotplug(qmp: &Qmp, spec: &VmSpec, index: usize) -> Result<()> {
    qmp.execute("device_add", Some(usb_host_device(spec, index)))
        .await
        .map_err(|e| anyhow!("QEMU refused the USB device: {e}"))?;
    Ok(())
}

/// Hot-unplug a device by id and wait for QEMU to confirm.
pub async fn unplug(qmp: &Qmp, id: &str) -> Result<()> {
    let mut events = qmp.events();
    match qmp.execute("device_del", Some(json!({"id": id}))).await {
        Ok(_) => {}
        // Already gone (e.g. attached while stopped and never started).
        Err(e) if e.to_string().contains("not found") => return Ok(()),
        Err(e) => bail!("QEMU refused to remove the USB device: {e}"),
    }
    let wait = async {
        loop {
            match events.recv().await {
                Ok(ev) if ev.event == "DEVICE_DELETED" && ev.data["device"] == id => return Ok(()),
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return Err(anyhow!("the VM's monitor closed")),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .map_err(|_| anyhow!("QEMU did not confirm removing the USB device"))?
}

fn bad(msg: impl Into<String>) -> OpErr {
    op_err(OpError::Conflict(msg.into()))
}

async fn attach(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(mut rule): Json<UsbPassthrough>,
) -> Result<Json<VmDetail>, OpErr> {
    let sv = &app.supervisor;
    rule.vendor_id =
        normalize_id(&rule.vendor_id).ok_or_else(|| bad("vendor id must be 4 hex digits"))?;
    rule.product_id =
        normalize_id(&rule.product_id).ok_or_else(|| bad("product id must be 4 hex digits"))?;
    let current = sv.detail(&id).map_err(op_err)?;
    let mut spec = current.spec.clone();
    if spec.usb.iter().any(|u| {
        u.vendor_id == rule.vendor_id
            && u.product_id == rule.product_id
            && u.host_bus == rule.host_bus
            && u.host_port == rule.host_port
    }) {
        return Err(bad("this device is already attached to the VM"));
    }
    spec.usb.push(rule.clone());
    let index = spec.usb.len() - 1;
    let detail = sv.update(&id, spec.clone()).await.map_err(op_err)?;

    if let Some(qmp) = sv.live_qmp(&id) {
        if let Err(e) = hotplug(&qmp, &detail.spec, index).await {
            // Keep the spec consistent with what is actually running.
            let _ = sv.update(&id, current.spec).await;
            return Err(op_err(OpError::Failed(e)));
        }
        sync_run_spec(sv, &id, |s| s.usb.push(rule));
    }
    sv.detail(&id).map(Json).map_err(op_err)
}

async fn detach(
    State(app): State<Arc<App>>,
    UrlPath((id, index)): UrlPath<(String, usize)>,
) -> Result<Json<VmDetail>, OpErr> {
    let sv = &app.supervisor;
    let current = sv.detail(&id).map_err(op_err)?;
    if index >= current.spec.usb.len() {
        return Err(op_err(OpError::NotFound));
    }
    let dev_id = device_id(&current.spec.usb, index);
    if let Some(qmp) = sv.live_qmp(&id) {
        unplug(&qmp, &dev_id)
            .await
            .map_err(|e| op_err(OpError::Failed(e)))?;
        sync_run_spec(sv, &id, |s| {
            if let Some(i) = (0..s.usb.len()).find(|&i| device_id(&s.usb, i) == dev_id) {
                s.usb.remove(i);
            }
        });
    }
    let mut spec = current.spec;
    spec.usb.remove(index);
    sv.update(&id, spec).await.map(Json).map_err(op_err)
}

/// Live attach/detach against a real QEMU, without any physical device:
/// QEMU accepts a `usb-host` that matches nothing and waits for it.
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn call(r: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let res = r
            .clone()
            .oneshot(
                b.body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn hotplug_and_unplug_usb_host() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("usb");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            if !caps.binaries[0]
                .devices
                .iter()
                .any(|d| d.name == "usb-host")
            {
                eprintln!("skipping: this QEMU has no usb-host");
                return;
            }
            let sv = make(&data, &caps).await;
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);
            let id = small_vm(&sv, &caps, "usbvm").await;

            // Host listing works (devices vary by machine; only the shape is checked).
            let (s, host) = call(&r, "GET", "/api/v1/host/usb", None).await;
            assert_eq!(s, 200);
            assert_eq!(host["qemu_supported"], true);
            assert!(host["devices"].is_array(), "{host}");

            // Stopped VM: the rule is just remembered and compiled into argv.
            let rule = json!({"vendor_id": "FFFE", "product_id": "0001", "label": "test device"});
            let (s, d) = call(&r, "POST", &format!("/api/v1/vms/{id}/usb"), Some(rule)).await;
            assert_eq!(s, 200, "{d}");
            assert_eq!(
                d["spec"]["usb"][0]["vendor_id"], "fffe",
                "ids are normalised"
            );
            let (s, _) = call(
                &r,
                "POST",
                &format!("/api/v1/vms/{id}/usb"),
                Some(json!({"vendor_id": "fffe", "product_id": "0001"})),
            )
            .await;
            assert_eq!(s, 409, "duplicate refused");

            // Start: QEMU accepts usb-host with no matching device present.
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let qmp = sv.live_qmp(&id).unwrap();
            let tree = |q: qemu_ui_qmp::Qmp| async move {
                q.execute("qom-list", Some(json!({"path": "/machine/peripheral"})))
                    .await
                    .unwrap()
            };
            let has =
                |v: &Value, name: &str| v.as_array().unwrap().iter().any(|p| p["name"] == name);
            assert!(has(&tree(qmp.clone()).await, "qui-usb-fffe-0001"));

            // Live attach of a second device.
            let (s, d) = call(
                &r,
                "POST",
                &format!("/api/v1/vms/{id}/usb"),
                Some(json!({"vendor_id": "fffe", "product_id": "0002"})),
            )
            .await;
            assert_eq!(s, 200, "{d}");
            assert!(has(&tree(qmp.clone()).await, "qui-usb-fffe-0002"));
            assert_eq!(
                d["summary"]["restart_pending"], false,
                "hotplug is not a pending change"
            );

            // Live detach of the first: QEMU confirms DEVICE_DELETED.
            let (s, d) = call(&r, "DELETE", &format!("/api/v1/vms/{id}/usb/0"), None).await;
            assert_eq!(s, 200, "{d}");
            let t = tree(qmp.clone()).await;
            assert!(
                !has(&t, "qui-usb-fffe-0001") && has(&t, "qui-usb-fffe-0002"),
                "{t}"
            );
            assert_eq!(d["spec"]["usb"].as_array().unwrap().len(), 1);
            assert_eq!(d["summary"]["restart_pending"], false);

            let (s, _) = call(&r, "DELETE", &format!("/api/v1/vms/{id}/usb/5"), None).await;
            assert_eq!(s, 404);

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, qemu_ui_core::vm::VmState::Stopped).await;
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
