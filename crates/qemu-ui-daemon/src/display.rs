//! Display engines (plan.md §8.1, M5-01).
//!
//! Every way of showing a VM's screen is a [`DisplayEngine`]: it says what
//! it can do ([`EngineCaps`]), whether it is usable for a VM right now, and
//! how a browser attaches. The console UI renders its controls from the
//! caps, so a missing feature is absent rather than broken.
//!
//! Engines today: `vnc:novnc` (noVNC over our WebSocket proxy, see
//! `consoles.rs`). Seams for later engines: `spice:native` (a `.vv` file for
//! remote-viewer, M5-05) returns a download instead of a WebSocket in
//! [`Attachment`]; recording (M5-03) flips `caps.record`; guacd (M5-04)
//! is another WebSocket engine. Add them to [`engines`].

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::display::{ConsolePresence, DisplayInfo, EngineCaps, EngineInfo};
use qemu_ui_core::spec::VmSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{ApiError, App};
use crate::consoles::{ConsoleKind, publish_presence};
use crate::supervisor::{Supervisor, VncInfo};

/// How a browser connects to an engine.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "transport", rename_all = "kebab-case")]
pub enum Attachment {
    /// Open `path` as a WebSocket with subprotocol `protocol`.
    Websocket {
        path: String,
        protocol: String,
        /// Matches `ConsoleViewer::id`.
        viewer_id: Option<u64>,
        read_only: bool,
        /// RFB password when QEMU's VNC is on loopback TCP (Windows).
        vnc_password: Option<String>,
    },
    /// Download `url` and open it with a native client.
    Download { url: String, filename: String },
}

pub trait DisplayEngine: Send + Sync {
    /// Stable id, e.g. `vnc:novnc`.
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// What this engine offers for a VM with `spec`.
    fn capabilities(&self, spec: &VmSpec) -> EngineCaps;
    /// `Ok` when a browser can attach now, else why not.
    fn available(&self, sv: &Supervisor, vm: &str, spec: &VmSpec) -> Result<(), String>;
    /// Admit one viewer.
    fn attach(&self, app: &App, vm: &str, read_only: bool) -> Result<Attachment, ApiError>;
}

/// noVNC in the browser, through the daemon's VNC WebSocket proxy.
pub struct NoVnc;

impl DisplayEngine for NoVnc {
    fn id(&self) -> &'static str {
        "vnc:novnc"
    }

    fn label(&self) -> &'static str {
        "Browser (VNC)"
    }

    fn capabilities(&self, spec: &VmSpec) -> EngineCaps {
        EngineCaps {
            // QEMU's vdagent chardev carries the clipboard over VNC.
            clipboard: spec.display.vdagent,
            keys: true,
            scale: true,
            fullscreen: true,
            screenshot: spec.display.device.is_some(),
            // Recordings capture the VNC stream (`recording.rs`).
            record: spec.display.vnc && spec.display.device.is_some(),
            view_only: true,
            take_control: true,
            ..EngineCaps::default()
        }
    }

    fn available(&self, sv: &Supervisor, vm: &str, spec: &VmSpec) -> Result<(), String> {
        if !spec.display.vnc {
            return Err("VNC is turned off for this VM".into());
        }
        if spec.display.device.is_none() {
            return Err("the VM has no display device (headless); use the serial console".into());
        }
        sv.vnc_of(vm)
            .map(|_| ())
            .ok_or_else(|| "the VM is not running".into())
    }

    fn attach(&self, app: &App, vm: &str, read_only: bool) -> Result<Attachment, ApiError> {
        let vnc = app
            .supervisor
            .vnc_of(vm)
            .ok_or_else(|| ApiError(StatusCode::CONFLICT, "the VM is not running".into()))?;
        let (ticket, viewer) = app
            .consoles
            .issue(vm, ConsoleKind::Vnc, read_only)
            .map_err(|m| ApiError(StatusCode::CONFLICT, m))?;
        Ok(Attachment::Websocket {
            path: format!("/ws/vnc/{vm}"),
            protocol: crate::consoles::ticket_protocol_for(&ticket),
            viewer_id: Some(viewer),
            read_only,
            vnc_password: match vnc {
                VncInfo::Tcp { password, .. } => Some(password),
                VncInfo::Unix { .. } => None,
            },
        })
    }
}

/// remote-viewer on the user's machine, through a `.vv` file (M5-05). QEMU
/// serves SPICE on loopback with a per-run password (`spice.rs`).
pub struct SpiceNative;

impl DisplayEngine for SpiceNative {
    fn id(&self) -> &'static str {
        "spice:native"
    }

    fn label(&self) -> &'static str {
        "Native client (SPICE)"
    }

    fn capabilities(&self, spec: &VmSpec) -> EngineCaps {
        EngineCaps {
            // remote-viewer has its own send-key menu, scaling and full
            // screen. The compiled `-spice` adds no usbredir channels, audio
            // device or extra heads yet, so those stay absent.
            keys: true,
            scale: true,
            fullscreen: true,
            screenshot: spec.display.device.is_some(),
            ..EngineCaps::default()
        }
    }

    fn available(&self, sv: &Supervisor, vm: &str, spec: &VmSpec) -> Result<(), String> {
        if !spec.display.spice {
            return Err("SPICE is off for this VM; turn it on in its display settings".into());
        }
        if sv.live_qmp(vm).is_none() {
            return Err("the VM is not running".into());
        }
        crate::spice::lookup(&sv.store, vm)
            .map(|_| ())
            .ok_or_else(|| "this run was started without SPICE; restart the VM".into())
    }

    fn attach(&self, _app: &App, vm: &str, _read_only: bool) -> Result<Attachment, ApiError> {
        Ok(Attachment::Download {
            url: format!("/api/v1/vms/{vm}/console.vv"),
            filename: "console.vv".into(),
        })
    }
}

