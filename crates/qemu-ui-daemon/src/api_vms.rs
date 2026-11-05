//! VM, pool and event routes (M1).

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use qemu_ui_core::compile::display_command;
use qemu_ui_core::spec::{ImageRef, VmSpec};
use qemu_ui_core::vm::{CompileResponse, PoolInfo, VmDetail, VmEvent, VmSummary, default_spec};
use serde::Deserialize;
use serde_json::json;

use crate::api::{ApiError, ApiResult, App};
use crate::images::DEFAULT_POOL;
use crate::supervisor::OpError;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms", get(list).post(create))
        .route("/vms/defaults", get(defaults))
        .route("/vms/compile", post(compile_preview))
        .route("/vms/{id}", get(detail).put(update).delete(delete))
        .route("/vms/{id}/actions/{action}", post(action))
        .route("/vms/{id}/serial-log", get(serial_log))
        .route("/vms/{id}/events", get(vm_events))
        .route("/pools/{pool}", get(pool))
        .route("/pools/{pool}/images", post(create_image))
        .route("/events", get(events))
}

/// API wrapper for supervisor errors. Errors with diagnostics are 422 with
/// `{error, diagnostics}`.
pub struct OpErr(OpError);

impl IntoResponse for OpErr {
    fn into_response(self) -> Response {
        match self.0 {
            OpError::Invalid(d) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "the VM configuration is invalid", "diagnostics": d})),
            )
                .into_response(),
            OpError::Conflict(m) => ApiError(StatusCode::CONFLICT, m).into_response(),
            OpError::NotFound => {
                ApiError(StatusCode::NOT_FOUND, "no such VM".into()).into_response()
            }
            OpError::Failed(e) => {
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
            }
        }
    }
}

fn op_err(e: OpError) -> OpErr {
    OpErr(e)
}

type OpResponse<T> = Result<Json<T>, OpErr>;

