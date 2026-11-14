//! Disk buses and live disk changes (M2-09).
//!
//! GET    /host/buses?arch=&machine=     disk-bus support for a machine
//! POST   /vms/{id}/disks                add (hotplug when running)
//! DELETE /vms/{id}/disks/{n}?force=     remove (hot-unplug when running)
//! POST   /vms/{id}/disks/{n}/media      change a CD-ROM's image
//!
//! `{n}` is the disk's id index (`qui-disk<n>`): its pinned slot, or its
//! position for disks that never moved. It stays stable while disks are
//! added and removed, unlike a list position.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use qemu_ui_core::buses::DiskBusSupport;
use qemu_ui_core::spec::{DiskSpec, ImageRef};
use qemu_ui_core::vm::DiskChange;
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/host/buses", get(buses))
        .route("/vms/{id}/disks", post(add))
        .route("/vms/{id}/disks/{n}", delete(remove))
        .route("/vms/{id}/disks/{n}/media", post(media))
}

#[derive(Deserialize)]
struct BusesQuery {
    arch: String,
    machine: String,
}

/// `null` when the machine could not be probed (buses are then unchecked).
async fn buses(
    State(app): State<Arc<App>>,
    Query(q): Query<BusesQuery>,
) -> Result<Json<Option<Vec<DiskBusSupport>>>, OpErr> {
    app.supervisor
        .disk_buses(&q.arch, &q.machine)
        .await
        .map(Json)
        .map_err(op_err)
}

#[derive(Deserialize)]
struct AddBody {
    disk: DiskSpec,
}

async fn add(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<AddBody>,
) -> Result<Json<DiskChange>, OpErr> {
    app.supervisor
        .add_disk(&id, b.disk)
        .await
        .map(Json)
        .map_err(op_err)
}

#[derive(Deserialize)]
struct RemoveQuery {
    #[serde(default)]
    force: bool,
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath((id, n)): UrlPath<(String, usize)>,
    Query(q): Query<RemoveQuery>,
) -> Result<Json<DiskChange>, OpErr> {
    app.supervisor
        .remove_disk(&id, n, q.force)
        .await
        .map(Json)
        .map_err(op_err)
}

#[derive(Deserialize)]
struct MediaBody {
    image: ImageRef,
}

async fn media(
    State(app): State<Arc<App>>,
    UrlPath((id, n)): UrlPath<(String, usize)>,
    Json(b): Json<MediaBody>,
) -> Result<Json<DiskChange>, OpErr> {
    app.supervisor
        .change_media(&id, n, b.image)
        .await
        .map(Json)
        .map_err(op_err)
}
