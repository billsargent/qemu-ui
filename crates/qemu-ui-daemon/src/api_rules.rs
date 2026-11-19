//! L2 curated rules (M4-03) and auto-fix (M4-04).
//!
//! GET  /rules     every rule: id, severity, message, docs, fix, covers
//! POST /vms/fix   {spec, rule} → {spec, changed, description}

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::rules::{FixResult, RuleInfo};
use qemu_ui_core::spec::VmSpec;
use serde::Deserialize;

use crate::api::App;
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/rules", get(list))
        .route("/vms/fix", post(fix))
}

async fn list(State(app): State<Arc<App>>) -> Json<Vec<RuleInfo>> {
    Json(app.supervisor.rule_info())
}

#[derive(Deserialize)]
struct FixBody {
    spec: VmSpec,
    rule: String,
}

async fn fix(
    State(app): State<Arc<App>>,
    Json(b): Json<FixBody>,
) -> Result<Json<FixResult>, OpErr> {
    app.supervisor
        .apply_fix(b.spec, &b.rule)
        .await
        .map(Json)
        .map_err(op_err)
}
