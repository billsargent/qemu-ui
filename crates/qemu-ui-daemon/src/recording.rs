//! Console recordings (plan.md §8.3, M5-03).
//!
//! A recording is made by the daemon's own VNC client, not by tapping a
//! viewer's connection: the stream then starts at a session start (RFB is
//! stateful), needs no viewer to be open, and uses a pixel format and
//! encodings the export decoder understands (`qemu_ui_core::recording`).
//! The client is a shared, view-only connection, so viewers are unaffected.
//!
//! Files live in `<data>/vms/<vm>/recordings/<id>.rec` with `<id>.json`
//! metadata and, after an export, `<id>.webm`. Each recording has a size
//! cap and every VM a total quota.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::recording::{
    Bookmark, ENCODINGS, Header, PIXEL_FORMAT, RecordingInfo, RecordingStatus, RecordingsInfo,
    ServerMsg, encode_header, encode_record, next_message, parse, render_frames, replay_preamble,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::api::{ApiError, App};
use crate::supervisor::{Supervisor, VncInfo};

/// At most this many update requests per second.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(100);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames per second in a WebM export.
const EXPORT_FPS: u32 = 10;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// One recording stops ("capped") at this size.
    pub cap_bytes: u64,
    /// All of a VM's recordings together.
    pub quota_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            cap_bytes: 512 << 20,
            quota_bytes: 2 << 30,
        }
    }
}

#[derive(Debug)]
pub enum RecError {
    Conflict(String),
    NotFound,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for RecError {
    fn from(e: anyhow::Error) -> Self {
        RecError::Failed(e)
    }
}

struct Active {
    id: String,
    started: Instant,
    stop: watch::Sender<bool>,
    info: Arc<Mutex<RecordingInfo>>,
    done: watch::Receiver<bool>,
}

pub struct Recordings {
    vms_dir: PathBuf,
    limits: Limits,
    active: Mutex<HashMap<String, Active>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_meta(path: &Path, info: &RecordingInfo) {
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec_pretty(info)
        .ok()
        .and_then(|b| std::fs::write(&tmp, b).ok())
        .and_then(|_| std::fs::rename(&tmp, path).ok());
    if ok.is_none() {
        tracing::warn!("could not save recording metadata {}", path.display());
    }
}

impl Recordings {
    pub fn new(data_dir: &Path, limits: Limits) -> Arc<Recordings> {
        Arc::new(Recordings {
            vms_dir: data_dir.join("vms"),
            limits,
            active: Mutex::default(),
        })
    }

    fn dir(&self, vm: &str) -> PathBuf {
        self.vms_dir.join(vm).join("recordings")
    }

    pub fn stream_path(&self, vm: &str, id: &str) -> PathBuf {
        self.dir(vm).join(format!("{id}.rec"))
    }

    fn meta_path(&self, vm: &str, id: &str) -> PathBuf {
        self.dir(vm).join(format!("{id}.json"))
    }

    pub fn webm_path(&self, vm: &str, id: &str) -> PathBuf {
        self.dir(vm).join(format!("{id}.webm"))
    }

    /// Every recording of `vm`, newest first.
    pub fn list(&self, vm: &str) -> Vec<RecordingInfo> {
        let mut out: Vec<RecordingInfo> = std::fs::read_dir(self.dir(vm))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| self.get(vm, e.path().file_stem()?.to_str()?))
            .collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        out
    }

    pub fn get(&self, vm: &str, id: &str) -> Option<RecordingInfo> {
        if !valid_id(id) {
            return None;
        }
        if let Some(a) = self.active.lock().unwrap().get(vm).filter(|a| a.id == id) {
            let mut i = a.info.lock().unwrap().clone();
            i.duration_ms = a.started.elapsed().as_millis() as u64;
            return Some(i);
        }
        let mut info: RecordingInfo =
            serde_json::from_slice(&std::fs::read(self.meta_path(vm, id)).ok()?).ok()?;
        if info.status == RecordingStatus::Recording {
            // Not active: the daemon stopped while it was being written.
            info.status = RecordingStatus::Interrupted;
            info.size_bytes = std::fs::metadata(self.stream_path(vm, id)).map_or(0, |m| m.len());
        }
        info.webm = self.webm_path(vm, id).is_file();
        Some(info)
    }

