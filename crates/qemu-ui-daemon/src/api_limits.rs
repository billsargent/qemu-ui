//! Per-VM resource limits (M6-07).
//!
//! GET /host/limits          how this host enforces limits
//! PUT /vms/{id}/limits      change a VM's limits (live where possible)

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use qemu_ui_core::limits::{LimitsChange, LimitsSupport, ResourceLimits};
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/host/limits", get(support))
        .route("/vms/{id}/limits", put(set))
}

async fn support(State(app): State<Arc<App>>) -> Json<LimitsSupport> {
    Json(app.supervisor.limits_support())
}

#[derive(Deserialize)]
struct Body {
    limits: ResourceLimits,
}

async fn set(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<Body>,
) -> Result<Json<LimitsChange>, OpErr> {
    app.supervisor
        .set_limits(&id, b.limits)
        .await
        .map(Json)
        .map_err(op_err)
}
