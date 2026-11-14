//! Resumable browser uploads (plan.md §9.4, M2-07).
//!
//! A tus-like protocol:
//!
//! * `POST   /uploads`        create a session `{pool, name, size_bytes, sha256?, client_key?}`;
//!   an existing session with the same pool, name, size and client key is
//!   returned instead, so a reloaded page can resume.
//! * `GET    /uploads`        open sessions (for resuming after a reload).
//! * `HEAD   /uploads/{id}`   `Upload-Offset` / `Upload-Length` headers.
//! * `GET    /uploads/{id}`   the session as JSON.
//! * `PATCH  /uploads/{id}`   a chunk; header `Upload-Offset` must equal the
//!   bytes already stored, otherwise 409 with the current offset. Bodies are
//!   streamed to disk, never buffered, and capped at `UPLOAD_CHUNK_SIZE`.
//! * `DELETE /uploads/{id}`   abandon: the partial file is removed.
//!
//! Partial data lives in `.upload-<id>.part` inside the target pool (same
//! filesystem, so completion is an atomic rename; dot-names are never listed
//! as images). The file length is the source of truth for the offset, so
//! sessions survive daemon restarts. Each active session is an `upload` job
//! on the Jobs page; cancelling the job abandons the upload. Upload jobs do
//! not take one of the limited job slots, since they mostly wait on the
//! browser.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::upload::{
    OFFSET_HEADER, UPLOAD_CHUNK_SIZE, UploadProgress, UploadSession, valid_sha256,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::api::{ApiError, App};
use crate::jobs::JobCtx;
use crate::store::UploadRow;
use crate::supervisor::Supervisor;

const LENGTH_HEADER: &str = "upload-length";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// In-memory state of a session that has a job running.
struct Live {
    /// Serialises writes to the partial file.
    write: tokio::sync::Mutex<()>,
    /// The job body's context, for progress reports.
    ctx: Mutex<Option<Arc<JobCtx>>>,
    /// Set once the upload completes or fails; ends the job.
    done: watch::Sender<Option<Result<String, String>>>,
    job_id: String,
}

pub struct Uploads {
    sv: Arc<Supervisor>,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

/// Errors carry the current offset when the client is out of sync.
enum UpErr {
    Api(ApiError),
    Offset(u64),
}

impl From<ApiError> for UpErr {
    fn from(e: ApiError) -> Self {
        UpErr::Api(e)
    }
}

impl IntoResponse for UpErr {
    fn into_response(self) -> Response {
        match self {
            UpErr::Api(e) => e.into_response(),
            UpErr::Offset(cur) => {
                let mut r = (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "offset mismatch; resume from the returned offset", "offset": cur})),
                )
                    .into_response();
                r.headers_mut()
                    .insert(OFFSET_HEADER, HeaderValue::from(cur));
                r
            }
        }
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(status, msg.into())
}