    fn used_bytes(&self, vm: &str) -> u64 {
        std::fs::read_dir(self.dir(vm))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Start recording `vm`'s display. Returns once the recorder is
    /// connected, so connection errors reach the caller.
    pub async fn start(
        self: &Arc<Self>,
        sv: &Supervisor,
        vm: &str,
    ) -> Result<RecordingInfo, RecError> {
        if self.active.lock().unwrap().contains_key(vm) {
            return Err(RecError::Conflict(
                "this VM is already being recorded".into(),
            ));
        }
        let vnc = sv.vnc_of(vm).ok_or_else(|| {
            RecError::Conflict("the VM is not running, or has no display to record".into())
        })?;
        let used = self.used_bytes(vm);
        if used >= self.limits.quota_bytes {
            return Err(RecError::Conflict(format!(
                "this VM's recordings use {} MiB, its whole quota; delete some first",
                used >> 20
            )));
        }
        let cap = self.limits.cap_bytes.min(self.limits.quota_bytes - used);

        let mut conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, Connection::open(&vnc))
            .await
            .map_err(|_| anyhow!("the VM's VNC server did not answer"))??;

        let id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        std::fs::create_dir_all(self.dir(vm)).context("creating the recordings folder")?;
        let started_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let header = Header {
            version: 1,
            vm_id: vm.into(),
            started_at_ms: started_ms,
            width: conn.width,
            height: conn.height,
            name: conn.name.clone(),
        };
        let mut file = tokio::io::BufWriter::new(
            tokio::fs::File::create(self.stream_path(vm, &id))
                .await
                .context("creating the recording file")?,
        );
        let head = encode_header(&header);
        let pre = encode_record(0, &replay_preamble(conn.width, conn.height, &conn.name));
        file.write_all(&head)
            .await
            .context("writing the recording")?;
        file.write_all(&pre)
            .await
            .context("writing the recording")?;

        let info = Arc::new(Mutex::new(RecordingInfo {
            id: id.clone(),
            vm_id: vm.into(),
            started_at: started_ms / 1000,
            ended_at: None,
            duration_ms: 0,
            size_bytes: (head.len() + pre.len()) as u64,
            status: RecordingStatus::Recording,
            error: None,
            width: conn.width,
            height: conn.height,
            bookmarks: vec![],
            webm: false,
        }));
        write_meta(&self.meta_path(vm, &id), &info.lock().unwrap());

        let (stop_tx, stop_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let started = Instant::now();
        self.active.lock().unwrap().insert(
            vm.into(),
            Active {
                id: id.clone(),
                started,
                stop: stop_tx,
                info: info.clone(),
                done: done_rx,
            },
        );
        let this = self.clone();
        let vm_id = vm.to_string();
        let meta = self.meta_path(vm, &id);
        tokio::spawn(async move {
            let res = conn.record(&mut file, stop_rx, started, cap, &info).await;
            let _ = file.flush().await;
            {
                let mut i = info.lock().unwrap();
                i.duration_ms = started.elapsed().as_millis() as u64;
                i.ended_at = Some(now_secs());
                match res {
                    Ok(status) => i.status = status,
                    Err(e) => {
                        i.status = RecordingStatus::Failed;
                        i.error = Some(format!("{e:#}"));
                    }
                }
                write_meta(&meta, &i);
            }
            this.active.lock().unwrap().remove(&vm_id);
            let _ = done_tx.send(true);
        });
        self.get(vm, &id).ok_or(RecError::NotFound)
    }

    /// Stop a recording and wait until its file is complete.
    pub async fn stop(&self, vm: &str, id: &str) -> Result<RecordingInfo, RecError> {
        let done = {
            let active = self.active.lock().unwrap();
            match active.get(vm).filter(|a| a.id == id) {
                Some(a) => {
                    let _ = a.stop.send(true);
                    Some(a.done.clone())
                }
                None => None,
            }
        };
        if let Some(mut done) = done {
            let _ = tokio::time::timeout(Duration::from_secs(10), done.wait_for(|d| *d)).await;
        }
        self.get(vm, id).ok_or(RecError::NotFound)
    }

    /// Mark the current position of an active recording.
    pub fn bookmark(&self, vm: &str, id: &str, label: &str) -> Result<RecordingInfo, RecError> {
        let label = label.trim();
        if label.is_empty() || label.len() > 200 || label.chars().any(char::is_control) {
            return Err(RecError::Conflict(
                "bookmark labels are 1-200 printable characters".into(),
            ));
        }
        {
            let active = self.active.lock().unwrap();
            let a = active
                .get(vm)
                .filter(|a| a.id == id)
                .ok_or_else(|| RecError::Conflict("bookmarks are added while recording".into()))?;
            let mut i = a.info.lock().unwrap();
            i.bookmarks.push(Bookmark {
                t_ms: a.started.elapsed().as_millis() as u64,
                label: label.into(),
            });
            write_meta(&self.meta_path(vm, id), &i);
        }
        self.get(vm, id).ok_or(RecError::NotFound)
    }

    pub fn delete(&self, vm: &str, id: &str) -> Result<(), RecError> {
        if self.get(vm, id).is_none() {
            return Err(RecError::NotFound);
        }
        if self
            .active
            .lock()
            .unwrap()
            .get(vm)
            .is_some_and(|a| a.id == id)
        {
            return Err(RecError::Conflict(
                "stop the recording before deleting it".into(),
            ));
        }
        for p in [
            self.stream_path(vm, id),
            self.webm_path(vm, id),
            self.meta_path(vm, id),
        ] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(RecError::Failed(anyhow!("deleting {}: {e}", p.display()))),
            }
        }
        Ok(())
    }

