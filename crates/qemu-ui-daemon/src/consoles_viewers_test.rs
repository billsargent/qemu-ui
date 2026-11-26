//! Multi-viewer consoles end to end (M5-06): three viewers complete a real
//! RFB handshake with QEMU's VNC server through the proxy. Only the viewer
//! holding control has its input forwarded; the others keep receiving
//! framebuffer updates; control moves on request; watch-only viewers can't
//! take it. Skipped without QEMU.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::supervisor::test_support::*;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ------------------------------------------------------------------- DES
// VNC authentication encrypts the server's challenge with DES, keyed by the
// password with each byte's bits reversed. Only the test needs it.

const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Pick bits of `input` (an `width`-bit value, bit 1 = most significant).
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &p| {
        (out << 1) | ((input >> (width - p as u32)) & 1)
    })
}

fn des_encrypt(key: [u8; 8], block: [u8; 8]) -> [u8; 8] {
    let k = permute(u64::from_be_bytes(key), 64, &PC1);
    let (mut c, mut d) = ((k >> 28) as u32 & 0x0fff_ffff, k as u32 & 0x0fff_ffff);
    let rot = |x: u32, n: u32| ((x << n) | (x >> (28 - n))) & 0x0fff_ffff;
    let mut subkeys = [0u64; 16];
    for (i, s) in SHIFTS.iter().enumerate() {
        c = rot(c, *s);
        d = rot(d, *s);
        subkeys[i] = permute(((c as u64) << 28) | d as u64, 56, &PC2);
    }
    let b = permute(u64::from_be_bytes(block), 64, &IP);
    let (mut l, mut r) = ((b >> 32) as u32, b as u32);
    for k in subkeys {
        let x = permute(r as u64, 32, &E) ^ k;
        let mut f = 0u64;
        for (i, sbox) in S.iter().enumerate() {
            let six = (x >> (42 - 6 * i)) & 0x3f;
            let row = ((six >> 4) & 2) | (six & 1);
            let col = (six >> 1) & 0xf;
            f = (f << 4) | sbox[(row * 16 + col) as usize] as u64;
        }
        let f = permute(f, 32, &P) as u32;
        (l, r) = (r, l ^ f);
    }
    permute(((r as u64) << 32) | l as u64, 64, &FP).to_be_bytes()
}

/// The 16-byte VNC authentication response.
fn vnc_auth(password: &str, challenge: &[u8]) -> Vec<u8> {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    challenge
        .chunks(8)
        .flat_map(|c| des_encrypt(key, c.try_into().unwrap()))
        .collect()
}

#[test]
fn des_matches_the_standard_vector() {
    let key = 0x1334_5779_9BBC_DFF1u64.to_be_bytes();
    let pt = 0x0123_4567_89AB_CDEFu64.to_be_bytes();
    assert_eq!(
        u64::from_be_bytes(des_encrypt(key, pt)),
        0x85E8_1354_0F0A_B405
    );
}

// --------------------------------------------------------- RFB over WS

struct Viewer {
    ws: Ws,
    buf: Vec<u8>,
    id: u64,
}