/// Free bytes on the filesystem holding `path`, if known.
fn fs_free(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

impl Uploads {
    pub fn new(sv: Arc<Supervisor>) -> Arc<Self> {
        Arc::new(Uploads {
            sv,
            live: Mutex::default(),
        })
    }

    fn part_path(&self, row: &UploadRow) -> Result<PathBuf, ApiError> {
        let dir = self
            .sv
            .pools
            .pool_dir(&row.pool)
            .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
        Ok(dir.join(format!(".upload-{}.part", row.id)))
    }

    fn offset_of(&self, row: &UploadRow) -> u64 {
        self.part_path(row)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    }

    fn session(&self, row: &UploadRow) -> UploadSession {
        // Only a live job: after a daemon restart the stored one is
        // `interrupted`, and a new one starts when the client resumes.
        let job_id = self
            .live
            .lock()
            .unwrap()
            .get(&row.id)
            .map(|l| l.job_id.clone());
        UploadSession {
            id: row.id.clone(),
            pool: row.pool.clone(),
            name: row.name.clone(),
            size: row.size,
            offset: self.offset_of(row),
            client_key: row.client_key.clone(),
            job_id,
            chunk_size: UPLOAD_CHUNK_SIZE,
            created_at: row.created_at,
        }
    }

    fn row(&self, id: &str) -> Result<UploadRow, ApiError> {
        self.sv
            .store
            .get_upload(id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such upload"))
    }

    /// The live state for a session, starting its job if needed (also after
    /// a daemon restart, when the previous job was marked interrupted).
    fn live(self: &Arc<Self>, row: &UploadRow) -> Arc<Live> {
        let mut map = self.live.lock().unwrap();
        if let Some(l) = map.get(&row.id) {
            return l.clone();
        }
        let (done, _) = watch::channel(None);
        let this = self.clone();
        let id = row.id.clone();
        let size = row.size.max(1);
        let start = self.offset_of(row);
        // The job body needs the Live it belongs to; fill it in once created.
        let slot: Arc<Mutex<Option<Arc<Live>>>> = Arc::default();
        let slot2 = slot.clone();
        let job = self.sv.jobs.spawn_unthrottled(
            "upload",
            &format!("Upload {} to {}", row.name, row.pool),
            json!({"pool": row.pool, "name": row.name, "upload": row.id}),
            true,
            move |ctx| async move {
                let live = loop {
                    if let Some(l) = slot2.lock().unwrap().clone() {
                        break l;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                };
                *live.ctx.lock().unwrap() = Some(ctx.clone());
                ctx.progress(
                    Some(start as f64 / size as f64),
                    Some("Waiting for data from the browser"),
                );
                let mut rx = live.done.subscribe();
                // `Some(outcome)` when the upload ended, `None` when cancelled.
                // (No await inside the arms: the watch guard is not `Send`.)
                let ended: Option<Option<Result<String, String>>> = tokio::select! {
                    r = rx.wait_for(Option::is_some) => Some(r.ok().and_then(|g| g.clone())),
                    _ = ctx.cancelled() => None,
                };
                match ended {
                    Some(Some(Ok(msg))) => Ok(msg),
                    Some(Some(Err(e))) => Err(anyhow!(e)),
                    Some(None) => Err(anyhow!("the upload ended unexpectedly")),
                    None => {
                        this.discard(&id).await;
                        Err(anyhow!("cancelled"))
                    }
                }
            },
        );
        let live = Arc::new(Live {
            write: tokio::sync::Mutex::new(()),
            ctx: Mutex::default(),
            done,
            job_id: job.id.clone(),
        });
        *slot.lock().unwrap() = Some(live.clone());
        if let Err(e) = self.sv.store.set_upload_job(&row.id, &job.id) {
            tracing::warn!("recording upload job: {e:#}");
        }
        map.insert(row.id.clone(), live.clone());
        live
    }

    /// Remove the partial file and the session.
    async fn discard(&self, id: &str) {
        let live = self.live.lock().unwrap().get(id).cloned();
        let _guard = match &live {
            Some(l) => Some(l.write.lock().await),
            None => None,
        };
        if let Ok(Some(row)) = self.sv.store.get_upload(id) {
            if let Ok(p) = self.part_path(&row) {
                let _ = std::fs::remove_file(p);
            }
            let _ = self.sv.store.delete_upload(id);
        }
        self.live.lock().unwrap().remove(id);
    }

    async fn create(
        self: &Arc<Self>,
        b: NewUpload,
    ) -> Result<(StatusCode, UploadSession), ApiError> {
        if b.size_bytes == 0 {
            return Err(err(StatusCode::UNPROCESSABLE_ENTITY, "the file is empty"));
        }
        if let Some(h) = &b.sha256
            && !valid_sha256(h)
        {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "sha256 must be 64 hex characters",
            ));
        }
        let uploads = self
            .sv
            .store
            .list_uploads()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        if let Some(existing) = uploads
            .iter()
            .find(|u| u.pool == b.pool && u.name == b.name)
        {
            // Same file from the same client: resume it.
            if existing.size == b.size_bytes
                && existing.client_key.is_some()
                && existing.client_key == b.client_key
            {
                self.live(existing);
                return Ok((StatusCode::OK, self.session(existing)));
            }
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "an upload of '{}' into '{}' is already in progress",
                    b.name, b.pool
                ),
            ));
        }
        let r = ImageRef {
            pool: b.pool.clone(),
            name: b.name.clone(),
        };
        let dest = self
            .sv
            .pools
            .new_image_path(&r)
            .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
        // Other unfinished uploads into the same pool have space reserved.
        let reserved: u64 = uploads
            .iter()
            .filter(|u| u.pool == b.pool)
            .map(|u| u.size.saturating_sub(self.offset_of(u)))
            .sum();
        let q = self.sv.qemu_img().await;
        self.sv
            .pools
            .check_quota(q.as_deref(), &b.pool, b.size_bytes.saturating_add(reserved))
            .await
            .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
        if let Some(dir) = dest.parent()
            && let Some(free) = fs_free(dir)
            && free < b.size_bytes.saturating_add(reserved)
        {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "not enough free disk space: {free} bytes free, {} needed",
                    b.size_bytes
                ),
            ));
        }
        let row = UploadRow {
            id: format!("{:016x}", rand::random::<u64>()),
            pool: b.pool,
            name: b.name,
            size: b.size_bytes,
            sha256: b.sha256.map(|s| s.to_ascii_lowercase()),
            client_key: b.client_key,
            job_id: None,
            created_at: now(),
        };
        let part = self.part_path(&row)?;
        std::fs::File::create(&part)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Err(e) = self.sv.store.insert_upload(&row) {
            let _ = std::fs::remove_file(&part);
            return Err(err(StatusCode::CONFLICT, format!("{e:#}")));
        }
        self.live(&row);
        Ok((StatusCode::CREATED, self.session(&row)))
    }

    /// Append one chunk starting at `offset`.
    async fn write_chunk(
        self: &Arc<Self>,
        id: &str,
        offset: u64,
        body: Body,
    ) -> Result<UploadProgress, UpErr> {
        let row = self.row(id)?;
        let live = self.live(&row);
        let _guard = live.write.lock().await;
        // The session may have been discarded while we waited for the lock.
        let row = self.row(id)?;
        let part = self.part_path(&row)?;
        let cur = std::fs::metadata(&part)
            .map(|m| m.len())
            .map_err(|_| err(StatusCode::GONE, "upload data is gone"))?;
        if offset != cur {
            return Err(UpErr::Offset(cur));
        }
        let allowed = UPLOAD_CHUNK_SIZE.min(row.size - cur);
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut written: u64 = 0;
        let mut stream = body.into_data_stream();
        let mut broken = false;
        while let Some(frame) = stream.next().await {
            let Ok(bytes) = frame else {
                // Client went away mid-chunk: keep what arrived; it resumes
                // from the new offset.
                broken = true;
                break;
            };
            if written + bytes.len() as u64 > allowed {
                drop(file);
                // Undo this chunk entirely so the offset stays consistent.
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&part) {
                    let _ = f.set_len(cur);
                }
                return Err(err(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("chunk exceeds the allowed {allowed} bytes (chunk size or remaining file size)"),
                )
                .into());
            }
            file.write_all(&bytes)
                .await
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            written += bytes.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        drop(file);
        let new_offset = cur + written;
        if let Some(ctx) = live.ctx.lock().unwrap().clone() {
            ctx.progress(
                Some(new_offset as f64 / row.size as f64),
                Some(&format!("{new_offset} of {} bytes", row.size)),
            );
        }
        if broken {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("the request body was interrupted at offset {new_offset}"),
            )
            .into());
        }
        if new_offset < row.size {
            return Ok(UploadProgress {
                offset: new_offset,
                size: row.size,
                complete: false,
                format: None,
            });
        }
        self.finish(&row, &part, &live).await.map_err(UpErr::Api)
    }

    /// All bytes are in: verify, move into place, identify.
    async fn finish(
        &self,
        row: &UploadRow,
        part: &Path,
        live: &Live,
    ) -> Result<UploadProgress, ApiError> {
        let fail = |msg: String| {
            let _ = live.done.send(Some(Err(msg.clone())));
            msg
        };
        if let Some(expected) = &row.sha256 {
            let p = part.to_path_buf();
            let actual = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
                use std::io::Read;
                let mut f = std::fs::File::open(p)?;
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                Ok(hex::encode(h.finalize()))
            })
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if &actual != expected {
                let _ = std::fs::remove_file(part);
                let _ = self.sv.store.delete_upload(&row.id);
                self.live.lock().unwrap().remove(&row.id);
                return Err(err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    fail(format!(
                        "checksum mismatch: expected {expected}, got {actual}; the upload was discarded"
                    )),
                ));
            }
        }
        let r = ImageRef {
            pool: row.pool.clone(),
            name: row.name.clone(),
        };
        let dest = match self.sv.pools.new_image_path(&r) {
            Ok(d) => d,
            Err(e) => {
                // Keep the data: the user can free the name and the next
                // request will complete it.
                return Err(err(StatusCode::CONFLICT, format!("cannot complete: {e:#}")));
            }
        };
        if let Ok(f) = std::fs::File::open(part) {
            let _ = f.sync_all();
        }
        std::fs::rename(part, &dest)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, fail(e.to_string())))?;
        let _ = self.sv.store.delete_upload(&row.id);
        self.live.lock().unwrap().remove(&row.id);
        let format = match self.sv.qemu_img().await {
            Some(q) => self.sv.pools.info(&q, &dest).await.ok().map(|(f, _, _)| f),
            None => None,
        };
        let _ = live.done.send(Some(Ok(format!(
            "Uploaded {} ({})",
            row.name,
            format.as_deref().unwrap_or("unknown format")
        ))));
        Ok(UploadProgress {
            offset: row.size,
            size: row.size,
            complete: true,
            format,
        })
    }

    /// Abandon an upload. Cancels its job (which removes the data) and waits
    /// briefly so the partial file is gone when this returns.
    async fn abandon(self: &Arc<Self>, id: &str) -> Result<(), ApiError> {
        self.row(id)?;
        let job = self.live.lock().unwrap().get(id).map(|l| l.job_id.clone());
        match job {
            Some(job_id) if self.sv.jobs.cancel(&job_id).is_ok() => {
                for _ in 0..100 {
                    if self.sv.store.get_upload(id).ok().flatten().is_none() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                self.discard(id).await;
            }
            _ => self.discard(id).await,
        }
        Ok(())
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/uploads", get(list).post(create))
        .route(
            "/uploads/{id}",
            get(get_one).head(head).patch(patch).delete(delete),
        )
}

#[derive(Deserialize)]
struct NewUpload {
    pool: String,
    name: String,
    size_bytes: u64,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    client_key: Option<String>,
}

async fn list(State(app): State<Arc<App>>) -> Result<Json<Vec<UploadSession>>, ApiError> {
    let rows = app
        .uploads
        .sv
        .store
        .list_uploads()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(rows.iter().map(|r| app.uploads.session(r)).collect()))
}