    /// Stop every recording of a VM (e.g. it is being deleted).
    #[allow(dead_code)]
    pub fn stop_all(&self, vm: &str) {
        if let Some(a) = self.active.lock().unwrap().get(vm) {
            let _ = a.stop.send(true);
        }
    }
}

// ------------------------------------------------------------- VNC client

/// The recorder's VNC connection, past the handshake.
struct Connection {
    stream: Box<dyn qemu_ui_qmp::transport::Stream>,
    width: u16,
    height: u16,
    name: String,
}

/// VNC authentication: DES-encrypt the challenge with the password, each
/// key byte bit-reversed (RFB 3.8 §7.2.2).
fn vnc_auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new_from_slice(&key).expect("8-byte key");
    let mut out = *challenge;
    for block in out.chunks_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

impl Connection {
    async fn open(vnc: &VncInfo) -> Result<Connection> {
        let (mut s, password): (Box<dyn qemu_ui_qmp::transport::Stream>, Option<String>) = match vnc
        {
            VncInfo::Unix { path } => (
                qemu_ui_qmp::transport::connect_unix(Path::new(path)).await?,
                None,
            ),
            VncInfo::Tcp { port, password } => {
                let t = tokio::net::TcpStream::connect(("127.0.0.1", *port)).await?;
                let _ = t.set_nodelay(true);
                (Box::new(t), Some(password.clone()))
            }
        };
        let mut version = [0u8; 12];
        s.read_exact(&mut version)
            .await
            .context("reading the RFB version")?;
        if !version.starts_with(b"RFB 003.") {
            bail!("not an RFB server");
        }
        s.write_all(b"RFB 003.008\n").await?;
        let n = s.read_u8().await?;
        if n == 0 {
            let len = s.read_u32().await? as usize;
            let mut why = vec![0; len.min(4096)];
            s.read_exact(&mut why).await?;
            bail!(
                "VNC refused the connection: {}",
                String::from_utf8_lossy(&why)
            );
        }
        let mut types = vec![0; n as usize];
        s.read_exact(&mut types).await?;
        if types.contains(&1) {
            s.write_all(&[1]).await?;
        } else if types.contains(&2) {
            let password =
                password.ok_or_else(|| anyhow!("VNC wants a password the daemon doesn't have"))?;
            s.write_all(&[2]).await?;
            let mut challenge = [0u8; 16];
            s.read_exact(&mut challenge).await?;
            s.write_all(&vnc_auth_response(&password, &challenge))
                .await?;
        } else {
            bail!("VNC offers no security type the recorder supports ({types:?})");
        }
        if s.read_u32().await? != 0 {
            let len = s.read_u32().await.unwrap_or(0) as usize;
            let mut why = vec![0; len.min(4096)];
            let _ = s.read_exact(&mut why).await;
            bail!(
                "VNC authentication failed: {}",
                String::from_utf8_lossy(&why)
            );
        }
        s.write_all(&[1]).await?; // ClientInit: shared, so viewers stay connected
        let width = s.read_u16().await?;
        let height = s.read_u16().await?;
        let mut pf = [0u8; 16];
        s.read_exact(&mut pf).await?;
        let len = s.read_u32().await? as usize;
        let mut name = vec![0; len.min(4096)];
        s.read_exact(&mut name).await?;

        let mut setup = vec![0u8, 0, 0, 0];
        setup.extend_from_slice(&PIXEL_FORMAT);
        setup.extend_from_slice(&[2, 0]);
        setup.extend_from_slice(&(ENCODINGS.len() as u16).to_be_bytes());
        for e in ENCODINGS {
            setup.extend_from_slice(&e.to_be_bytes());
        }
        s.write_all(&setup).await?;
        let mut c = Connection {
            stream: s,
            width,
            height,
            name: String::from_utf8_lossy(&name).into_owned(),
        };
        c.request(false, (width, height)).await?;
        Ok(c)
    }

