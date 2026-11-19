//! Screenshots and dashboard thumbnails (plan.md §8.3, M5-02).
//!
//! QMP `screendump` writes the guest's display to a file in the run dir; we
//! read it back, downscale it for the dashboard and cache the result. There
//! is no background loop: a thumbnail is refreshed only when a client asks
//! for it and the cached one is older than [`THUMB_TTL`], so VMs nobody is
//! looking at cost nothing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, App};

/// How long a thumbnail is reused before a request takes a new one.
pub const THUMB_TTL: Duration = Duration::from_secs(5);
/// Thumbnails fit in this box, keeping the aspect ratio.
pub const THUMB_MAX: (u32, u32) = (320, 240);

/// An RGB image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` bytes, row-major.
    pub data: Vec<u8>,
}

#[derive(Clone)]
struct Thumb {
    png: Arc<Vec<u8>>,
    etag: String,
    taken: Instant,
}

/// Per-VM thumbnail cache.
#[derive(Default)]
pub struct Thumbnails {
    cache: Mutex<HashMap<String, Thumb>>,
    /// One screendump at a time per VM.
    busy: tokio::sync::Mutex<()>,
}

impl Thumbnails {
    pub fn forget(&self, vm: &str) {
        self.cache.lock().unwrap().remove(vm);
    }
}

// ------------------------------------------------------------ image helpers

/// Parse a binary PPM (`P6`, maxval 255), QEMU's default screendump format.
pub fn parse_ppm(bytes: &[u8]) -> Result<Rgb> {
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        // Skip whitespace and comments.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'#') {
            if bytes[i] == b'#' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            bail!("truncated PPM header");
        }
        fields.push(std::str::from_utf8(&bytes[start..i])?.to_string());
    }
    i += 1; // the single whitespace byte after maxval
    if fields[0] != "P6" {
        bail!("not a binary PPM ({})", fields[0]);
    }
    let width: u32 = fields[1].parse()?;
    let height: u32 = fields[2].parse()?;
    if fields[3] != "255" {
        bail!("unsupported PPM maxval {}", fields[3]);
    }
    let len = width as usize * height as usize * 3;
    let data = bytes
        .get(i..i + len)
        .ok_or_else(|| anyhow!("truncated PPM data"))?
        .to_vec();
    Ok(Rgb {
        width,
        height,
        data,
    })
}

/// Decode a PNG to RGB (alpha dropped).
pub fn decode_png(bytes: &[u8]) -> Result<Rgb> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("PNG too large")?];
    let info = reader.next_frame(&mut buf)?;
    let buf = &buf[..info.buffer_size()];
    let (w, h) = (info.width, info.height);
    let data = match info.color_type {
        png::ColorType::Rgb => buf.to_vec(),
        png::ColorType::Rgba => buf
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        png::ColorType::Indexed => bail!("unexpected indexed PNG after expansion"),
    };
    Ok(Rgb {
        width: w,
        height: h,
        data,
    })
}

pub fn encode_png(img: &Rgb) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&img.data)?;
    }
    Ok(out)
}

/// Downscale to fit `max` (box filter); never upscales.
pub fn downscale(img: &Rgb, max: (u32, u32)) -> Rgb {
    let scale = f64::min(
        max.0 as f64 / img.width.max(1) as f64,
        max.1 as f64 / img.height.max(1) as f64,
    );
    if scale >= 1.0 {
        return img.clone();
    }
    let w = ((img.width as f64 * scale).round() as u32).max(1);
    let h = ((img.height as f64 * scale).round() as u32).max(1);
    let mut data = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        let y0 = (y as u64 * img.height as u64 / h as u64) as u32;
        let y1 = (((y + 1) as u64 * img.height as u64).div_ceil(h as u64) as u32).max(y0 + 1);
        for x in 0..w {
            let x0 = (x as u64 * img.width as u64 / w as u64) as u32;
            let x1 = (((x + 1) as u64 * img.width as u64).div_ceil(w as u64) as u32).max(x0 + 1);
            let mut sum = [0u64; 3];
            let mut n = 0u64;
            for sy in y0..y1.min(img.height) {
                let row = sy as usize * img.width as usize * 3;
                for sx in x0..x1.min(img.width) {
                    let p = row + sx as usize * 3;
                    sum[0] += img.data[p] as u64;
                    sum[1] += img.data[p + 1] as u64;
                    sum[2] += img.data[p + 2] as u64;
                    n += 1;
                }
            }
            let n = n.max(1);
            data.extend(sum.map(|s| (s / n) as u8));
        }
    }
    Rgb {
        width: w,
        height: h,
        data,
    }
}