/// Every engine, preferred first.
pub fn engines() -> Vec<Box<dyn DisplayEngine>> {
    vec![Box::new(NoVnc), Box::new(SpiceNative)]
}

fn engine(id: &str) -> Option<Box<dyn DisplayEngine>> {
    engines().into_iter().find(|e| e.id() == id)
}

/// The spec the VM runs with (live) or will start with.
fn current_spec(sv: &Supervisor, vm: &str) -> Result<VmSpec, ApiError> {
    let d = sv
        .detail(vm)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "no such VM".into()))?;
    Ok(d.live_spec.unwrap_or(d.spec))
}

pub fn display_info(app: &App, vm: &str) -> Result<DisplayInfo, ApiError> {
    let spec = current_spec(&app.supervisor, vm)?;
    let engines = engines()
        .iter()
        .map(|e| {
            let avail = e.available(&app.supervisor, vm, &spec);
            EngineInfo {
                id: e.id().into(),
                label: e.label().into(),
                available: avail.is_ok(),
                reason: avail.err(),
                caps: e.capabilities(&spec),
            }
        })
        .collect();
    Ok(DisplayInfo {
        engines,
        presence: app.consoles.presence(vm),
    })
}

/// A PNG of the VM's screen, via QMP `screendump` (any engine).
pub async fn screenshot(sv: &Supervisor, vm: &str) -> Result<Vec<u8>> {
    let qmp = sv
        .live_qmp(vm)
        .ok_or_else(|| anyhow!("the VM is not running"))?;
    let dir = sv.run_dir(vm);
    let file = dir.join(format!("screen-{:08x}.png", rand::random::<u32>()));
    let path = file.display().to_string();
    let res = qmp
        .execute(
            "screendump",
            Some(json!({"filename": path, "format": "png"})),
        )
        .await
        .context("QEMU could not take a screenshot");
    let png = res.and_then(|_| std::fs::read(&file).context("reading the screenshot"));
    let _ = std::fs::remove_file(&file);
    png
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/display", get(info))
        .route("/vms/{id}/display/control", post(take_control))
        .route("/vms/{id}/display/screenshot", get(screenshot_png))
        .route("/vms/{id}/display/{engine}/attach", post(attach))
}

async fn info(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<DisplayInfo>, ApiError> {
    display_info(&app, &id).map(Json)
}

#[derive(Deserialize, Default)]
struct AttachQuery {
    /// `view` for a watch-only viewer.
    #[serde(default)]
    mode: Option<String>,
}

async fn attach(
    State(app): State<Arc<App>>,
    UrlPath((id, engine_id)): UrlPath<(String, String)>,
    Query(q): Query<AttachQuery>,
) -> Result<Json<Attachment>, ApiError> {
    let e = engine(&engine_id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown display engine".into()))?;
    let spec = current_spec(&app.supervisor, &id)?;
    e.available(&app.supervisor, &id, &spec)
        .map_err(|m| ApiError(StatusCode::CONFLICT, m))?;
    e.attach(&app, &id, q.mode.as_deref() == Some("view"))
        .map(Json)
}

#[derive(Deserialize)]
struct ControlBody {
    viewer_id: u64,
}

async fn take_control(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<ControlBody>,
) -> Result<Json<ConsolePresence>, ApiError> {
    app.consoles
        .take_control(&id, b.viewer_id)
        .map_err(|m| ApiError(StatusCode::CONFLICT, m))?;
    publish_presence(&app, &id);
    Ok(Json(app.consoles.presence(&id)))
}

async fn screenshot_png(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Response {
    match screenshot(&app.supervisor, &id).await {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(png),
        )
            .into_response(),
        Err(e) => ApiError(StatusCode::CONFLICT, format!("{e:#}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn novnc_caps_follow_the_spec() {
        // Any valid spec; the importer keeps this in step with the spec type.
        let mut spec = qemu_ui_core::import_argv::import_argv("qemu-system-x86_64 -m 128")
            .unwrap()
            .spec;
        spec.display.device = Some("VGA".into());
        spec.display.vnc = true;
        spec.display.vdagent = false;
        let c = NoVnc.capabilities(&spec);
        assert!(c.keys && c.screenshot && c.take_control && !c.clipboard && !c.audio);
        spec.display.vdagent = true;
        assert!(NoVnc.capabilities(&spec).clipboard);
        assert!(NoVnc.capabilities(&spec).record);
        let s = SpiceNative.capabilities(&spec);
        assert!(s.keys && !s.usb && !s.audio && !s.multihead && !s.take_control);
        spec.display.device = None;
        assert!(!NoVnc.capabilities(&spec).screenshot);
        assert!(!NoVnc.capabilities(&spec).record);
        assert_eq!(
            engines().iter().map(|e| e.id()).collect::<Vec<_>>(),
            ["vnc:novnc", "spice:native"]
        );
    }
}