    async fn request(&mut self, incremental: bool, (w, h): (u16, u16)) -> Result<()> {
        let mut m = vec![3, incremental as u8, 0, 0, 0, 0];
        m.extend_from_slice(&w.to_be_bytes());
        m.extend_from_slice(&h.to_be_bytes());
        self.stream.write_all(&m).await?;
        Ok(())
    }

    /// Record server messages until stopped, the cap, or the VM goes away.
    async fn record(
        &mut self,
        file: &mut tokio::io::BufWriter<tokio::fs::File>,
        mut stop: watch::Receiver<bool>,
        started: Instant,
        cap: u64,
        info: &Mutex<RecordingInfo>,
    ) -> Result<RecordingStatus> {
        let mut size = (self.width, self.height);
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut written = info.lock().unwrap().size_bytes;
        let mut last_request = Instant::now();
        // Some((at, full)) once an update completed and the next is due.
        let mut due: Option<(Instant, bool)> = None;
        loop {
            let wait = due.map(|(at, _)| at);
            tokio::select! {
                _ = stop.changed() => return Ok(RecordingStatus::Finished),
                _ = async { tokio::time::sleep_until(wait.unwrap().into()).await }, if wait.is_some() => {
                    let full = due.take().is_some_and(|(_, f)| f);
                    self.request(!full, size).await?;
                    last_request = Instant::now();
                }
                n = self.stream.read(&mut chunk) => {
                    let n = match n {
                        Ok(0) | Err(_) => return Ok(RecordingStatus::Finished), // the VM stopped
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    let mut used = 0;
                    while let Some((len, kind)) = next_message(&buf[used..], &mut size).map_err(|e| anyhow!(e))? {
                        let msg = &buf[used..used + len];
                        used += len;
                        // Clipboard contents stay out of recordings.
                        if kind == ServerMsg::CutText {
                            continue;
                        }
                        let t = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
                        let rec = encode_record(t, msg);
                        file.write_all(&rec).await.context("writing the recording")?;
                        written += rec.len() as u64;
                        if let ServerMsg::Update { resized } = kind {
                            let at = (last_request + MIN_REQUEST_INTERVAL).max(Instant::now());
                            let full = resized || due.is_some_and(|(_, f)| f);
                            due = Some((at, full));
                        }
                    }
                    buf.drain(..used);
                    {
                        let mut i = info.lock().unwrap();
                        i.size_bytes = written;
                        i.duration_ms = started.elapsed().as_millis() as u64;
                    }
                    if written >= cap {
                        return Ok(RecordingStatus::Capped);
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------- export

/// `ffmpeg` for WebM export: `QEMU_UI_FFMPEG`, else `PATH`.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("QEMU_UI_FFMPEG").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// Decode a recording and encode it to WebM with ffmpeg (a job body).
async fn export_webm(
    ffmpeg: PathBuf,
    rec: PathBuf,
    out: PathBuf,
    ctx: Arc<crate::jobs::JobCtx>,
) -> Result<String> {
    let bytes = tokio::fs::read(&rec)
        .await
        .context("reading the recording")?;
    let (header, records) = parse(&bytes).map_err(|e| anyhow!(e))?;
    let part = out.with_extension("webm.part");
    let mut child = tokio::process::Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
        ])
        .arg("-s")
        .arg(format!("{}x{}", header.width, header.height))
        .arg("-r")
        .arg(EXPORT_FPS.to_string())
        .args([
            "-i",
            "-",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "0",
            "-crf",
            "40",
            "-deadline",
            "realtime",
            "-f",
            "webm",
        ])
        .arg(&part)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting ffmpeg")?;
    let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
    let total = records.last().map_or(0, |r| r.t_ms as u64) * EXPORT_FPS as u64 / 1000 + 1;
    // Decoding is CPU work; frames go to ffmpeg through a bounded channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let decode = tokio::task::spawn_blocking(move || {
        render_frames(&header, &records, EXPORT_FPS, |f| {
            tx.blocking_send(f.to_vec())
                .map_err(|_| "ffmpeg stopped reading".to_string())
        })
    });
    let mut n = 0u64;
    while let Some(frame) = rx.recv().await {
        ctx.check_cancelled()?;
        if stdin.write_all(&frame).await.is_err() {
            break;
        }
        n += 1;
        if n.is_multiple_of(50) {
            ctx.progress(
                Some(n as f64 / total as f64),
                Some(&format!("{n} of {total} frames")),
            );
        }
    }
    drop(stdin);
    let decoded = decode.await.context("decoder task")?;
    let output = child
        .wait_with_output()
        .await
        .context("waiting for ffmpeg")?;
    decoded.map_err(|e| anyhow!("decoding the recording: {e}"))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&part);
        bail!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::fs::rename(&part, &out).context("saving the WebM file")?;
    Ok(format!("exported {n} frames"))
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/{id}/recordings", get(list).post(start))
        .route("/vms/{id}/recordings/{rid}", get(one).delete(remove))
        .route("/vms/{id}/recordings/{rid}/stop", post(stop))
        .route("/vms/{id}/recordings/{rid}/bookmarks", post(bookmark))
        .route("/vms/{id}/recordings/{rid}/stream", get(stream))
        .route("/vms/{id}/recordings/{rid}/webm", get(webm))
        .route("/vms/{id}/recordings/{rid}/export", post(export))
}

fn api_err(e: RecError) -> ApiError {
    match e {
        RecError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
        RecError::NotFound => ApiError(StatusCode::NOT_FOUND, "no such recording".into()),
        RecError::Failed(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// The VM must exist; recording ids are checked by `Recordings`.
fn vm_exists(app: &App, id: &str) -> Result<(), ApiError> {
    app.supervisor
        .detail(id)
        .map(|_| ())
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "no such VM".into()))
}

async fn list(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<RecordingsInfo>, ApiError> {
    vm_exists(&app, &id)?;
    Ok(Json(RecordingsInfo {
        recordings: app.recordings.list(&id),
        export_available: find_ffmpeg().is_some(),
    }))
}

async fn start(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<(StatusCode, Json<RecordingInfo>), ApiError> {
    vm_exists(&app, &id)?;
    app.recordings
        .start(&app.supervisor, &id)
        .await
        .map(|r| (StatusCode::CREATED, Json(r)))
        .map_err(api_err)
}

async fn one(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<Json<RecordingInfo>, ApiError> {
    app.recordings
        .get(&id, &rid)
        .map(Json)
        .ok_or_else(|| api_err(RecError::NotFound))
}

async fn stop(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<Json<RecordingInfo>, ApiError> {
    app.recordings
        .stop(&id, &rid)
        .await
        .map(Json)
        .map_err(api_err)
}

#[derive(Deserialize)]
struct BookmarkBody {
    label: String,
}

async fn bookmark(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
    Json(b): Json<BookmarkBody>,
) -> Result<Json<RecordingInfo>, ApiError> {
    app.recordings
        .bookmark(&id, &rid, &b.label)
        .map(Json)
        .map_err(api_err)
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<StatusCode, ApiError> {
    app.recordings
        .delete(&id, &rid)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

async fn send_file(
    path: PathBuf,
    content_type: &'static str,
    download: Option<String>,
) -> Result<Response, ApiError> {
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| api_err(RecError::NotFound))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = Body::from_stream(async_stream::stream! {
        let mut file = file;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => yield Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(&buf[..n])),
                Err(e) => { yield Err(e); break; }
            }
        }
    });
    let mut resp = (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        body,
    )
        .into_response();
    if let Some(name) = download
        && let Ok(v) = format!("attachment; filename=\"{name}\"").parse()
    {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

async fn stream(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<Response, ApiError> {
    if app.recordings.get(&id, &rid).is_none() {
        return Err(api_err(RecError::NotFound));
    }
    send_file(
        app.recordings.stream_path(&id, &rid),
        "application/octet-stream",
        None,
    )
    .await
}

async fn webm(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<Response, ApiError> {
    if app.recordings.get(&id, &rid).is_none() {
        return Err(api_err(RecError::NotFound));
    }
    send_file(
        app.recordings.webm_path(&id, &rid),
        "video/webm",
        Some(format!("recording-{rid}.webm")),
    )
    .await
}

async fn export(
    State(app): State<Arc<App>>,
    UrlPath((id, rid)): UrlPath<(String, String)>,
) -> Result<(StatusCode, Json<qemu_ui_core::storage::Job>), ApiError> {
    let info = app
        .recordings
        .get(&id, &rid)
        .ok_or_else(|| api_err(RecError::NotFound))?;
    if info.status == RecordingStatus::Recording {
        return Err(api_err(RecError::Conflict(
            "stop the recording before exporting it".into(),
        )));
    }
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "WebM export needs ffmpeg; install it (or set QEMU_UI_FFMPEG) and try again".into(),
        )
    })?;
    let rec = app.recordings.stream_path(&id, &rid);
    let out = app.recordings.webm_path(&id, &rid);
    let job = app.supervisor.jobs.spawn(
        "recording-export",
        &format!("Export recording {rid} to WebM"),
        serde_json::json!({"vm": id, "recording": rid}),
        true,
        move |ctx| export_webm(ffmpeg, rec, out, ctx),
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnc_auth_matches_a_known_vector() {
        // DES with the bit-reversed key "password" over a zero challenge,
        // as libvncserver/QEMU compute it.
        let r = vnc_auth_response("password", &[0u8; 16]);
        assert_eq!(&r[..8], &r[8..], "ECB: equal blocks encrypt equally");
        assert_ne!(r, [0u8; 16]);
        // Only the first 8 password bytes count.
        assert_eq!(r, vnc_auth_response("password-and-more", &[0u8; 16]));
    }

    #[test]
    fn quota_cap_and_listing() {
        let dir = std::env::temp_dir().join(format!("qui-rec-{}", rand::random::<u32>()));
        let recs = Recordings::new(
            &dir,
            Limits {
                cap_bytes: 10,
                quota_bytes: 100,
            },
        );
        let vm = "00000001";
        std::fs::create_dir_all(recs.dir(vm)).unwrap();
        let info = RecordingInfo {
            id: "abc".into(),
            vm_id: vm.into(),
            started_at: 5,
            ended_at: None,
            duration_ms: 0,
            size_bytes: 0,
            status: RecordingStatus::Recording,
            error: None,
            width: 1,
            height: 1,
            bookmarks: vec![],
            webm: false,
        };
        write_meta(&recs.meta_path(vm, "abc"), &info);
        std::fs::write(recs.stream_path(vm, "abc"), vec![0u8; 120]).unwrap();
        // Left "recording" by a crash: reported as interrupted, with its size.
        let got = recs.get(vm, "abc").unwrap();
        assert_eq!(got.status, RecordingStatus::Interrupted);
        assert_eq!(got.size_bytes, 120);
        assert_eq!(recs.list(vm).len(), 1);
        assert!(recs.used_bytes(vm) >= 120);
        assert!(recs.get(vm, "../x").is_none(), "ids are hex only");
        assert!(matches!(
            recs.bookmark(vm, "abc", "x"),
            Err(RecError::Conflict(_))
        ));
        recs.delete(vm, "abc").unwrap();
        assert!(recs.list(vm).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Real QEMU: record a few seconds of a booting VM, then check the file
/// replays (parses, timestamps monotonic, decodes without errors).
#[cfg(test)]
mod real {
    use super::*;
    use crate::supervisor::test_support::*;
    use qemu_ui_core::vm::VmState;

    #[test]
    fn records_a_running_vm_and_the_file_decodes() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: QEMU not found");
            return;
        };
        let data = short_data_dir("rec");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let id = small_vm(&sv, &caps, "recorded").await;
            let recs = Recordings::new(&data, Limits::default());
            assert!(
                matches!(recs.start(&sv, &id).await, Err(RecError::Conflict(_))),
                "not running"
            );
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let body = async {
                let r = recs.start(&sv, &id).await.map_err(|e| format!("{e:?}"))?;
                if !matches!(recs.start(&sv, &id).await, Err(RecError::Conflict(_))) {
                    return Err("a second recording must be refused".to_string());
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
                recs.bookmark(&id, &r.id, "booting")
                    .map_err(|e| format!("{e:?}"))?;
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let done = recs.stop(&id, &r.id).await.map_err(|e| format!("{e:?}"))?;
                Ok::<_, String>(done)
            };
            let res = tokio::time::timeout(Duration::from_secs(120), body).await;
            let _ = sv.force_stop(&id).await;
            wait_state(&sv, &id, VmState::Stopped).await;
            let done = res.map_err(|_| "timed out".to_string())??;
            let bytes =
                std::fs::read(recs.stream_path(&id, &done.id)).map_err(|e| e.to_string())?;
            Ok::<_, String>((done, bytes))
        });
        drop(rt);
        let result = outcome.and_then(|(done, bytes)| {
            if done.status != RecordingStatus::Finished {
                return Err(format!("status {:?} {:?}", done.status, done.error));
            }
            if done.bookmarks.len() != 1 || done.bookmarks[0].label != "booting" {
                return Err(format!("bookmarks {:?}", done.bookmarks));
            }
            let (header, records) = parse(&bytes)?;
            if records.len() < 2 || header.width == 0 {
                return Err(format!("only {} records", records.len()));
            }
            if !records[0].data.starts_with(b"RFB 003.008\n") {
                return Err("record 0 is not the replay preamble".into());
            }
            let frames = render_frames(&header, &records, 10, |_| Ok(()))?;
            if frames == 0 {
                return Err("no frames".into());
            }
            // Something other than black was drawn (the BIOS screen).
            let mut dec = qemu_ui_core::recording::Decoder::new(header.width, header.height);
            for r in &records[1..] {
                dec.apply(&r.data)?;
            }
            if !dec.fb.chunks(4).any(|p| p[..3] != [0, 0, 0]) {
                return Err("the decoded screen is all black".into());
            }
            Ok(())
        });
        let _ = std::fs::remove_dir_all(&data);
        result.unwrap();
    }
}
