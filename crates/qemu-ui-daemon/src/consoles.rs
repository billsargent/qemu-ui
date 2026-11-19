//! Browser consoles (plan.md §8.2, M1-11/M1-12): WebSocket proxies for VNC
//! and the serial port.
//!
//! * Access is by single-use tickets that expire after 30 s, obtained from
//!   the API and passed as a WebSocket subprotocol (`qui-ticket.<ticket>`),
//!   never in the URL. The `Origin` header must match the `Host`.
//! * VNC: one upstream connection per viewer (QEMU's VNC server supports
//!   several clients). A VM's viewers form a session (M5-06): one holds
//!   input control; the proxy splits every viewer's stream into RFB
//!   messages (`qemu_ui_core::rfb`) and forwards input only from the
//!   controller, so others can watch without typing. Watch-only tickets can
//!   never take control. Idle viewers are disconnected, each VM takes a
//!   bounded number of viewers, and presence changes go out as transient
//!   `console-presence` events.
//! * Serial: QEMU's socket chardev serves one client at a time, so the
//!   daemon keeps a single connection per VM and fans it out (`SerialHub`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

use qemu_ui_core::display::{ConsolePresence, ConsoleViewer};
use qemu_ui_core::rfb::ClientStream;

use crate::api::{ApiError, App};
use crate::supervisor::VncInfo;

const TICKET_TTL: Duration = Duration::from_secs(30);
const PROTOCOL_PREFIX: &str = "qui-ticket.";
/// Most graphical-console viewers per VM (connected plus ticketed).
pub const MAX_VIEWERS: usize = 8;
/// A viewer with no input for this long is disconnected.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleKind {
    Vnc,
    Serial,
}

#[derive(Debug, Clone)]
pub(crate) struct Ticket {
    vm_id: String,
    kind: ConsoleKind,
    expires: Instant,
    /// Watch-only viewer.
    read_only: bool,
    /// The viewer this ticket admits (VNC).
    viewer: u64,
}

/// One person watching a VM's display.
struct Viewer {
    id: u64,
    label: String,
    read_only: bool,
    since_ms: u64,
    joined: Instant,
    last_input: Option<Instant>,
    input_dropped: u64,
}

/// The viewers of one VM's graphical console.
#[derive(Default)]
struct Session {
    viewers: Vec<Viewer>,
    controller: Option<u64>,
    /// For "Viewer N" labels; restarts when the session empties.
    next_label: u32,
}

pub struct Consoles {
    tickets: Mutex<HashMap<String, Ticket>>,
    hubs: Mutex<HashMap<String, Arc<SerialHub>>>,
    sessions: Mutex<HashMap<String, Session>>,
    next_viewer: AtomicU64,
    idle_timeout: Mutex<Duration>,
}

impl Default for Consoles {
    fn default() -> Self {
        Consoles {
            tickets: Mutex::default(),
            hubs: Mutex::default(),
            sessions: Mutex::default(),
            next_viewer: AtomicU64::new(1),
            idle_timeout: Mutex::new(IDLE_TIMEOUT),
        }
    }
}

impl Consoles {
    /// A single-use ticket and the viewer id it admits. VNC tickets count
    /// against the VM's viewer limit until they expire or are used.
    pub(crate) fn issue(
        &self,
        vm_id: &str,
        kind: ConsoleKind,
        read_only: bool,
    ) -> Result<(String, u64), String> {
        let ticket: String = (0..32)
            .map(|_| format!("{:02x}", rand::random::<u8>()))
            .collect();
        let mut t = self.tickets.lock().unwrap();
        t.retain(|_, v| v.expires > Instant::now());
        if kind == ConsoleKind::Vnc {
            let pending = t
                .values()
                .filter(|v| v.kind == ConsoleKind::Vnc && v.vm_id == vm_id)
                .count();
            let connected = self
                .sessions
                .lock()
                .unwrap()
                .get(vm_id)
                .map_or(0, |s| s.viewers.len());
            if pending + connected >= MAX_VIEWERS {
                return Err(format!(
                    "this display already has {MAX_VIEWERS} viewers; close one first"
                ));
            }
        }
        let viewer = self.next_viewer.fetch_add(1, Ordering::Relaxed);
        t.insert(
            ticket.clone(),
            Ticket {
                vm_id: vm_id.into(),
                kind,
                expires: Instant::now() + TICKET_TTL,
                read_only,
                viewer,
            },
        );
        Ok((ticket, viewer))
    }

