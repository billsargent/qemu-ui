//! Expert surfaces (plan.md §13, M6-08): a raw QMP console. Root-equivalent,
//! so it needs expert mode (off by default) and every command is audited.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, App};
use crate::auth::Actor;

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/vms/{id}/qmp", post(qmp))
}

#[derive(Deserialize)]
struct QmpCommand {
    execute: String,
    #[serde(default)]
    arguments: Option<Value>,
}

async fn qmp(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Actor>,
    UrlPath(id): UrlPath<String>,
    Json(c): Json<QmpCommand>,
) -> Result<Json<Value>, ApiError> {
    crate::auth::require_expert(&app, &actor)?;
    let qmp = app
        .supervisor
        .live_qmp(&id)
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "the VM is not running".into()))?;
    // Arguments may carry secrets (passwords, keys): log only their names.
    let arg_names: Vec<String> = c
        .arguments
        .as_ref()
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    crate::audit::record(
        &actor,
        "expert.qmp",
        &id,
        json!({"command": c.execute, "arguments": arg_names}),
    );
    match qmp.execute(&c.execute, c.arguments).await {
        Ok(v) => Ok(Json(json!({ "return": v }))),
        Err(e) => Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))),
    }
}
