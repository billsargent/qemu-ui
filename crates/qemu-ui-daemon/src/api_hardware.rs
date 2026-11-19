//! Hardware editor API (M4-02, M4-05).
//!
//! GET  /host/device-palette?arch=&machine=          addable devices
//! GET  /host/binaries/{id}/devices/{device}/fields  property editor fields
//! POST /vms/hardware {spec, vm_id?}                 the device tree

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::devices::{HardwareTree, PaletteEntry, PropField, prop_fields};
use qemu_ui_core::spec::VmSpec;
use serde::Deserialize;

use crate::api::{ApiError, App, cached_device_properties};
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/host/device-palette", get(palette))
        .route("/host/binaries/{id}/devices/{device}/fields", get(fields))
        .route("/vms/hardware", post(tree))
}

#[derive(Deserialize)]
struct PaletteQuery {
    arch: String,
    machine: String,
}

async fn palette(
    State(app): State<Arc<App>>,
    Query(q): Query<PaletteQuery>,
) -> Result<Json<Vec<PaletteEntry>>, OpErr> {
    app.supervisor
        .device_palette(&q.arch, &q.machine)
        .await
        .map(Json)
        .map_err(op_err)
}

async fn fields(
    State(app): State<Arc<App>>,
    UrlPath((id, device)): UrlPath<(String, String)>,
) -> Result<Json<Vec<PropField>>, ApiError> {
    let props = cached_device_properties(&app, &id, &device).await?;
    Ok(Json(prop_fields(&props)))
}

#[derive(Deserialize)]
struct TreeBody {
    spec: VmSpec,
    #[serde(default)]
    vm_id: Option<String>,
}

async fn tree(
    State(app): State<Arc<App>>,
    Json(b): Json<TreeBody>,
) -> Result<Json<HardwareTree>, OpErr> {
    app.supervisor
        .hardware_tree(b.vm_id.as_deref(), b.spec)
        .await
        .map(Json)
        .map_err(op_err)
}