    /// Consume a ticket: valid once, for one VM and console kind.
    fn redeem(&self, ticket: &str, vm_id: &str, kind: ConsoleKind) -> Option<Ticket> {
        let t = self.tickets.lock().unwrap().remove(ticket)?;
        (t.vm_id == vm_id && t.kind == kind && t.expires > Instant::now()).then_some(t)
    }

    /// Change how long a viewer may go without input (tests).
    #[cfg(test)]
    pub(crate) fn set_idle_timeout(&self, d: Duration) {
        *self.idle_timeout.lock().unwrap() = d;
    }

    fn idle_timeout(&self) -> Duration {
        *self.idle_timeout.lock().unwrap()
    }

    /// Who is watching `vm_id`.
    pub fn presence(&self, vm_id: &str) -> ConsolePresence {
        let sessions = self.sessions.lock().unwrap();
        let viewers = sessions.get(vm_id).map_or_else(Vec::new, |s| {
            s.viewers
                .iter()
                .map(|v| ConsoleViewer {
                    id: v.id,
                    label: v.label.clone(),
                    read_only: v.read_only,
                    controlling: s.controller == Some(v.id),
                    since_ms: v.since_ms,
                    input_dropped: v.input_dropped,
                })
                .collect()
        });
        ConsolePresence {
            viewers,
            limit: MAX_VIEWERS as u32,
            idle_timeout_secs: self.idle_timeout().as_secs(),
        }
    }

    /// A viewer connected. The first one able to type gets control.
    fn join(&self, vm_id: &str, t: &Ticket) {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.entry(vm_id.to_string()).or_default();
        s.next_label += 1;
        s.viewers.push(Viewer {
            id: t.viewer,
            label: format!("Viewer {}", s.next_label),
            read_only: t.read_only,
            since_ms: now_ms(),
            joined: Instant::now(),
            last_input: None,
            input_dropped: 0,
        });
        if s.controller.is_none() && !t.read_only {
            s.controller = Some(t.viewer);
        }
    }

    /// A viewer left. Control passes to the longest-connected viewer who can
    /// type, so a session is never stuck with nobody at the keyboard.
    fn leave(&self, vm_id: &str, viewer: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(vm_id) else {
            return;
        };
        s.viewers.retain(|v| v.id != viewer);
        if s.controller == Some(viewer) {
            s.controller = s.viewers.iter().find(|v| !v.read_only).map(|v| v.id);
        }
        if s.viewers.is_empty() {
            sessions.remove(vm_id);
        }
    }

