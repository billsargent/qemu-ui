//! Guest network routes (M3-08).
//!
//! GET /vms/{id}/guest-net → `GuestNetInfo` (addresses per NIC, best first)
//! GET /vms/{id}/guest     → `GuestInfo` (what the guest agent reports, M7-02)

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::guest::GuestInfo;
use qemu_ui_core::guest_net::GuestNetInfo;
use qemu_ui_core::vm::VmState;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/guest-net", get(guest_net))
        .route("/vms/{id}/guest", get(guest))
}

async fn guest_net(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<GuestNetInfo>, OpErr> {
    crate::guest_net::guest_net(&app.supervisor, &id)
        .await
        .map(Json)
        .map_err(op_err)
}

async fn guest(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<GuestInfo>, OpErr> {
    let sv = &app.supervisor;
    let detail = sv.detail(&id).map_err(op_err)?;
    let spec = detail.live_spec.as_ref().unwrap_or(&detail.spec);
    let running = matches!(detail.summary.state, VmState::Running | VmState::Paused);
    let idle = GuestInfo {
        agent: spec.guest.agent,
        running,
        ..Default::default()
    };
    if !running || !spec.guest.agent {
        return Ok(Json(idle));
    }
    let Some(qmp) = sv.live_qmp(&id) else {
        return Ok(Json(idle));
    };
    Ok(Json(match crate::qga::socket(&qmp).await {
        Some(sock) => crate::qga::info(&sock).await,
        None => GuestInfo {
            error: Some("the running VM has no guest agent channel; restart it to add one".into()),
            ..idle
        },
    }))
}

/// A VM with a guest-agent channel and a cloud-init seed starts on real
/// QEMU; the seed is attached read-only, and with no agent in the guest
/// the API says so promptly and `stop` falls back to ACPI.
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use qemu_ui_core::guest::CloudInit;
    use qemu_ui_core::vm::VmState;

    #[test]
    fn agent_channel_and_seed_on_real_qemu() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("guest");
        let _guard = KillVmsOnDrop(data.clone());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "guest").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.guest.agent = true;
            spec.guest.cloud_init = Some(CloudInit {
                user: Some("ops".into()),
                password: Some("plain-text".into()),
                ..Default::default()
            });
            let saved = sv
                .update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let pw = saved.spec.guest.cloud_init.unwrap().password.unwrap();
            assert!(pw.starts_with("$6$"), "stored hashed: {pw}");

            let s = sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert_eq!(s.state, VmState::Running);
            let qmp = sv.live_qmp(&id).unwrap();
            let blocks = qmp.execute("query-block", None).await.unwrap();
            let seed = blocks
                .as_array()
                .unwrap()
                .iter()
                .find(|b| {
                    b["qdev"]
                        .as_str()
                        .is_some_and(|q| q.contains("qui-seed-cd"))
                })
                .unwrap_or_else(|| panic!("seed CD-ROM attached: {blocks}"));
            assert_eq!(seed["inserted"]["ro"], true, "{seed}");
            let iso = std::fs::read(data.join("vms").join(&id).join("cloud-init.iso")).unwrap();
            assert_eq!(&iso[16 * 2048 + 40..16 * 2048 + 46], b"CIDATA");

            let chardevs = qmp.execute("query-chardev", None).await.unwrap();
            assert!(
                crate::qga::socket(&qmp).await.is_some(),
                "qui-ga0 chardev: {chardevs}"
            );
            let t0 = std::time::Instant::now();
            let sock = crate::qga::socket(&qmp).await.unwrap();
            let info = crate::qga::info(&sock).await;
            assert!(!info.connected && info.error.is_some(), "{info:?}");
            assert!(t0.elapsed() < std::time::Duration::from_secs(10));

            // No agent in the guest: stop falls back to ACPI.
            let s = sv.stop(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            assert!(matches!(s.state, VmState::Stopping | VmState::Stopped));
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
