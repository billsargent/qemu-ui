//! NIC models per machine (M3-07).
//!
//! GET /host/nic-models?arch=&machine=   models for the machine, preferred first

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::nic::NicModel;
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/host/nic-models", get(models))
}

#[derive(Deserialize)]
struct ModelsQuery {
    arch: String,
    machine: String,
}

async fn models(
    State(app): State<Arc<App>>,
    Query(q): Query<ModelsQuery>,
) -> Result<Json<Vec<NicModel>>, OpErr> {
    app.supervisor
        .nic_models(&q.arch, &q.machine)
        .await
        .map(Json)
        .map_err(op_err)
}