async fn list(State(app): State<Arc<App>>) -> ApiResult<Vec<VmSummary>> {
    app.supervisor
        .list()
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

#[derive(Deserialize)]
struct SpecBody {
    spec: VmSpec,
}

async fn create(State(app): State<Arc<App>>, Json(b): Json<SpecBody>) -> OpResponse<VmDetail> {
    app.supervisor
        .create(b.spec)
        .await
        .map(Json)
        .map_err(op_err)
}

async fn detail(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> OpResponse<VmDetail> {
    app.supervisor.detail(&id).map(Json).map_err(op_err)
}

async fn update(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<SpecBody>,
) -> OpResponse<VmDetail> {
    app.supervisor
        .update(&id, b.spec)
        .await
        .map(Json)
        .map_err(op_err)
}

async fn delete(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, OpErr> {
    app.supervisor
        .delete(&id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(op_err)
}

async fn action(
    State(app): State<Arc<App>>,
    UrlPath((id, action)): UrlPath<(String, String)>,
) -> OpResponse<VmSummary> {
    let sv = &app.supervisor;
    let r = match action.as_str() {
        "start" => sv.start(&id).await,
        "stop" => sv.stop(&id).await,
        "force-stop" => sv.force_stop(&id).await,
        other => sv.qmp_action(&id, other).await,
    };
    r.map(Json).map_err(op_err)
}

#[derive(Deserialize)]
struct DefaultsQuery {
    arch: String,
    #[serde(default)]
    name: Option<String>,
}

async fn defaults(
    State(app): State<Arc<App>>,
    Query(q): Query<DefaultsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caps = app.scanner.caps.read().await;
    let bin = caps
        .as_ref()
        .and_then(|c| c.binaries.iter().find(|b| b.arch == q.arch))
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("no qemu-system-{} installed", q.arch),
            )
        })?;
    let (spec, diagnostics) = default_spec(q.name.as_deref().unwrap_or("new-vm"), bin);
    Ok(Json(json!({"spec": spec, "diagnostics": diagnostics})))
}

#[derive(Deserialize)]
struct CompileBody {
    spec: VmSpec,
    #[serde(default)]
    vm_id: Option<String>,
}

/// Compile without starting anything: the live argv drawer.
async fn compile_preview(
    State(app): State<Arc<App>>,
    Json(b): Json<CompileBody>,
) -> Json<CompileResponse> {
    match app.supervisor.preview(b.vm_id.as_deref(), b.spec).await {
        Ok((c, warnings)) => Json(CompileResponse {
            command: Some(display_command(&c.program, &c.args)),
            args: Some(c.args),
            diagnostics: warnings,
        }),
        Err(OpError::Invalid(d)) => Json(CompileResponse {
            command: None,
            args: None,
            diagnostics: d,
        }),
        Err(e) => Json(CompileResponse {
            command: None,
            args: None,
            diagnostics: vec![qemu_ui_core::compile::Diagnostic {
                severity: qemu_ui_core::compile::Severity::Error,
                field: String::new(),
                rule: "compile.failed".into(),
                message: match e {
                    OpError::Failed(e) => format!("{e:#}"),
                    OpError::Conflict(m) => m,
                    _ => "cannot compile".into(),
                },
            }],
        }),
    }
}

/// Recent events of one VM, oldest first.
async fn vm_events(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult<Vec<VmEvent>> {
    let rows = app
        .supervisor
        .store
        .events_for_vm(&id, 200)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(
        rows.into_iter()
            .map(|(seq, vm_id, at_us, kind, data)| VmEvent {
                seq,
                vm_id,
                at_us,
                kind,
                data: serde_json::from_str(&data).unwrap_or_default(),
            })
            .collect(),
    ))
}

/// Serial output so far (from the persistent serial log), for scrollback.
async fn serial_log(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<String, ApiError> {
    let path: PathBuf = app.supervisor.serial_log(&id);
    let bytes = std::fs::read(&path).unwrap_or_default();
    // Last 256 KiB is plenty for scrollback.
    let start = bytes.len().saturating_sub(256 * 1024);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

async fn pool(State(app): State<Arc<App>>, UrlPath(pool): UrlPath<String>) -> ApiResult<PoolInfo> {
    let qemu_img = {
        let caps = app.scanner.caps.read().await;
        caps.as_ref()
            .and_then(|c| c.qemu_img.as_ref())
            .map(|b| PathBuf::from(&b.path))
    };
    let mut info = app
        .supervisor
        .pools
        .list(qemu_img.as_deref(), &pool)
        .await
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    // Which VMs use each image.
    if let Ok(rows) = app.supervisor.store.list_vms() {
        for row in rows {
            let Ok(spec) = serde_json::from_str::<VmSpec>(&row.spec_json) else {
                continue;
            };
            for d in &spec.disks {
                if let Some(img) = info
                    .images
                    .iter_mut()
                    .find(|i| i.pool == d.image.pool && i.name == d.image.name)
                    && !img.used_by.contains(&row.name)
                {
                    img.used_by.push(row.name.clone());
                }
            }
        }
    }
    Ok(Json(info))
}

#[derive(Deserialize)]
struct CreateImage {
    name: String,
    size_bytes: u64,
    #[serde(default = "qcow2")]
    format: String,
}

fn qcow2() -> String {
    "qcow2".into()
}

async fn create_image(
    State(app): State<Arc<App>>,
    UrlPath(pool): UrlPath<String>,
    Json(b): Json<CreateImage>,
) -> Result<StatusCode, ApiError> {
    if pool != DEFAULT_POOL {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("unknown pool '{pool}'"),
        ));
    }
    let qemu_img = {
        let caps = app.scanner.caps.read().await;
        caps.as_ref()
            .and_then(|c| c.qemu_img.as_ref())
            .map(|b| PathBuf::from(&b.path))
    }
    .ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "qemu-img is not installed".into(),
        )
    })?;
    app.supervisor
        .pools
        .create(
            &qemu_img,
            &ImageRef { pool, name: b.name },
            b.size_bytes,
            &b.format,
        )
        .await
        .map(|_| StatusCode::CREATED)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Replay events after this sequence number (alternative to Last-Event-ID).
    #[serde(default)]
    after: Option<i64>,
}

/// Server-sent events: replay from the event log after the client's cursor,
/// then stream live (plan.md §7.3). Clients reconnect with `Last-Event-ID`.
async fn events(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .or(q.after);
    // Subscribe before replaying so nothing falls in the gap.
    let mut live = app.supervisor.subscribe();
    let store = app.supervisor.store.clone();

    let stream = async_stream::stream! {
        let mut last = cursor.unwrap_or(0);
        if cursor.is_some() {
            loop {
                let batch = store.events_after(last, 500).unwrap_or_default();
                let n = batch.len();
                for (seq, vm_id, at_us, kind, data) in batch {
                    last = seq;
                    let ev = VmEvent { seq, vm_id, at_us, kind, data: serde_json::from_str(&data).unwrap_or_default() };
                    yield Ok(to_sse(&ev));
                }
                if n < 500 { break; }
            }
        }
        loop {
            match live.recv().await {
                // seq 0 = not persisted (e.g. deletion notices); always forward.
                Ok(ev) if ev.seq == 0 || ev.seq > last => {
                    if ev.seq > 0 { last = ev.seq; }
                    yield Ok(to_sse(&ev));
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Tell the client to resync from its last id.
                    yield Ok(Event::default().event("resync").data("{}"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn to_sse(ev: &VmEvent) -> Event {
    let e = Event::default().data(serde_json::to_string(ev).unwrap_or_default());
    if ev.seq > 0 {
        e.id(ev.seq.to_string())
    } else {
        e
    }
}
