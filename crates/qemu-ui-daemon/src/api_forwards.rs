//! Port forwards on NAT adapters (M3-02).
//!
//! POST   /vms/{id}/nics/{i}/forwards       add (live when running)
//! DELETE /vms/{id}/nics/{i}/forwards/{j}   remove (live when running)
//!
//! `{i}` is the NIC's position and `{j}` the forward's position in the saved
//! spec.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::routing::{delete, post};
use axum::{Json, Router};
use qemu_ui_core::spec::PortForward;
use qemu_ui_core::vm::ForwardChange;
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/nics/{i}/forwards", post(add))
        .route("/vms/{id}/nics/{i}/forwards/{j}", delete(remove))
}

#[derive(Deserialize)]
struct AddBody {
    forward: PortForward,
}

async fn add(
    State(app): State<Arc<App>>,
    UrlPath((id, i)): UrlPath<(String, usize)>,
    Json(b): Json<AddBody>,
) -> Result<Json<ForwardChange>, OpErr> {
    app.supervisor
        .add_forward(&id, i, b.forward)
        .await
        .map(Json)
        .map_err(op_err)
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath((id, i, j)): UrlPath<(String, usize, usize)>,
) -> Result<Json<ForwardChange>, OpErr> {
    app.supervisor
        .remove_forward(&id, i, j)
        .await
        .map(Json)
        .map_err(op_err)
}