impl Viewer {
    async fn read(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            let m = tokio::time::timeout(Duration::from_secs(20), self.ws.next())
                .await
                .expect("VNC read timed out")
                .expect("connection closed")
                .unwrap();
            if let WsMsg::Binary(b) = m {
                self.buf.extend_from_slice(&b);
            }
        }
        self.buf.drain(..n).collect()
    }

    async fn send(&mut self, b: &[u8]) {
        self.ws
            .send(WsMsg::Binary(b.to_vec().into()))
            .await
            .unwrap();
    }

    /// RFB 3.8 handshake up to and including ServerInit.
    async fn handshake(&mut self, password: Option<&str>) {
        let v = self.read(12).await;
        assert!(v.starts_with(b"RFB 003."), "{v:?}");
        self.send(b"RFB 003.008\n").await;
        let n = self.read(1).await[0] as usize;
        let types = self.read(n).await;
        if types.contains(&1) {
            self.send(&[1]).await;
        } else {
            assert!(types.contains(&2), "security types {types:?}");
            self.send(&[2]).await;
            let challenge = self.read(16).await;
            let resp = vnc_auth(password.expect("a VNC password"), &challenge);
            self.send(&resp).await;
        }
        assert_eq!(self.read(4).await, [0, 0, 0, 0], "security result");
        self.send(&[1]).await; // ClientInit, shared
        let init = self.read(24).await;
        let name_len = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
        self.read(name_len).await;
    }

    async fn key(&mut self, down: bool) {
        self.send(&[4, down as u8, 0, 0, 0, 0, 0, 0x61]).await;
    }

    /// A full update request; the server must answer with an update.
    async fn expect_update(&mut self) {
        self.send(&[3, 0, 0, 0, 0, 0, 0, 64, 0, 64]).await;
        assert_eq!(self.read(1).await[0], 0, "FramebufferUpdate");
        self.buf.clear();
    }
}

async fn post_json(base: &str, path: &str, body: &str) -> serde_json::Value {
    let url = format!("{base}{path}");
    let body = body.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        let mut s = std::net::TcpStream::connect(host).unwrap();
        let token = "t".repeat(64);
        write!(
            s,
            "POST /{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or_default();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
        v["_status"] = status.into();
        v
    })
    .await
    .unwrap()
}

async fn join(base: &str, ws_base: &str, vm: &str, watch_only: bool) -> (Viewer, Option<String>) {
    let q = if watch_only { "?mode=view" } else { "" };
    let a = post_json(
        base,
        &format!("/api/v1/vms/{vm}/display/vnc:novnc/attach{q}"),
        "",
    )
    .await;
    assert_eq!(a["transport"], "websocket", "{a}");
    let mut req = format!("{ws_base}{}", a["path"].as_str().unwrap())
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(a["protocol"].as_str().unwrap()).unwrap(),
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let viewer = Viewer {
        ws,
        buf: Vec::new(),
        id: a["viewer_id"].as_u64().unwrap(),
    };
    (viewer, a["vnc_password"].as_str().map(String::from))
}

fn dropped(app: &crate::api::App, vm: &str, id: u64) -> u64 {
    app.consoles
        .presence(vm)
        .viewers
        .iter()
        .find(|v| v.id == id)
        .map_or(u64::MAX, |v| v.input_dropped)
}

