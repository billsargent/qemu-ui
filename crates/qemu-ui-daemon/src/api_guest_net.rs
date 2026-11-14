//! Guest network routes (M3-08).
//!
//! GET /vms/{id}/guest-net → `GuestNetInfo` (addresses per NIC, best first)

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::guest_net::GuestNetInfo;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/vms/{id}/guest-net", get(guest_net))
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
