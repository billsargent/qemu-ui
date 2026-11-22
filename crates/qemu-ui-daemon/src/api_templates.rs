//! Template gallery API (M7-04) and the Windows 11 lane (M7-07).
//!
//! `GET /templates` lists the shipped templates with what this host can give
//! each; `POST /templates/{id}/plan` turns one into a spec for an
//! architecture (no side effects) for the editor; `POST /vms/from-template`
//! creates the system disk and the VM from the (possibly edited) plan.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use qemu_ui_core::caps::BinaryCaps;
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::templates::{self, FromTemplate, TemplateInfo, TemplatePlan, TpmNeed};
use serde::Deserialize;

use crate::api::{ApiError, App};
use crate::auth::Actor;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/templates", get(list))
        .route("/templates/{id}/plan", post(plan))
        .route("/vms/from-template", post(create))
}

fn bad(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

async fn list(State(app): State<Arc<App>>) -> Result<Json<Vec<TemplateInfo>>, ApiError> {
    let caps = app.scanner.caps.read().await;
    let bins: Vec<BinaryCaps> = caps
        .as_ref()
        .map(|c| c.binaries.clone())
        .unwrap_or_default();
    drop(caps);
    Ok(Json(
        templates::builtin()
            .into_iter()
            .map(|t| {
                let applicable: Vec<&BinaryCaps> =
                    bins.iter().filter(|b| t.applies_to(&b.arch)).collect();
                let tpm_unavailable = (t.tpm != TpmNeed::None)
                    .then(|| {
                        applicable
                            .first()
                            .and_then(|b| app.supervisor.tpm_unavailable(b))
                    })
                    .flatten();
                TemplateInfo {
                    arches: applicable.iter().map(|b| b.arch.clone()).collect(),
                    tpm_unavailable,
                    template: t,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PlanBody {
    arch: String,
    name: String,
    /// The user will attach the template's drivers ISO.
    #[serde(default)]
    with_drivers: bool,
}

/// The plan for template `id` on the binary for `arch`.
pub async fn plan_for(
    app: &App,
    id: &str,
    arch: &str,
    name: &str,
    with_drivers: bool,
) -> Result<TemplatePlan, ApiError> {
    let t = templates::builtin()
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such template"))?;
    if !t.applies_to(arch) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the {} template is for {}", t.name, t.arch.join(", ")),
        ));
    }
    let caps = app.scanner.caps.read().await;
    let firmware = caps
        .as_ref()
        .map(|c| c.firmware.clone())
        .unwrap_or_default();
    let bin = caps
        .as_ref()
        .and_then(|c| c.binaries.iter().find(|b| b.arch == arch))
        .cloned()
        .ok_or_else(|| {
            bad(
                StatusCode::NOT_FOUND,
                format!("no qemu-system-{arch} installed"),
            )
        })?;
    drop(caps);
    let (default_machine, _) = qemu_ui_core::vm::default_spec("probe", &bin);
    let buses = app
        .supervisor
        .machine_buses(&bin, &default_machine.machine.type_name)
        .await;
    let (mut base, _) = qemu_ui_core::vm::default_spec_on(name, &bin, |_| buses);
    for nic in &mut base.nics {
        nic.mac = Some(qemu_ui_core::vm::mac_from_bytes(rand::random()));
    }
    let host = templates::Host {
        firmware: &firmware,
        tpm: match app.supervisor.tpm_unavailable(&bin) {
            Some(why) => Err(why),
            None => Ok(()),
        },
    };
    Ok(templates::plan(&t, base, &bin, &host, with_drivers))
}

async fn plan(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<PlanBody>,
) -> Result<Json<TemplatePlan>, ApiError> {
    plan_for(&app, &id, &b.arch, &b.name, b.with_drivers)
        .await
        .map(Json)
}

async fn create(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<FromTemplate>,
) -> Response {
    match create_inner(&app, &actor, b).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn create_inner(app: &App, actor: &Actor, b: FromTemplate) -> Result<Response, ApiError> {
    let t = templates::builtin()
        .into_iter()
        .find(|t| t.id == b.template)
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "no such template"))?;
    if t.tpm == TpmNeed::Required && b.spec.tpm.is_none() && !b.accept_without_tpm {
        return Err(bad(
            StatusCode::CONFLICT,
            format!(
                "{} requires a TPM 2.0 and this VM has none; confirm creating it without one",
                t.name
            ),
        ));
    }
    if !(1..=65536).contains(&b.disk.size_gib) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "disk size must be 1 GiB to 64 TiB",
        ));
    }
    crate::api_vms::gate_passthrough(app, actor, &b.spec.name, &[], &b.spec.passthrough)?;
    let q = app
        .supervisor
        .qemu_img()
        .await
        .ok_or_else(|| bad(StatusCode::SERVICE_UNAVAILABLE, "qemu-img is not installed"))?;
    let pools = &app.supervisor.pools;
    let name = pools
        .free_name(&b.disk.pool, &format!("{}-disk0", b.spec.name), "qcow2")
        .map_err(|e| bad(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let disk = ImageRef {
        pool: b.disk.pool.clone(),
        name,
    };
    pools
        .create(&q, &disk, b.disk.size_gib << 30, "qcow2")
        .await
        .map_err(|e| bad(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let spec = templates::attach(&b.spec, b.disk.bus, disk.clone(), b.install, b.drivers);
    match app.supervisor.create(spec).await {
        Ok(d) => {
            crate::audit::record(
                &actor.label(),
                "vm.create-from-template",
                &d.summary.id,
                serde_json::json!({"template": t.id, "disk": disk.name}),
            );
            Ok(Json(d).into_response())
        }
        Err(e) => {
            // Don't leave the new, empty disk behind.
            if let Ok(p) = pools.resolve(&disk) {
                let _ = std::fs::remove_file(p);
            }
            Ok(crate::api_vms::op_err(e).into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b
            .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The gallery lists templates; a plan and a create from it work on real
    /// QEMU (the VM is created, not started, so no installer is needed).
    #[test]
    fn plan_and_create_from_a_template() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("tmpl");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);

            let (s, list) = call(&r, "GET", "/api/v1/templates", None).await;
            assert_eq!(s, 200, "{list}");
            let win = list
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["template"]["id"] == "windows-11")
                .unwrap();
            assert_eq!(win["arches"], json!(["x86_64"]));

            let (s, plan) = call(
                &r,
                "POST",
                "/api/v1/templates/windows-11/plan",
                Some(json!({"arch": "x86_64", "name": "w11"})),
            )
            .await;
            assert_eq!(s, 200, "{plan}");
            let plan: TemplatePlan = serde_json::from_value(plan).unwrap();
            assert_eq!(plan.spec.memory.size_mib, 4096);

            let mut body = json!({
                "template": "windows-11",
                "spec": plan.spec,
                "disk": {"pool": "default", "size_gib": 1, "bus": plan.disk_bus},
            });
            if plan.tpm_missing.is_some() {
                // Without consent the request is refused with the reason.
                let (s, _) =
                    call(&r, "POST", "/api/v1/vms/from-template", Some(body.clone())).await;
                assert_eq!(s, 409);
                body["accept_without_tpm"] = json!(true);
            }
            let (s, vm) = call(&r, "POST", "/api/v1/vms/from-template", Some(body)).await;
            assert_eq!(s, 200, "{vm}");
            let disk = &vm["spec"]["disks"][0];
            assert_eq!(disk["image"]["name"], "w11-disk0.qcow2");
            assert_eq!(disk["boot_index"], 1);
            assert!(
                sv.pools
                    .resolve(&ImageRef {
                        pool: "default".into(),
                        name: "w11-disk0.qcow2".into()
                    })
                    .unwrap()
                    .exists()
            );

            // The new VM compiles; a TPM, if attached, is in the argv.
            let id = vm["summary"]["id"].as_str().unwrap().to_string();
            let (s, c) = call(
                &r,
                "POST",
                "/api/v1/vms/compile",
                Some(json!({"spec": vm["spec"], "vm_id": id})),
            )
            .await;
            assert_eq!(s, 200, "{c}");
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