    /// Give input control to `viewer`.
    pub fn take_control(&self, vm_id: &str, viewer: u64) -> Result<(), String> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions
            .get_mut(vm_id)
            .ok_or("nobody is viewing this display")?;
        let v = s
            .viewers
            .iter()
            .find(|v| v.id == viewer)
            .ok_or("no such viewer")?;
        if v.read_only {
            return Err("this viewer joined watch-only and cannot take control".into());
        }
        s.controller = Some(viewer);
        Ok(())
    }

    /// Account for one input message; returns whether to forward it.
    fn admit_input(&self, vm_id: &str, viewer: u64) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(vm_id) else {
            return false;
        };
        let controlling = s.controller == Some(viewer);
        if let Some(v) = s.viewers.iter_mut().find(|v| v.id == viewer) {
            v.last_input = Some(Instant::now());
            if !controlling {
                v.input_dropped += 1;
            }
        }
        controlling
    }

    fn is_controller(&self, vm_id: &str, viewer: u64) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(vm_id)
            .is_some_and(|s| s.controller == Some(viewer))
    }

    /// Why this viewer must disconnect now, if it must.
    fn must_close(&self, vm_id: &str, viewer: u64) -> Option<String> {
        let idle = self.idle_timeout();
        let sessions = self.sessions.lock().unwrap();
        let v = sessions
            .get(vm_id)?
            .viewers
            .iter()
            .find(|v| v.id == viewer)?;
        let quiet = v.last_input.unwrap_or(v.joined).elapsed();
        (quiet >= idle).then(|| {
            let mins = idle.as_secs() / 60;
            if mins > 0 {
                format!("disconnected after {mins} minutes without input")
            } else {
                "disconnected: idle".to_string()
            }
        })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Tell browsers who is watching (a transient event; not stored).
pub(crate) fn publish_presence(app: &App, vm_id: &str) {
    let p = app.consoles.presence(vm_id);
    app.supervisor.notify(
        vm_id,
        "console-presence",
        serde_json::to_value(p).unwrap_or_default(),
    );
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/vms/{id}/consoles/{kind}/ticket", post(ticket))
}

pub fn ws_routes() -> Router<Arc<App>> {
    Router::new()
        .route("/ws/vnc/{id}", get(vnc_ws))
        .route("/ws/serial/{id}", get(serial_ws))
}

#[derive(Serialize)]
struct TicketResponse {
    ticket: String,
    /// WebSocket subprotocol to request.
    protocol: String,
    /// VNC password when QEMU's VNC is on loopback TCP (Windows). The browser
    /// is already authenticated to get here; M6 replaces this with
    /// proxy-side RFB authentication.
    vnc_password: Option<String>,
    /// The viewer this ticket admits (VNC); matches `ConsoleViewer::id`.
    viewer_id: Option<u64>,
    read_only: bool,
}

#[derive(Deserialize, Default)]
struct TicketQuery {
    /// `view` for a watch-only viewer.
    #[serde(default)]
    mode: Option<String>,
}

async fn ticket(
    State(app): State<Arc<App>>,
    UrlPath((id, kind)): UrlPath<(String, String)>,
    Query(q): Query<TicketQuery>,
) -> Result<Json<TicketResponse>, ApiError> {
    let kind = match kind.as_str() {
        "vnc" => ConsoleKind::Vnc,
        "serial" => ConsoleKind::Serial,
        _ => return Err(ApiError(StatusCode::NOT_FOUND, "unknown console".into())),
    };
    let not_running = || ApiError(StatusCode::CONFLICT, "the VM is not running".into());
    let vnc_password = match kind {
        ConsoleKind::Vnc => match app.supervisor.vnc_of(&id).ok_or_else(not_running)? {
            VncInfo::Tcp { password, .. } => Some(password),
            VncInfo::Unix { .. } => None,
        },
        ConsoleKind::Serial => {
            app.supervisor.serial_socket(&id).ok_or_else(not_running)?;
            None
        }
    };
    let read_only = q.mode.as_deref() == Some("view");
    let (ticket, viewer) = app
        .consoles
        .issue(&id, kind, read_only)
        .map_err(|m| ApiError(StatusCode::CONFLICT, m))?;
    Ok(Json(TicketResponse {
        protocol: format!("{PROTOCOL_PREFIX}{ticket}"),
        ticket,
        vnc_password,
        viewer_id: (kind == ConsoleKind::Vnc).then_some(viewer),
        read_only,
    }))
}

/// Same-origin check: `Origin` must name the host the request was sent to.
fn origin_ok(headers: &HeaderMap) -> bool {
    let host = headers.get("host").and_then(|h| h.to_str().ok());
    let origin = headers.get("origin").and_then(|h| h.to_str().ok());
    match (host, origin) {
        (Some(host), Some(origin)) => origin
            .split_once("://")
            .is_some_and(|(_, rest)| rest.trim_end_matches('/') == host),
        // Non-browser clients send no Origin; the ticket still applies.
        (_, None) => true,
        _ => false,
    }
}

/// The WebSocket subprotocol that presents `ticket`.
pub(crate) fn ticket_protocol_for(ticket: &str) -> String {
    format!("{PROTOCOL_PREFIX}{ticket}")
}

/// Find our ticket among the requested subprotocols.
fn ticket_protocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find(|p| p.starts_with(PROTOCOL_PREFIX))
        .map(String::from)
}