// ------------------------------------------------------------- screendump

/// Take a screenshot of a running VM as RGB. PNG when this QEMU can write it,
/// else PPM.
pub async fn screendump(app: &App, id: &str) -> Result<Rgb> {
    let qmp = app
        .supervisor
        .live_qmp(id)
        .ok_or_else(|| anyhow!("the VM is not running"))?;
    let dir = app.supervisor.run_dir(id);
    let png_path = dir.join("screendump.png");
    let png = qmp
        .execute(
            "screendump",
            Some(json!({"filename": png_path.display().to_string(), "format": "png"})),
        )
        .await;
    let img = match png {
        Ok(_) => decode_png(&read_and_remove(&png_path).await?),
        Err(_) => {
            let ppm_path = dir.join("screendump.ppm");
            qmp.execute(
                "screendump",
                Some(json!({"filename": ppm_path.display().to_string()})),
            )
            .await
            .context("screendump")?;
            parse_ppm(&read_and_remove(&ppm_path).await?)
        }
    }?;
    Ok(img)
}

async fn read_and_remove(p: &Path) -> Result<Vec<u8>> {
    let bytes = tokio::fs::read(p)
        .await
        .with_context(|| "reading the screendump".to_string())?;
    let _ = tokio::fs::remove_file(p).await;
    Ok(bytes)
}

// --------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/thumbnail", get(thumbnail))
        .route("/vms/{id}/screenshot", get(screenshot))
}

fn not_running() -> Response {
    ApiError(StatusCode::CONFLICT, "the VM is not running".into()).into_response()
}

async fn thumbnail(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if app.supervisor.live_qmp(&id).is_none() {
        app.thumbnails.forget(&id);
        return not_running();
    }
    let cached = |app: &App| {
        app.thumbnails
            .cache
            .lock()
            .unwrap()
            .get(&id)
            .filter(|t| t.taken.elapsed() < THUMB_TTL)
            .cloned()
    };
    let thumb = match cached(&app) {
        Some(t) => t,
        None => {
            let _one = app.thumbnails.busy.lock().await;
            // Another request may have refreshed it while we waited.
            match cached(&app) {
                Some(t) => t,
                None => match take_thumb(&app, &id).await {
                    Ok(t) => t,
                    Err(e) => {
                        return ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}"))
                            .into_response();
                    }
                },
            }
        }
    };
    let etag = HeaderValue::from_str(&thumb.etag).expect("hex etag");
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == thumb.etag.as_bytes());
    let mut res = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, "image/png")],
            Body::from(thumb.png.to_vec()),
        )
            .into_response()
    };
    let h = res.headers_mut();
    h.insert(header::ETAG, etag);
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    res
}

async fn take_thumb(app: &App, id: &str) -> Result<Thumb> {
    let img = screendump(app, id).await?;
    let png = encode_png(&downscale(&img, THUMB_MAX))?;
    let etag = format!("\"{}\"", hex(&Sha256::digest(&png)[..12]));
    let t = Thumb {
        png: Arc::new(png),
        etag,
        taken: Instant::now(),
    };
    app.thumbnails
        .cache
        .lock()
        .unwrap()
        .insert(id.to_string(), t.clone());
    Ok(t)
}

async fn screenshot(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Response {
    if app.supervisor.live_qmp(&id).is_none() {
        return not_running();
    }
    let png = match screendump(&app, &id).await.and_then(|i| encode_png(&i)) {
        Ok(p) => p,
        Err(e) => {
            return ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response();
        }
    };
    let name = app
        .supervisor
        .detail(&id)
        .ok()
        .map(|d| d.summary.name)
        .unwrap_or_else(|| id.clone());
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe}-screenshot.png\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from(png),
    )
        .into_response()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Rgb {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                data.extend([v, 0, 255 - v]);
            }
        }
        Rgb {
            width: w,
            height: h,
            data,
        }
    }

    #[test]
    fn ppm_parses_with_comments() {
        let mut b = b"P6\n# made by a test\n2 1\n255\n".to_vec();
        b.extend([1, 2, 3, 4, 5, 6]);
        let img = parse_ppm(&b).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.data, [1, 2, 3, 4, 5, 6]);
        assert!(parse_ppm(b"P6\n2 1\n255\n\x01").is_err(), "truncated");
        assert!(parse_ppm(b"P3\n1 1\n255\n1 2 3").is_err(), "ascii PPM");
    }

    #[test]
    fn png_roundtrip_and_downscale() {
        let img = checker(800, 600);
        let small = downscale(&img, THUMB_MAX);
        assert_eq!((small.width, small.height), (320, 240));
        // A fine checkerboard averages to mid-grey-ish.
        assert!(
            small
                .data
                .iter()
                .step_by(3)
                .all(|&r| (100..=155).contains(&r))
        );
        let back = decode_png(&encode_png(&small).unwrap()).unwrap();
        assert_eq!(back, small);
        // Never upscales; odd aspect ratios keep their shape.
        assert_eq!(downscale(&checker(100, 50), THUMB_MAX), checker(100, 50));
        let tall = downscale(&checker(300, 1200), THUMB_MAX);
        assert_eq!((tall.width, tall.height), (60, 240));
    }
}

