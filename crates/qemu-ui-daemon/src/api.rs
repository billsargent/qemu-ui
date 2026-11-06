//! HTTP API (`/api/v1`): host capabilities (M0) and VMs (M1). VM routes
//! live in `api_vms`. Serves the built SPA from `--web-dir` when present.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::caps::{CapabilitiesResponse, DeviceProperty, ScanStatus};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;
use tower_http::services::{ServeDir, ServeFile};

use crate::probe;
use crate::scan::Scanner;
use crate::supervisor::Supervisor;

pub struct App {
    pub scanner: Arc<Scanner>,
    pub supervisor: Arc<Supervisor>,
    pub consoles: crate::consoles::Consoles,
    pub uploads: Arc<crate::api_upload::Uploads>,
    /// Local API token for non-browser clients (see `guard`).
    pub token: String,
    props: Mutex<HashMap<(String, String), Vec<DeviceProperty>>>,
}

impl App {
    pub fn new(scanner: Arc<Scanner>, supervisor: Arc<Supervisor>, token: String) -> Arc<Self> {
        Arc::new(App {
            scanner,
            uploads: crate::api_upload::Uploads::new(supervisor.clone()),
            supervisor,
            token,
            consoles: crate::consoles::Consoles::default(),
            props: Mutex::default(),
        })
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(app: Arc<App>, web_dir: Option<PathBuf>) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/host/capabilities", get(capabilities))
        .route("/host/rescan", post(rescan))
        .route(
            "/host/binaries/{id}/devices/{device}/properties",
            get(device_properties),
        )
        .merge(crate::api_vms::routes())
        .merge(crate::api_storage::routes())
        .merge(crate::image_ops::routes())
        .merge(crate::api_snapshots::routes())
        .merge(crate::api_upload::routes())
        .merge(crate::consoles::routes())
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "no such endpoint".into()) })
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            crate::guard::api_guard,
        ))
        .with_state(app.clone());
    let router = Router::new()
        .nest("/api/v1", api)
        .merge(crate::consoles::ws_routes().with_state(app));
    let router = match web_dir {
        Some(dir) if dir.join("index.html").is_file() => {
            let index = dir.join("index.html");
            router.fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(index)))
        }
        _ => router.fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                "Web UI not built. Run `npm run build` in web/, or use the dev server (`npm run dev`).",
            )
        }),
    };
    router.layer(axum::middleware::from_fn(crate::guard::host_guard))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn capabilities(State(app): State<Arc<App>>) -> ApiResult<CapabilitiesResponse> {
    Ok(Json(CapabilitiesResponse {
        caps: app.scanner.caps.read().await.clone(),
        scan: app.scanner.status.read().await.clone(),
    }))
}

#[derive(Deserialize)]
struct RescanQuery {
    #[serde(default)]
    force: bool,
}

async fn rescan(
    State(app): State<Arc<App>>,
    Query(q): Query<RescanQuery>,
) -> (StatusCode, Json<ScanStatus>) {
    if q.force {
        app.props.lock().await.clear();
    }
    (StatusCode::ACCEPTED, Json(app.scanner.start(q.force).await))
}

async fn device_properties(
    State(app): State<Arc<App>>,
    UrlPath((id, device)): UrlPath<(String, String)>,
) -> ApiResult<Vec<DeviceProperty>> {
    let key = (id.clone(), device.clone());
    if let Some(p) = app.props.lock().await.get(&key) {
        return Ok(Json(p.clone()));
    }
    let path = {
        let caps = app.scanner.caps.read().await;
        let bin = caps
            .as_ref()
            .and_then(|c| c.binaries.iter().find(|b| b.binary.id == id))
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown binary {id}")))?;
        // Only device names the probe reported can be passed to QEMU.
        if !bin.devices.iter().any(|d| d.name == device) {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown device {device}"),
            ));
        }
        bin.binary.path.clone()
    };
    let props = probe::device_properties(Path::new(&path), &device)
        .await
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    app.props.lock().await.insert(key, props.clone());
    Ok(Json(props))
}