fn authorize(
    app: &App,
    headers: &HeaderMap,
    id: &str,
    kind: ConsoleKind,
) -> Result<(String, Ticket), ApiError> {
    if !origin_ok(headers) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "cross-origin console request".into(),
        ));
    }
    let proto = ticket_protocol(headers)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "missing console ticket".into()))?;
    let ticket = app
        .consoles
        .redeem(&proto[PROTOCOL_PREFIX.len()..], id, kind)
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "invalid or expired console ticket".into(),
            )
        })?;
    Ok((proto, ticket))
}

async fn vnc_ws(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let (proto, ticket) = match authorize(&app, &headers, &id, ConsoleKind::Vnc) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let Some(vnc) = app.supervisor.vnc_of(&id) else {
        return ApiError(StatusCode::CONFLICT, "the VM is not running".into()).into_response();
    };
    // noVNC also asks for "binary"; answering with our ticket protocol is fine.
    ws.protocols([proto]).on_upgrade(move |socket| async move {
        let upstream: std::io::Result<Box<dyn qemu_ui_qmp::transport::Stream>> = match vnc {
            VncInfo::Unix { path } => {
                qemu_ui_qmp::transport::connect_unix(std::path::Path::new(&path)).await
            }
            VncInfo::Tcp { port, .. } => tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .map(|s| {
                    let _ = s.set_nodelay(true);
                    Box::new(s) as Box<dyn qemu_ui_qmp::transport::Stream>
                }),
        };
        let up = match upstream {
            Ok(up) => up,
            Err(e) => return close_with(socket, &format!("VNC unavailable: {e}")).await,
        };
        app.consoles.join(&id, &ticket);
        publish_presence(&app, &id);
        pump_vnc(socket, up, &app, &id, ticket.viewer).await;
        app.consoles.leave(&id, ticket.viewer);
        publish_presence(&app, &id);
    })
}

/// Shuttle one viewer's VNC traffic. Server→browser bytes pass untouched.
/// Browser→server bytes are split into RFB messages, and input goes through
/// only while this viewer holds control.
async fn pump_vnc(
    socket: WebSocket,
    upstream: Box<dyn qemu_ui_qmp::transport::Stream>,
    app: &App,
    vm: &str,
    viewer: u64,
) {
    let (mut up_r, mut up_w) = tokio::io::split(upstream);
    let (mut ws_tx, mut ws_rx) = socket.split();
    // A reason sent here closes the browser side with it.
    let (close_tx, mut close_rx) = mpsc::channel::<String>(1);
    let to_browser = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            tokio::select! {
                n = up_r.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_tx
                            .send(Message::Binary(buf[..n].to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                },
                reason = close_rx.recv() => {
                    let _ = ws_tx
                        .send(Message::Close(Some(CloseFrame {
                            code: 4000,
                            reason: reason.unwrap_or_default().into(),
                        })))
                        .await;
                    break;
                }
            }
        }
        let _ = ws_tx.close().await;
    };
    let to_upstream = async {
        let mut parser = ClientStream::new();
        // A stream that cannot be split is forwarded whole, and only while
        // this viewer holds control.
        let mut opaque = false;
        while let Some(Ok(msg)) = ws_rx.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            let mut out = Vec::with_capacity(data.len());
            if opaque {
                if !app.consoles.is_controller(vm, viewer) {
                    let _ = close_tx
                        .send("this viewer's client can't watch without control".into())
                        .await;
                    return std::future::pending::<()>().await;
                }
                out = data;
            } else {
                for (kind, bytes) in parser.push(&data) {
                    if kind.is_input() && !app.consoles.admit_input(vm, viewer) {
                        continue;
                    }
                    out.extend(bytes);
                }
                if let Some(e) = parser.error() {
                    tracing::debug!(vm, viewer, "VNC client stream not parseable: {e}");
                    opaque = true;
                    if app.consoles.is_controller(vm, viewer) {
                        out.extend(parser.take_rest());
                    }
                }
            }
            if !out.is_empty() && up_w.write_all(&out).await.is_err() {
                break;
            }
        }
        let _ = up_w.shutdown().await;
    };
    let watchdog = async {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tick.tick().await;
            if let Some(reason) = app.consoles.must_close(vm, viewer) {
                let _ = close_tx.send(reason).await;
                return std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = to_browser => {},
        _ = to_upstream => {},
        _ = watchdog => {},
    }
}