/// Wait until viewer `id` has had exactly `want` input messages dropped.
/// A screen update can arrive before the proxy has parsed the key events
/// sent just before it, so the count is awaited, not read once.
async fn expect_dropped(app: &crate::api::App, vm: &str, id: u64, want: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = dropped(app, vm, id);
        if got == want {
            return;
        }
        assert!(
            got < want && tokio::time::Instant::now() < deadline,
            "viewer {id}: {got} inputs dropped, want {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn only_the_controller_types_and_others_keep_watching() {
    let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
        eprintln!("skipping: qemu not found");
        return;
    };
    let data = short_data_dir("mv");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let caps = host_caps(&qemu, &img).await;
        let sv = make(&data, &caps).await;
        let id = small_vm(&sv, &caps, "viewers").await;
        sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();

        let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), "t".repeat(64));
        let router = crate::api::router(app.clone(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let base = format!("http://{addr}");
        let ws_base = format!("ws://{addr}");

        let scenario = async {
            // The display API describes the engine.
            let (status, info) = {
                let v = tokio::task::spawn_blocking({
                    let url = format!("{addr}");
                    let id = id.clone();
                    move || {
                        use std::io::{Read, Write};
                        let mut s = std::net::TcpStream::connect(&url).unwrap();
                        write!(
                            s,
                            "GET /api/v1/vms/{id}/display HTTP/1.1\r\nHost: {url}\r\n\
                             Authorization: Bearer {}\r\nConnection: close\r\n\r\n",
                            "t".repeat(64)
                        )
                        .unwrap();
                        let mut out = String::new();
                        s.read_to_string(&mut out).unwrap();
                        out
                    }
                })
                .await
                .unwrap();
                let (head, body) = v.split_once("\r\n\r\n").unwrap();
                (
                    head.to_string(),
                    serde_json::from_str::<serde_json::Value>(body).unwrap(),
                )
            };
            assert!(status.contains(" 200 "), "{status}");
            assert_eq!(info["engines"][0]["id"], "vnc:novnc");
            assert_eq!(info["engines"][0]["available"], true, "{info}");
            assert_eq!(info["engines"][0]["caps"]["take_control"], true);

            let (mut a, pw) = join(&base, &ws_base, &id, false).await;
            a.handshake(pw.as_deref()).await;
            let (mut b, pw) = join(&base, &ws_base, &id, false).await;
            b.handshake(pw.as_deref()).await;
            let (mut c, pw) = join(&base, &ws_base, &id, true).await;
            c.handshake(pw.as_deref()).await;

            let p = app.consoles.presence(&id);
            assert_eq!(p.viewers.len(), 3);
            assert!(p.viewers.iter().find(|v| v.id == a.id).unwrap().controlling);

            // A types; B and the watch-only C try to.
            for v in [&mut a, &mut b, &mut c] {
                v.key(true).await;
                v.key(false).await;
            }
            // Everyone still gets screen updates (their non-input messages
            // pass), which also orders the checks after the key events.
            for v in [&mut a, &mut b, &mut c] {
                v.expect_update().await;
            }
            expect_dropped(&app, &id, b.id, 2).await;
            expect_dropped(&app, &id, c.id, 2).await;
            assert_eq!(dropped(&app, &id, a.id), 0);

            // B takes control over the API; A's input is dropped from now on.
            let r = post_json(
                &base,
                &format!("/api/v1/vms/{id}/display/control"),
                &format!("{{\"viewer_id\":{}}}", b.id),
            )
            .await;
            assert_eq!(r["_status"], 200, "{r}");
            a.key(true).await;
            b.key(true).await;
            a.expect_update().await;
            b.expect_update().await;
            expect_dropped(&app, &id, a.id, 1).await;
            assert_eq!(dropped(&app, &id, b.id), 2);

            // Watch-only viewers can't take control.
            let r = post_json(
                &base,
                &format!("/api/v1/vms/{id}/display/control"),
                &format!("{{\"viewer_id\":{}}}", c.id),
            )
            .await;
            assert_eq!(r["_status"], 409, "{r}");

            // When the controller leaves, control returns to A.
            drop(b);
            for _ in 0..100 {
                if app.consoles.presence(&id).viewers.len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let p = app.consoles.presence(&id);
            assert!(
                p.viewers.iter().find(|v| v.id == a.id).unwrap().controlling,
                "{p:?}"
            );

            // Idle viewers are closed.
            app.consoles.set_idle_timeout(Duration::ZERO);
            let closed = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match c.ws.next().await {
                        Some(Ok(WsMsg::Close(f))) => return f.map(|f| f.reason.to_string()),
                        None | Some(Err(_)) => return None,
                        _ => {}
                    }
                }
            })
            .await
            .expect("idle viewer not closed");
            assert!(closed.unwrap_or_default().contains("idle"));
            drop(a);
        };
        // Stop the VM whatever happened, then report.
        let outcome = tokio::time::timeout(
            Duration::from_secs(150),
            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(scenario)),
        )
        .await;
        let _ = sv.force_stop(&id).await;
        wait_state(&sv, &id, qemu_ui_core::vm::VmState::Stopped).await;
        match outcome {
            Err(_) => panic!("scenario timed out"),
            Ok(Err(p)) => std::panic::resume_unwind(p),
            Ok(Ok(())) => {}
        }
    });
    drop(rt);
    let _ = std::fs::remove_dir_all(&data);
}
