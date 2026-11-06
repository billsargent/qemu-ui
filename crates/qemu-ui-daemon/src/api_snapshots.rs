//! VM snapshot routes (M2-06).
//!
//! GET    /vms/{id}/snapshots
//! POST   /vms/{id}/snapshots                    {name, description?, with_memory?}  → job
//! POST   /vms/{id}/snapshots/{sid}/revert       {safety_snapshot?}                  → job
//! DELETE /vms/{id}/snapshots/{sid}                                                  → job

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use qemu_ui_core::snapshot::SnapshotList;
use qemu_ui_core::storage::Job;
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/snapshots", get(list).post(take))
        .route("/vms/{id}/snapshots/{sid}/revert", post(revert))
        .route("/vms/{id}/snapshots/{sid}", delete(remove))
}

async fn list(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<SnapshotList>, OpErr> {
    app.supervisor
        .list_snapshots(&id)
        .await
        .map(Json)
        .map_err(op_err)
}

#[derive(Deserialize)]
struct TakeBody {
    name: String,
    #[serde(default)]
    description: String,
    /// Include RAM and device state (VM must be running).
    #[serde(default)]
    with_memory: bool,
}

async fn take(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<TakeBody>,
) -> Result<(StatusCode, Json<Job>), OpErr> {
    app.supervisor
        .take_snapshot(&id, &b.name, &b.description, b.with_memory)
        .await
        .map(|j| (StatusCode::ACCEPTED, Json(j)))
        .map_err(op_err)
}

#[derive(Deserialize)]
struct RevertBody {
    /// Snapshot the current state first (default on).
    #[serde(default = "yes")]
    safety_snapshot: bool,
}

fn yes() -> bool {
    true
}

async fn revert(
    State(app): State<Arc<App>>,
    UrlPath((id, sid)): UrlPath<(String, String)>,
    body: Option<Json<RevertBody>>,
) -> Result<(StatusCode, Json<Job>), OpErr> {
    let safety = body.is_none_or(|Json(b)| b.safety_snapshot);
    app.supervisor
        .revert_snapshot(&id, &sid, safety)
        .await
        .map(|j| (StatusCode::ACCEPTED, Json(j)))
        .map_err(op_err)
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath((id, sid)): UrlPath<(String, String)>,
) -> Result<(StatusCode, Json<Job>), OpErr> {
    app.supervisor
        .delete_snapshot(&id, &sid)
        .await
        .map(|j| (StatusCode::ACCEPTED, Json(j)))
        .map_err(op_err)
}