async fn close_with(mut socket: WebSocket, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1011,
            reason: reason.to_string().into(),
        })))
        .await;
}

// ------------------------------------------------------------------ serial

/// One upstream serial connection per VM, shared by every viewer.
pub struct SerialHub {
    output: broadcast::Sender<Vec<u8>>,
    input: mpsc::Sender<Vec<u8>>,
}

impl SerialHub {
    async fn connect(path: PathBuf) -> std::io::Result<Arc<SerialHub>> {
        let stream = qemu_ui_qmp::transport::connect_unix(&path).await?;
        let (mut r, mut w) = tokio::io::split(stream);
        let (output, _) = broadcast::channel(1024);
        let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(256);
        let out = output.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                tokio::select! {
                    n = r.read(&mut buf) => match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => { let _ = out.send(buf[..n].to_vec()); }
                    },
                    msg = input_rx.recv() => match msg {
                        Some(data) => { if w.write_all(&data).await.is_err() { break; } }
                        None => break, // every viewer and the hub entry are gone
                    },
                }
            }
        });
        Ok(Arc::new(SerialHub { output, input }))
    }
}

async fn serial_ws(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let proto = match authorize(&app, &headers, &id, ConsoleKind::Serial) {
        Ok((p, _)) => p,
        Err(r) => return r.into_response(),
    };
    let Some(sock) = app.supervisor.serial_socket(&id) else {
        return ApiError(StatusCode::CONFLICT, "the VM is not running".into()).into_response();
    };
    ws.protocols([proto]).on_upgrade(move |socket| async move {
        let hub = {
            let existing = app.consoles.hubs.lock().unwrap().get(&id).cloned();
            match existing {
                Some(h) if !h.input.is_closed() => Ok(h),
                _ => SerialHub::connect(sock).await.inspect(|h| {
                    app.consoles
                        .hubs
                        .lock()
                        .unwrap()
                        .insert(id.clone(), h.clone());
                }),
            }
        };
        let hub = match hub {
            Ok(h) => h,
            Err(e) => return close_with(socket, &format!("serial unavailable: {e}")).await,
        };
        let mut output = hub.output.subscribe();
        let (mut ws_tx, mut ws_rx) = socket.split();
        let to_browser = async {
            loop {
                match output.recv().await {
                    Ok(data) => {
                        if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            let _ = ws_tx.close().await;
        };
        let to_vm = async {
            while let Some(Ok(msg)) = ws_rx.next().await {
                let data = match msg {
                    Message::Binary(b) => b.to_vec(),
                    Message::Text(t) => t.as_bytes().to_vec(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                if hub.input.send(data).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = to_browser => {},
            _ = to_vm => {},
        }
        // Drop the hub when its last viewer leaves, so QEMU's chardev is free
        // and a stopped VM does not keep a stale connection.
        let mut hubs = app.consoles.hubs.lock().unwrap();
        if hubs
            .get(&id)
            .is_some_and(|h| Arc::ptr_eq(h, &hub) && Arc::strong_count(&hub) <= 2)
        {
            hubs.remove(&id);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn tickets_are_single_use_and_scoped() {
        let c = Consoles::default();
        let issue = |vm: &str, k| c.issue(vm, k, false).unwrap().0;
        let t = issue("vm1", ConsoleKind::Vnc);
        assert!(c.redeem(&t, "vm2", ConsoleKind::Vnc).is_none(), "wrong VM");
        let t = issue("vm1", ConsoleKind::Vnc);
        assert!(
            c.redeem(&t, "vm1", ConsoleKind::Serial).is_none(),
            "wrong kind"
        );
        let t = issue("vm1", ConsoleKind::Vnc);
        assert!(c.redeem(&t, "vm1", ConsoleKind::Vnc).is_some());
        assert!(
            c.redeem(&t, "vm1", ConsoleKind::Vnc).is_none(),
            "single use"
        );
        assert!(c.redeem("nope", "vm1", ConsoleKind::Vnc).is_none());
    }

    fn joined(c: &Consoles, vm: &str, read_only: bool) -> u64 {
        let (t, _) = c.issue(vm, ConsoleKind::Vnc, read_only).unwrap();
        let t = c.redeem(&t, vm, ConsoleKind::Vnc).unwrap();
        c.join(vm, &t);
        t.viewer
    }

    #[test]
    fn control_passes_between_viewers() {
        let c = Consoles::default();
        let watcher = joined(&c, "vm", true);
        assert_eq!(c.presence("vm").viewers[0].label, "Viewer 1");
        assert!(!c.is_controller("vm", watcher), "watch-only never controls");
        let a = joined(&c, "vm", false);
        let b = joined(&c, "vm", false);
        assert!(c.is_controller("vm", a), "first viewer who can type");
        assert!(c.admit_input("vm", a));
        assert!(!c.admit_input("vm", b));
        assert!(!c.admit_input("vm", watcher));
        let p = c.presence("vm");
        let dropped: Vec<u64> = p.viewers.iter().map(|v| v.input_dropped).collect();
        assert_eq!(dropped, [1, 0, 1]);

        c.take_control("vm", b).unwrap();
        assert!(!c.admit_input("vm", a) && c.admit_input("vm", b));
        assert!(c.take_control("vm", watcher).is_err());
        assert!(c.take_control("vm", 9999).is_err());

        // The controller leaves: the longest-connected typist takes over.
        c.leave("vm", b);
        assert!(c.is_controller("vm", a));
        c.leave("vm", a);
        assert!(c.presence("vm").viewers.iter().all(|v| !v.controlling));
        c.leave("vm", watcher);
        assert!(c.presence("vm").viewers.is_empty());
        assert_eq!(joined(&c, "vm", false), 4, "ids keep growing");
        assert_eq!(
            c.presence("vm").viewers[0].label,
            "Viewer 1",
            "labels restart"
        );
    }

    #[test]
    fn viewer_limit_counts_pending_tickets() {
        let c = Consoles::default();
        for _ in 0..MAX_VIEWERS {
            c.issue("vm", ConsoleKind::Vnc, true).unwrap();
        }
        assert!(c.issue("vm", ConsoleKind::Vnc, true).is_err());
        assert!(c.issue("other", ConsoleKind::Vnc, true).is_ok(), "per VM");
        assert!(
            c.issue("vm", ConsoleKind::Serial, false).is_ok(),
            "serial is separate"
        );
    }

    #[test]
    fn idle_viewers_are_closed() {
        let c = Consoles::default();
        let v = joined(&c, "vm", false);
        assert!(c.must_close("vm", v).is_none());
        c.set_idle_timeout(Duration::ZERO);
        assert!(c.must_close("vm", v).unwrap().contains("idle"));
    }

    #[test]
    fn origin_must_match_host() {
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("127.0.0.1:7480"));
        h.insert("origin", HeaderValue::from_static("http://127.0.0.1:7480"));
        assert!(origin_ok(&h));
        h.insert("origin", HeaderValue::from_static("http://evil.example"));
        assert!(!origin_ok(&h));
        h.remove("origin");
        assert!(origin_ok(&h), "non-browser clients");
    }

    #[test]
    fn finds_ticket_protocol() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("binary, qui-ticket.abc"),
        );
        assert_eq!(ticket_protocol(&h).as_deref(), Some("qui-ticket.abc"));
    }
}

/// Consoles end to end against a real QEMU: ticket → WebSocket → VNC (the
/// RFB server greeting must arrive), ticket reuse and cross-origin are
/// refused, and two serial viewers can share the hub. Skipped without QEMU.
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio_tungstenite::tungstenite::Message as WsMsg;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    async fn ticket(base: &str, id: &str, kind: &str) -> serde_json::Value {
        let url = format!("{base}/api/v1/vms/{id}/consoles/{kind}/ticket");
        tokio::task::spawn_blocking(move || ureq_post(&url))
            .await
            .unwrap()
    }

    fn ureq_post(url: &str) -> serde_json::Value {
        // Minimal HTTP/1.1 POST over std, to avoid another HTTP client dependency.
        use std::io::{Read, Write};
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        let mut s = std::net::TcpStream::connect(host).unwrap();
        let token = "t".repeat(64);
        write!(
            s,
            "POST /{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out.split("\r\n\r\n").nth(1).unwrap_or_default();
        serde_json::from_str(body).unwrap_or_else(|_| panic!("bad response: {out}"))
    }

    async fn connect(
        ws_base: &str,
        path: &str,
        protocol: &str,
        origin: Option<&str>,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    > {
        let mut req = format!("{ws_base}{path}").into_client_request().unwrap();
        req.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(protocol).unwrap(),
        );
        if let Some(o) = origin {
            req.headers_mut()
                .insert("origin", HeaderValue::from_str(o).unwrap());
        }
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn vnc_and_serial_over_websockets() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("con");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "console").await;
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();

            let token = "t".repeat(64);
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), token.clone());
            let router = crate::api::router(app, None);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let base = format!("http://{addr}");
            let ws_base = format!("ws://{addr}");
            let origin = format!("http://{addr}");

            // VNC: greeting arrives through the proxy.
            let t = ticket(&base, &id, "vnc").await;
            let proto = t["protocol"].as_str().unwrap().to_string();
            assert_eq!(t["vnc_password"].is_string(), cfg!(windows));
            let mut ws = connect(&ws_base, &format!("/ws/vnc/{id}"), &proto, Some(&origin))
                .await
                .unwrap();
            let first = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
                .await
                .expect("VNC greeting timed out")
                .unwrap()
                .unwrap();
            let WsMsg::Binary(bytes) = first else {
                panic!("expected binary, got {first:?}")
            };
            assert!(
                bytes.starts_with(b"RFB 003."),
                "{:?}",
                String::from_utf8_lossy(&bytes)
            );
            drop(ws);

            // The same ticket cannot be used twice.
            assert!(
                connect(&ws_base, &format!("/ws/vnc/{id}"), &proto, Some(&origin))
                    .await
                    .is_err()
            );
            // Cross-origin is refused even with a fresh ticket.
            let t = ticket(&base, &id, "vnc").await;
            let evil = connect(
                &ws_base,
                &format!("/ws/vnc/{id}"),
                t["protocol"].as_str().unwrap(),
                Some("http://evil.example"),
            )
            .await;
            assert!(evil.is_err());
            // No ticket at all.
            assert!(
                connect(&ws_base, &format!("/ws/vnc/{id}"), "binary", Some(&origin))
                    .await
                    .is_err()
            );

            // Serial: two viewers share one upstream connection.
            let t1 = ticket(&base, &id, "serial").await;
            let t2 = ticket(&base, &id, "serial").await;
            let s1 = connect(
                &ws_base,
                &format!("/ws/serial/{id}"),
                t1["protocol"].as_str().unwrap(),
                Some(&origin),
            )
            .await
            .unwrap();
            let s2 = connect(
                &ws_base,
                &format!("/ws/serial/{id}"),
                t2["protocol"].as_str().unwrap(),
                Some(&origin),
            )
            .await
            .unwrap();
            // Neither viewer is closed by the server within a moment.
            let (mut s1, mut s2) = (s1, s2);
            for s in [&mut s1, &mut s2] {
                let r = tokio::time::timeout(std::time::Duration::from_millis(500), s.next()).await;
                assert!(
                    r.is_err() || matches!(r, Ok(Some(Ok(WsMsg::Binary(_))))),
                    "viewer closed: {r:?}"
                );
            }
            drop((s1, s2));

            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, qemu_ui_core::vm::VmState::Stopped).await;
            let _ = Arc::strong_count(&sv);
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}

#[cfg(test)]
#[path = "consoles_viewers_test.rs"]
mod viewers_test;