async fn create(
    State(app): State<Arc<App>>,
    Json(b): Json<NewUpload>,
) -> Result<(StatusCode, Json<UploadSession>), ApiError> {
    app.uploads.create(b).await.map(|(s, u)| (s, Json(u)))
}

async fn get_one(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<UploadSession>, ApiError> {
    let row = app.uploads.row(&id)?;
    Ok(Json(app.uploads.session(&row)))
}

async fn head(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let row = app.uploads.row(&id)?;
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert(
        OFFSET_HEADER,
        HeaderValue::from(app.uploads.offset_of(&row)),
    );
    r.headers_mut()
        .insert(LENGTH_HEADER, HeaderValue::from(row.size));
    r.headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(r)
}

async fn patch(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(offset) = headers
        .get(OFFSET_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    else {
        return err(
            StatusCode::BAD_REQUEST,
            "missing or invalid Upload-Offset header",
        )
        .into_response();
    };
    match app.uploads.write_chunk(&id, offset, body).await {
        Ok(p) => {
            let mut r = Json(&p).into_response();
            r.headers_mut()
                .insert(OFFSET_HEADER, HeaderValue::from(p.offset));
            r
        }
        Err(e) => e.into_response(),
    }
}

async fn delete(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    app.uploads
        .abandon(&id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
}

/// Uploads end to end through the real router, with a real multi-chunk
/// qcow2 created by qemu-img. Skipped when QEMU is absent.
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use qemu_ui_core::upload::UPLOAD_CHUNK_SIZE;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CHUNK: usize = UPLOAD_CHUNK_SIZE as usize;

    async fn call(
        r: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> (u16, Value, Option<u64>) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"));
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        let res = r.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let off = res
            .headers()
            .get("upload-offset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            off,
        )
    }

    async fn post_json(r: &axum::Router, uri: &str, v: Value) -> (u16, Value) {
        let (s, b, _) = call(
            r,
            "POST",
            uri,
            &[("content-type", "application/json".into())],
            Body::from(v.to_string()),
        )
        .await;
        (s, b)
    }

    async fn patch(
        r: &axum::Router,
        id: &str,
        offset: u64,
        data: Vec<u8>,
    ) -> (u16, Value, Option<u64>) {
        call(
            r,
            "PATCH",
            &format!("/api/v1/uploads/{id}"),
            &[("upload-offset", offset.to_string())],
            Body::from(data),
        )
        .await
    }

    async fn router(
        data: &Path,
        qemu: &Path,
        img: &Path,
    ) -> (axum::Router, std::sync::Arc<crate::supervisor::Supervisor>) {
        let caps = host_caps(qemu, img).await;
        let sv = make(data, &caps).await;
        let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
        (crate::api::router(app, None), sv)
    }

    async fn job(r: &axum::Router, id: &str) -> Value {
        call(r, "GET", &format!("/api/v1/jobs/{id}"), &[], Body::empty())
            .await
            .1
    }

    async fn wait_job(r: &axum::Router, id: &str, status: &str) -> Value {
        for _ in 0..200 {
            let j = job(r, id).await;
            if j["status"] == status {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("job {id} never reached {status}: {}", job(r, id).await);
    }

    fn parts(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".upload-"))
            .count()
    }

    #[test]
    fn chunked_upload_survives_interruption_and_restart() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("upl");
        // A real, valid qcow2 large enough for three chunks.
        let src = data.join("src.qcow2");
        let ok = std::process::Command::new(&img)
            .args([
                "create",
                "-q",
                "-f",
                "qcow2",
                "-o",
                "preallocation=metadata",
            ])
            .arg(&src)
            .arg("40M")
            .status()
            .unwrap();
        assert!(ok.success());
        let bytes = std::fs::read(&src).unwrap();
        assert!(bytes.len() > 2 * CHUNK, "need a multi-chunk file");
        let size = bytes.len() as u64;
        let key = format!("disk.qcow2:{size}:1700000000000");

        // ---- first daemon: create, one chunk, then an interrupted chunk.
        let rt1 = tokio::runtime::Runtime::new().unwrap();
        let (id, first_job, offset) = rt1.block_on(async {
            let (r, sv) = router(&data, &qemu, &img).await;
            let (s, sess) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "disk.qcow2", "size_bytes": size, "client_key": key})).await;
            assert_eq!(s, 201, "{sess}");
            let id = sess["id"].as_str().unwrap().to_string();
            let job_id = sess["job_id"].as_str().unwrap().to_string();
            assert_eq!(job(&r, &job_id).await["kind"], "upload");
            assert_eq!(job(&r, &job_id).await["status"], "running", "upload jobs do not wait for a slot");

            let (s, p, _) = patch(&r, &id, 0, bytes[..CHUNK].to_vec()).await;
            assert_eq!((s, p["offset"].as_u64()), (200, Some(CHUNK as u64)), "{p}");
            // Progress reaches the job.
            tokio::time::sleep(Duration::from_millis(300)).await;

            // A chunk whose body breaks after 5 MiB: what arrived is kept.
            let five = 5 << 20;
            let piece = bytes[CHUNK..CHUNK + five].to_vec();
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(axum::body::Bytes::from(piece)),
                Err(std::io::Error::other("connection reset")),
            ]);
            let (s, _, _) = call(&r, "PATCH", &format!("/api/v1/uploads/{id}"), &[("upload-offset", CHUNK.to_string())], Body::from_stream(stream)).await;
            assert_eq!(s, 400);
            let (s, _, off) = call(&r, "HEAD", &format!("/api/v1/uploads/{id}"), &[], Body::empty()).await;
            assert_eq!((s, off), (204, Some((CHUNK + five) as u64)));

            // Replaying the earlier chunk is refused with the real offset.
            let (s, b, off) = patch(&r, &id, 0, bytes[..1024].to_vec()).await;
            assert_eq!((s, b["offset"].as_u64(), off), (409, Some((CHUNK + five) as u64), Some((CHUNK + five) as u64)));

            // Same file again from a reloaded page: the same session.
            let (s, again) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "disk.qcow2", "size_bytes": size, "client_key": key})).await;
            assert_eq!((s, again["id"].as_str()), (200, Some(id.as_str())));
            // A different file with the same name: refused.
            let (s, _) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "disk.qcow2", "size_bytes": 123, "client_key": "other"})).await;
            assert_eq!(s, 409);
            // Partial data is not an image.
            let (_, pool) = call(&r, "GET", "/api/v1/pools/default", &[], Body::empty()).await.into_tuple2();
            assert!(pool["images"].as_array().unwrap().is_empty(), "{pool}");
            drop(sv);
            (id, job_id, (CHUNK + five) as u64)
        });
        drop(rt1); // the daemon "dies"

        // ---- second daemon: resume from the partial file and finish.
        let rt2 = tokio::runtime::Runtime::new().unwrap();
        rt2.block_on(async {
            let (r, sv) = router(&data, &qemu, &img).await;
            assert_eq!(job(&r, &first_job).await["status"], "interrupted");
            let (s, sess, _) = call(
                &r,
                "GET",
                &format!("/api/v1/uploads/{id}"),
                &[],
                Body::empty(),
            )
            .await;
            assert_eq!((s, sess["offset"].as_u64()), (200, Some(offset)));
            let (_, list, _) = call(&r, "GET", "/api/v1/uploads", &[], Body::empty()).await;
            assert_eq!(
                list.as_array().unwrap().len(),
                1,
                "open sessions are listed for resuming"
            );

            assert!(
                sess["job_id"].is_null(),
                "no live job until the client resumes"
            );

            let mut off = offset as usize;
            let mut last = Value::Null;
            let mut new_job = String::new();
            while off < bytes.len() {
                if new_job.is_empty() && off > offset as usize {
                    let (_, s, _) = call(
                        &r,
                        "GET",
                        &format!("/api/v1/uploads/{id}"),
                        &[],
                        Body::empty(),
                    )
                    .await;
                    new_job = s["job_id"].as_str().unwrap().to_string();
                }
                let end = (off + CHUNK).min(bytes.len());
                let (s, p, _) = patch(&r, &id, off as u64, bytes[off..end].to_vec()).await;
                assert_eq!(s, 200, "{p}");
                off = p["offset"].as_u64().unwrap() as usize;
                last = p;
            }
            assert_eq!(last["complete"], true);
            assert_eq!(last["format"], "qcow2");

            // The new job finished; the image is real; nothing is left over.
            assert!(
                !new_job.is_empty(),
                "the file needs more than one post-restart chunk"
            );
            assert_ne!(new_job, first_job, "a new job after the restart");
            wait_job(&r, &new_job, "succeeded").await;
            let (_, d, _) = call(
                &r,
                "GET",
                "/api/v1/pools/default/images/disk.qcow2",
                &[],
                Body::empty(),
            )
            .await;
            assert_eq!(
                (d["format"].as_str(), d["virtual_size"].as_u64()),
                (Some("qcow2"), Some(40 << 20)),
                "{d}"
            );
            let dir = sv.pools.pool_dir("default").unwrap();
            assert_eq!(
                std::fs::read(dir.join("disk.qcow2")).unwrap(),
                bytes,
                "byte-identical"
            );
            assert_eq!(parts(&dir), 0);
            let (_, list, _) = call(&r, "GET", "/api/v1/uploads", &[], Body::empty()).await;
            assert!(list.as_array().unwrap().is_empty());
        });
        drop(rt2);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn refusals_checksums_and_abandon() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("upr");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (r, sv) = router(&data, &qemu, &img).await;
            let dir = sv.pools.pool_dir("default").unwrap();

            // Names and pools are validated like any new image.
            for name in ["../escape.iso", "a/b.iso", "nul.iso", ".hidden"] {
                let (s, _) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": name, "size_bytes": 10})).await;
                assert_eq!(s, 422, "{name}");
            }
            let (s, _) = post_json(&r, "/api/v1/uploads", json!({"pool": "nope", "name": "x.iso", "size_bytes": 10})).await;
            assert_eq!(s, 422);
            let (s, _) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "x.iso", "size_bytes": 10, "sha256": "zz"})).await;
            assert_eq!(s, 422, "malformed checksum");

            // Quota: also counts space reserved by other unfinished uploads.
            sv.pools.update("default", Some(1 << 20), false).unwrap();
            let (s, e) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "big.iso", "size_bytes": 2u64 << 20})).await;
            assert_eq!(s, 422);
            assert!(e["error"].as_str().unwrap().contains("quota"), "{e}");
            let (s, a) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "a.iso", "size_bytes": 700u64 << 10})).await;
            assert_eq!(s, 201, "{a}");
            let (s, _) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "b.iso", "size_bytes": 700u64 << 10})).await;
            assert_eq!(s, 422, "a.iso's reservation counts");
            sv.pools.update("default", None, false).unwrap();

            // A chunk larger than the remaining size is refused and undone.
            let a_id = a["id"].as_str().unwrap();
            let (s, _, _) = patch(&r, a_id, 0, vec![1u8; (700 << 10) + 1]).await;
            assert_eq!(s, 413);
            let (_, _, off) = call(&r, "HEAD", &format!("/api/v1/uploads/{a_id}"), &[], Body::empty()).await;
            assert_eq!(off, Some(0));

            // Abandon: the partial data and the session go; the job is cancelled.
            let (s, _, _) = patch(&r, a_id, 0, vec![1u8; 1000]).await;
            assert_eq!(s, 200);
            assert_eq!(parts(&dir), 1);
            let (s, _, _) = call(&r, "DELETE", &format!("/api/v1/uploads/{a_id}"), &[], Body::empty()).await;
            assert_eq!(s, 204);
            assert_eq!(parts(&dir), 0);
            wait_job(&r, a["job_id"].as_str().unwrap(), "cancelled").await;
            let (s, _, _) = call(&r, "HEAD", &format!("/api/v1/uploads/{a_id}"), &[], Body::empty()).await;
            assert_eq!(s, 404);

            // Cancelling the job from the Jobs page abandons the upload too.
            let (_, c) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "c.iso", "size_bytes": 100})).await;
            let (s, _) = post_json(&r, &format!("/api/v1/jobs/{}/cancel", c["job_id"].as_str().unwrap()), json!({})).await;
            assert_eq!(s, 202);
            wait_job(&r, c["job_id"].as_str().unwrap(), "cancelled").await;
            assert_eq!(parts(&dir), 0);

            // Checksums: a match completes, a mismatch discards.
            let payload = b"hello, iso".to_vec();
            use sha2::Digest;
            let good = hex::encode(sha2::Sha256::digest(&payload));
            let (_, ok) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "ok.iso", "size_bytes": payload.len(), "sha256": good})).await;
            let (s, p, _) = patch(&r, ok["id"].as_str().unwrap(), 0, payload.clone()).await;
            assert_eq!((s, p["complete"].as_bool()), (200, Some(true)), "{p}");
            assert!(dir.join("ok.iso").is_file());
            let (_, bad) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "bad.iso", "size_bytes": payload.len(), "sha256": "0".repeat(64)})).await;
            let (s, e, _) = patch(&r, bad["id"].as_str().unwrap(), 0, payload).await;
            assert_eq!(s, 422);
            assert!(e["error"].as_str().unwrap().contains("checksum"), "{e}");
            assert!(!dir.join("bad.iso").exists());
            assert_eq!(parts(&dir), 0);
            wait_job(&r, bad["job_id"].as_str().unwrap(), "failed").await;

            // Missing offset header.
            let (_, d) = post_json(&r, "/api/v1/uploads", json!({"pool": "default", "name": "d.iso", "size_bytes": 10})).await;
            let (s, _, _) = call(&r, "PATCH", &format!("/api/v1/uploads/{}", d["id"].as_str().unwrap()), &[], Body::from(vec![0u8; 10])).await;
            assert_eq!(s, 400);
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }

    trait Tuple2 {
        fn into_tuple2(self) -> (u16, Value);
    }
    impl Tuple2 for (u16, Value, Option<u64>) {
        fn into_tuple2(self) -> (u16, Value) {
            (self.0, self.1)
        }
    }
}