/// Thumbnails, screenshots and the SPICE handoff against a real QEMU.
#[cfg(test)]
mod real {
    use super::*;
    use crate::supervisor::test_support::*;
    use axum::http::Request;
    use qemu_ui_core::vm::VmState;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn get(r: &Router, uri: &str, headers: &[(&str, &str)]) -> (u16, HeaderMap, Vec<u8>) {
        let mut b = Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = r
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let h = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), 32 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, h, body)
    }

    #[test]
    fn thumbnail_screenshot_and_spice_handoff() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("scr");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let app = App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);
            let id = small_vm(&sv, &caps, "screens").await;
            let spice = caps.binaries[0].backends.has_spice();
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.display.device = Some("VGA".into());
            spec.display.spice = spice;
            sv.update(&id, spec)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();

            // Not running: no thumbnail, no .vv.
            assert_eq!(
                get(&r, &format!("/api/v1/vms/{id}/thumbnail"), &[]).await.0,
                409
            );
            assert_eq!(
                get(&r, &format!("/api/v1/vms/{id}/console.vv"), &[])
                    .await
                    .0,
                409
            );

            let outcome = tokio::time::timeout(Duration::from_secs(180), async {
                sv.start(&id).await.map_err(|e| format!("{e:?}"))?;
                let (st, h, png) = get(&r, &format!("/api/v1/vms/{id}/thumbnail"), &[]).await;
                assert_eq!(st, 200);
                assert_eq!(h[header::CONTENT_TYPE], "image/png");
                let thumb = decode_png(&png).unwrap();
                assert!(
                    thumb.width <= THUMB_MAX.0 && thumb.height <= THUMB_MAX.1,
                    "{}x{}",
                    thumb.width,
                    thumb.height
                );
                let etag = h[header::ETAG].to_str().unwrap().to_string();
                // Within the TTL the same thumbnail is served: 304 for the ETag.
                let (st, h2, body) = get(
                    &r,
                    &format!("/api/v1/vms/{id}/thumbnail"),
                    &[("if-none-match", &etag)],
                )
                .await;
                assert_eq!(st, 304);
                assert!(body.is_empty());
                assert_eq!(h2[header::ETAG].to_str().unwrap(), etag);

                let (st, h, png) = get(&r, &format!("/api/v1/vms/{id}/screenshot"), &[]).await;
                assert_eq!(st, 200);
                assert!(
                    h[header::CONTENT_DISPOSITION]
                        .to_str()
                        .unwrap()
                        .contains("screens-screenshot.png")
                );
                let full = decode_png(&png).unwrap();
                assert!(
                    full.width > thumb.width,
                    "full size {}x{}",
                    full.width,
                    full.height
                );

                if spice {
                    let qmp = sv.live_qmp(&id).unwrap();
                    let q = qmp.execute("query-spice", None).await.unwrap();
                    assert_eq!(q["enabled"], true, "{q}");
                    let info = crate::spice::lookup(&sv.store, &id).unwrap();
                    assert_eq!(q["port"], info.port, "{q}");
                    let (st, _, vv) = get(&r, &format!("/api/v1/vms/{id}/console.vv"), &[]).await;
                    assert_eq!(st, 200);
                    let vv = String::from_utf8(vv).unwrap();
                    assert!(vv.contains(&format!("port={}", info.port)), "{vv}");
                    assert!(vv.contains(&format!("password={}", info.password)));
                } else {
                    eprintln!("this QEMU has no SPICE; skipped the SPICE part");
                }
                Ok::<_, String>(())
            })
            .await;
            let _ = sv.force_stop(&id).await;
            wait_state(&sv, &id, VmState::Stopped).await;
            outcome.expect("timed out").unwrap();
            assert!(
                crate::spice::lookup(&sv.store, &id).is_none(),
                "SPICE record cleared at stop"
            );
            assert_eq!(
                get(&r, &format!("/api/v1/vms/{id}/thumbnail"), &[]).await.0,
                409
            );
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
