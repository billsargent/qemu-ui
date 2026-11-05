//! Browser consoles (plan.md §8.2, M1-11/M1-12): WebSocket proxies for VNC
//! and the serial port.
//!
//! * Access is by single-use tickets that expire after 30 s, obtained from
//!   the API and passed as a WebSocket subprotocol (`qui-ticket.<ticket>`),
//!   never in the URL. The `Origin` header must match the `Host`.
//! * VNC: one upstream connection per viewer (QEMU's VNC server supports
//!   several clients).
//! * Serial: QEMU's socket chardev serves one client at a time, so the
//!   daemon keeps a single connection per VM and fans it out (`SerialHub`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};

use crate::api::{ApiError, App};
use crate::supervisor::VncInfo;

const TICKET_TTL: Duration = Duration::from_secs(30);
const PROTOCOL_PREFIX: &str = "qui-ticket.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleKind {
    Vnc,
    Serial,
}

struct Ticket {
    vm_id: String,
    kind: ConsoleKind,
    expires: Instant,
}

#[derive(Default)]
pub struct Consoles {
    tickets: Mutex<HashMap<String, Ticket>>,
    hubs: Mutex<HashMap<String, Arc<SerialHub>>>,
}

impl Consoles {
    fn issue(&self, vm_id: &str, kind: ConsoleKind) -> String {
        let ticket: String = (0..32)
            .map(|_| format!("{:02x}", rand::random::<u8>()))
            .collect();
        let mut t = self.tickets.lock().unwrap();
        t.retain(|_, v| v.expires > Instant::now());
        t.insert(
            ticket.clone(),
            Ticket {
                vm_id: vm_id.into(),
                kind,
                expires: Instant::now() + TICKET_TTL,
            },
        );
        ticket
    }

    /// Consume a ticket: valid once, for one VM and console kind.
    fn redeem(&self, ticket: &str, vm_id: &str, kind: ConsoleKind) -> bool {
        let Some(t) = self.tickets.lock().unwrap().remove(ticket) else {
            return false;
        };
        t.vm_id == vm_id && t.kind == kind && t.expires > Instant::now()
    }
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
}

async fn ticket(
    State(app): State<Arc<App>>,
    UrlPath((id, kind)): UrlPath<(String, String)>,
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
    let ticket = app.consoles.issue(&id, kind);
    Ok(Json(TicketResponse {
        protocol: format!("{PROTOCOL_PREFIX}{ticket}"),
        ticket,
        vnc_password,
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
) -> Result<String, ApiError> {
    if !origin_ok(headers) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "cross-origin console request".into(),
        ));
    }
    let proto = ticket_protocol(headers)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "missing console ticket".into()))?;
    if !app
        .consoles
        .redeem(&proto[PROTOCOL_PREFIX.len()..], id, kind)
    {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid or expired console ticket".into(),
        ));
    }
    Ok(proto)
}

async fn vnc_ws(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let proto = match authorize(&app, &headers, &id, ConsoleKind::Vnc) {
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
        match upstream {
            Ok(up) => pump(socket, up).await,
            Err(e) => close_with(socket, &format!("VNC unavailable: {e}")).await,
        }
    })
}

/// Shuttle bytes between a WebSocket (binary frames) and a stream.
async fn pump(socket: WebSocket, upstream: Box<dyn qemu_ui_qmp::transport::Stream>) {
    let (mut up_r, mut up_w) = tokio::io::split(upstream);
    let (mut ws_tx, mut ws_rx) = socket.split();
    let to_browser = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match up_r.read(&mut buf).await {
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
            }
        }
        let _ = ws_tx.close().await;
    };
    let to_upstream = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            if up_w.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = up_w.shutdown().await;
    };
    tokio::select! {
        _ = to_browser => {},
        _ = to_upstream => {},
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
        Ok(p) => p,
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
        let t = c.issue("vm1", ConsoleKind::Vnc);
        assert!(!c.redeem(&t, "vm2", ConsoleKind::Vnc), "wrong VM");
        let t = c.issue("vm1", ConsoleKind::Vnc);
        assert!(!c.redeem(&t, "vm1", ConsoleKind::Serial), "wrong kind");
        let t = c.issue("vm1", ConsoleKind::Vnc);
        assert!(c.redeem(&t, "vm1", ConsoleKind::Vnc));
        assert!(!c.redeem(&t, "vm1", ConsoleKind::Vnc), "single use");
        assert!(!c.redeem("nope", "vm1", ConsoleKind::Vnc));
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
        write!(s, "POST /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
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

            let app = crate::api::App::new(sv.scanner.clone(), sv.clone());
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
