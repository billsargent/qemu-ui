//! Download an image from a URL into a pool (plan.md §9.4, M2-08).
//!
//! `POST /pools/{pool}/fetch {url, name?, sha256?}` starts an `image.fetch`
//! job. The download is a server-side request made on behalf of the browser,
//! so it is guarded against SSRF (rules in `qemu_ui_core::fetch`):
//!
//! * The URL (and every redirect target) is validated: https, or http only
//!   with a SHA-256 checksum; no user info.
//! * Name resolution goes through [`GuardedResolver`], which refuses the whole
//!   request if *any* resolved address is not allowed. The HTTP client then
//!   connects to exactly those checked addresses, so DNS rebinding between
//!   the check and the connection is impossible.
//! * Redirects are followed by hand (at most `MAX_REDIRECTS`), each hop
//!   re-validated and re-resolved through the same resolver.
//! * No proxy is used (a proxy would do its own, unchecked resolution).
//! * Size is capped by the pool quota and the free disk space, up front when
//!   the server sends `Content-Length` and while streaming otherwise.
//!
//! Data goes to `.fetch-<id>.part` in the target pool (not listed as an
//! image), is verified against the optional SHA-256, identified with
//! `qemu-img info`, and renamed into place. Failure removes the partial file.
//!
//! Read-only remote attach (booting straight from a URL with QEMU's https
//! block driver) is deliberately not offered: QEMU resolves the host itself,
//! so the address pinning above could not be enforced for it.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use qemu_ui_core::fetch::{self, MAX_REDIRECTS};
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::storage::Job;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::Url;

use crate::api::{ApiError, App};

/// Largest download accepted regardless of quota.
pub const MAX_FETCH_BYTES: u64 = 256 << 30;

/// Opt-in for private network ranges (RFC 1918, CGNAT, ULA), set from the
/// daemon's `--allow-private-fetch` flag. Loopback and link-local are never
/// allowed through this.
static ALLOW_PRIVATE: AtomicBool = AtomicBool::new(false);

pub fn set_allow_private(v: bool) {
    ALLOW_PRIVATE.store(v, Ordering::Relaxed);
}

/// Tests serve files from 127.0.0.1; this override exists only in test
/// builds and is not reachable from the API.
#[cfg(test)]
static ALLOW_LOOPBACK_FOR_TESTS: AtomicBool = AtomicBool::new(false);

fn allow_loopback() -> bool {
    #[cfg(test)]
    {
        ALLOW_LOOPBACK_FOR_TESTS.load(Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Check one address against the policy.
fn guard_ip(ip: std::net::IpAddr, allow_private: bool) -> Result<(), String> {
    if allow_loopback() && ip.is_loopback() {
        return Ok(());
    }
    fetch::check_ip(ip, allow_private)
}

/// A refusal from the guard, carried through ureq's error type.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Resolves with the system resolver, then refuses the request if any
/// address is not allowed. ureq connects only to what this returns.
#[derive(Debug)]
struct GuardedResolver {
    allow_private: bool,
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = DefaultResolver::default().resolve(uri, config, timeout)?;
        let host = uri.host().unwrap_or_default();
        for a in addrs.iter() {
            if let Err(why) = guard_ip(a.ip(), self.allow_private) {
                return Err(ureq::Error::Other(Box::new(Refused(format!(
                    "{host} resolves to {why}"
                )))));
            }
        }
        Ok(addrs)
    }
}

fn agent(allow_private: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent("qemu-ui")
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // A stalled server fails the job (and frees a cancelled one) within
        // a minute; a slow but steady transfer may take as long as it needs.
        .timeout_per_read(Some(Duration::from_secs(60)))
        .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::new(),
        GuardedResolver { allow_private },
    )
}

/// Turn a ureq error into a plain explanation.
fn explain(e: ureq::Error) -> anyhow::Error {
    if let ureq::Error::Other(inner) = &e
        && let Some(r) = inner.downcast_ref::<Refused>()
    {
        return anyhow!("refused: {r}");
    }
    match e {
        ureq::Error::HostNotFound => anyhow!("the host name could not be resolved"),
        ureq::Error::Timeout(t) => anyhow!("the server stopped responding ({t})"),
        other => anyhow!("download failed: {other}"),
    }
}

/// Progress shared between the download thread and the job.
#[derive(Default)]
struct Shared {
    done: AtomicU64,
    /// 0 = unknown.
    total: AtomicU64,
    cancel: AtomicBool,
}

struct Download {
    url: Url,
    has_checksum: bool,
    sha256: Option<String>,
    part: PathBuf,
    /// Most bytes allowed (quota, free space, global cap).
    limit: u64,
    allow_private: bool,
}

/// Blocking download into `part`. Removes `part` on any failure.
fn download(d: &Download, shared: &Shared, log: &dyn Fn(&str)) -> Result<String> {
    let res = download_inner(d, shared, log);
    if res.is_err() {
        let _ = std::fs::remove_file(&d.part);
    }
    res
}

fn download_inner(d: &Download, shared: &Shared, log: &dyn Fn(&str)) -> Result<String> {
    let agent = agent(d.allow_private);
    let mut url = d.url.clone();
    for hop in 0..=MAX_REDIRECTS {
        log(&format!("GET {url}"));
        let mut resp = agent.get(url.as_str()).call().map_err(explain)?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            if hop == MAX_REDIRECTS {
                bail!("too many redirects (more than {MAX_REDIRECTS})");
            }
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("the server redirected ({status}) without a location"))?;
            url = fetch::redirect_target(&url, location, d.has_checksum).map_err(|e| anyhow!(e))?;
            log(&format!("redirected ({status}) to {url}"));
            continue;
        }
        if !(200..300).contains(&status) {
            bail!("the server answered {status} for {url}");
        }
        let length = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(len) = length
            && len > d.limit
        {
            bail!(
                "the file is {len} bytes, more than the {} bytes this pool can take",
                d.limit
            );
        }
        shared.total.store(length.unwrap_or(0), Ordering::Relaxed);

        let mut out = File::create_new(&d.part)?;
        let mut hasher = Sha256::new();
        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0u8; 256 * 1024];
        let mut written: u64 = 0;
        loop {
            if shared.cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| anyhow!("download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > d.limit {
                bail!(
                    "the download exceeded the {} bytes this pool can take",
                    d.limit
                );
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            shared.done.store(written, Ordering::Relaxed);
        }
        if let Some(len) = length
            && written != len
        {
            bail!("the connection closed after {written} of {len} bytes");
        }
        out.sync_all()?;
        drop(out);
        let digest = hex::encode(hasher.finalize());
        if let Some(want) = &d.sha256
            && !want.eq_ignore_ascii_case(&digest)
        {
            bail!("checksum mismatch: expected {want}, got {digest}");
        }
        log(&format!("{written} bytes, sha256 {digest}"));
        return Ok(digest);
    }
    unreachable!("the loop returns or bails")
}

/// Names with a download in flight, so two fetches cannot race for one.
static IN_FLIGHT: Mutex<Option<HashSet<ImageRef>>> = Mutex::new(None);

struct InFlight(ImageRef);

impl InFlight {
    fn claim(r: &ImageRef) -> Option<InFlight> {
        let mut g = IN_FLIGHT.lock().unwrap();
        g.get_or_insert_with(HashSet::new)
            .insert(r.clone())
            .then(|| InFlight(r.clone()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(s) = IN_FLIGHT.lock().unwrap().as_mut() {
            s.remove(&self.0);
        }
    }
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

pub fn routes() -> Router<std::sync::Arc<App>> {
    Router::new().route("/pools/{pool}/fetch", post(start_fetch))
}

#[derive(Deserialize)]
struct FetchBody {
    url: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, msg.into())
}

async fn start_fetch(
    State(app): State<std::sync::Arc<App>>,
    UrlPath(pool): UrlPath<String>,
    Json(b): Json<FetchBody>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let sha256 = match b.sha256.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) if fetch::valid_sha256(s) => Some(s.to_ascii_lowercase()),
        Some(_) => return Err(bad("the checksum must be a SHA-256 in hex (64 characters)")),
        None => None,
    };
    let url = fetch::validate_url(&b.url, sha256.is_some()).map_err(bad)?;
    let name = match b.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => fetch::default_name(&url)
            .ok_or_else(|| bad("could not derive a file name from the URL; please give one"))?,
    };
    let r = ImageRef {
        pool: pool.clone(),
        name: name.clone(),
    };
    let dest = app
        .supervisor
        .pools
        .new_image_path(&r)
        .map_err(|e| bad(format!("{e:#}")))?;
    let claim = InFlight::claim(&r).ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            format!("'{name}' is already being downloaded"),
        )
    })?;

    // Fail fast on hosts the guard refuses, so the form can say so at once.
    // The download re-resolves through the same guard and connects only to
    // the addresses it approves.
    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let allow_private = ALLOW_PRIVATE.load(Ordering::Relaxed);
    let addrs = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        (host.trim_matches(['[', ']']), port)
            .to_socket_addrs()
            .map(|a| a.collect::<Vec<_>>())
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(|_| bad("the host name could not be resolved"))?;
    for a in &addrs {
        guard_ip(a.ip(), allow_private).map_err(|why| {
            bad(format!(
                "refused: {} resolves to {why}",
                url.host_str().unwrap_or_default()
            ))
        })?;
    }

    // Size limit: quota remaining, free disk space, global cap.
    let q = app.supervisor.qemu_img().await;
    let row = app
        .supervisor
        .pools
        .get(&pool)
        .map_err(|e| bad(format!("{e:#}")))?;
    let mut limit = MAX_FETCH_BYTES;
    if let Some(quota) = row.quota_bytes {
        let used: u64 = app
            .supervisor
            .pools
            .list(q.as_deref(), &pool)
            .await
            .map_err(|e| bad(format!("{e:#}")))?
            .images
            .iter()
            .map(|i| i.actual_size)
            .sum();
        limit = limit.min(quota.saturating_sub(used));
    }
    if let Some(free) = dest.parent().and_then(fs_free) {
        limit = limit.min(free);
    }
    if limit == 0 {
        return Err(bad(format!(
            "pool '{pool}' has no space left (quota or disk)"
        )));
    }

    let id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
    let part = dest.with_file_name(format!(".fetch-{id}.part"));
    let d = Download {
        url: url.clone(),
        has_checksum: sha256.is_some(),
        sha256,
        part: part.clone(),
        limit,
        allow_private,
    };
    let pools = app.supervisor.pools.clone();
    let title = format!("Download {} into {pool}", url);
    let job = app.supervisor.jobs.spawn_unthrottled(
        "image.fetch",
        &title,
        json!({"pool": pool, "name": name, "url": url.as_str()}),
        true,
        move |ctx| async move {
            let _claim = claim;
            let shared = Arc::new(Shared::default());
            let worker = {
                let shared = shared.clone();
                let ctx = ctx.clone();
                tokio::task::spawn_blocking(move || download(&d, &shared, &|l| ctx.log_line(l)))
            };
            tokio::pin!(worker);
            let digest = loop {
                tokio::select! {
                    r = &mut worker => break r.map_err(|e| anyhow!("download task failed: {e}"))??,
                    _ = ctx.cancelled() => {
                        // The worker notices at its next read and removes the partial file.
                        shared.cancel.store(true, Ordering::Relaxed);
                        bail!("cancelled");
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        let (done, total) = (shared.done.load(Ordering::Relaxed), shared.total.load(Ordering::Relaxed));
                        let msg = format!("{} downloaded", human(done));
                        ctx.progress((total > 0).then(|| done as f64 / total as f64), Some(&msg));
                    }
                }
            };
            // Identify the result, then put it in place.
            let format = match &q {
                Some(q) => pools.info(q, &part).await.map(|(f, _, _)| f).unwrap_or_else(|_| "raw".into()),
                None => "raw".into(),
            };
            if dest.exists() {
                let _ = std::fs::remove_file(&part);
                bail!("'{}' appeared in the pool while downloading; the download was discarded", r.name);
            }
            if let Err(e) = std::fs::rename(&part, &dest) {
                let _ = std::fs::remove_file(&part);
                bail!("could not move the download into place: {e}");
            }
            ctx.log_line(&format!("saved as {} ({format})", r.name));
            Ok(format!("Downloaded {} ({format}, sha256 {}…)", r.name, &digest[..12]))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

fn human(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

/// URL fetch end to end through the real router, against a tiny HTTP server
/// on 127.0.0.1 (reachable only through the test-only loopback override).
#[cfg(test)]
mod real {
    use super::ALLOW_LOOPBACK_FOR_TESTS;
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Serve canned responses; returns the base URL.
    fn serve(qcow2: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = qcow2.clone();
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut line = String::new();
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    if r.read_line(&mut line).is_err() {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let send = |s: &mut std::net::TcpStream, head: &str, body: &[u8]| {
                        let _ = s.write_all(head.as_bytes());
                        let _ = s.write_all(body);
                    };
                    match path.as_str() {
                        "/disk.qcow2" => send(
                            &mut s,
                            &format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            ),
                            &body,
                        ),
                        "/stream.bin" => send(
                            &mut s,
                            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
                            &vec![7u8; 3 << 20],
                        ),
                        "/hop" => send(
                            &mut s,
                            "HTTP/1.1 302 Found\r\nLocation: /disk.qcow2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            b"",
                        ),
                        "/to-metadata" => send(
                            &mut s,
                            "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            b"",
                        ),
                        "/slow.bin" => {
                            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 104857600\r\nConnection: close\r\n\r\n");
                            for _ in 0..600 {
                                if s.write_all(&[1u8; 4096]).is_err() {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                        _ => send(
                            &mut s,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            b"",
                        ),
                    }
                });
            }
        });
        base
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:7480")
            .header("authorization", format!("Bearer {T}"));
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let req = b
            .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn finish(router: &axum::Router, job: &Value) -> Value {
        let id = job["id"]
            .as_str()
            .unwrap_or_else(|| panic!("not a job: {job}"));
        for _ in 0..600 {
            let (_, j) = call(router, "GET", &format!("/api/v1/jobs/{id}"), None).await;
            if !matches!(j["status"].as_str(), Some("queued" | "running")) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {id} did not finish");
    }

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn partials(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".fetch-"))
            .collect()
    }

    #[test]
    fn downloads_are_guarded_verified_and_cleaned_up() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        ALLOW_LOOPBACK_FOR_TESTS.store(true, Ordering::Relaxed);
        let data = short_data_dir("fetch");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // A real qcow2 to serve.
            let src = data.join("src.qcow2");
            let ok = crate::probe::command(&img).args(["create", "-q", "-f", "qcow2"]).arg(&src).arg("16M").status().await.unwrap();
            assert!(ok.success());
            let qcow2 = std::fs::read(&src).unwrap();
            let digest = sha(&qcow2);
            let base = serve(qcow2.clone());

            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);
            let pool_dir = sv.pools.pool_dir("default").unwrap();
            let fetch = |body: Value| {
                let r = r.clone();
                async move { call(&r, "POST", "/api/v1/pools/default/fetch", Some(body)).await }
            };

            // Request-level refusals.
            let (s, e) = fetch(json!({"url": format!("{base}/disk.qcow2")})).await;
            assert_eq!(s, 422, "plain http needs a checksum: {e}");
            assert!(e["error"].as_str().unwrap().contains("SHA-256"));
            for bad in ["ftp://example.org/x.iso", "file:///etc/passwd", "https://u:p@example.org/x.iso"] {
                let (s, _) = fetch(json!({"url": bad, "sha256": digest})).await;
                assert_eq!(s, 422, "{bad}");
            }
            let (s, e) = fetch(json!({"url": "http://169.254.169.254/latest/meta-data/", "name": "m.iso", "sha256": digest})).await;
            assert_eq!(s, 422, "metadata endpoint refused up front: {e}");
            assert!(e["error"].as_str().unwrap().contains("link-local"), "{e}");
            let (s, _) = fetch(json!({"url": format!("{base}/disk.qcow2"), "name": "../x.qcow2", "sha256": digest})).await;
            assert_eq!(s, 422, "path traversal in the name");

            // A full download through a redirect.
            let (s, job) = fetch(json!({"url": format!("{base}/hop"), "name": "fetched.qcow2", "sha256": digest})).await;
            assert_eq!(s, 202, "{job}");
            assert_eq!(job["kind"], "image.fetch");
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "succeeded", "{fin}");
            assert!(fin["message"].as_str().unwrap().contains("qcow2"), "{fin}");
            assert_eq!(std::fs::read(pool_dir.join("fetched.qcow2")).unwrap(), qcow2, "byte-identical");
            let (_, d) = call(&r, "GET", "/api/v1/pools/default/images/fetched.qcow2", None).await;
            assert_eq!(d["format"], "qcow2");
            // The same name again is refused.
            let (s, _) = fetch(json!({"url": format!("{base}/disk.qcow2"), "name": "fetched.qcow2", "sha256": digest})).await;
            assert_eq!(s, 422);

            // A redirect to the metadata service is refused at the redirect.
            let (s, job) = fetch(json!({"url": format!("{base}/to-metadata"), "name": "meta.bin", "sha256": digest})).await;
            assert_eq!(s, 202);
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "failed");
            assert!(fin["error"].as_str().unwrap().contains("refused"), "{fin}");

            // Checksum mismatch: failed, nothing left behind.
            let (_, job) = fetch(json!({"url": format!("{base}/disk.qcow2"), "name": "wrong.qcow2", "sha256": "0".repeat(64)})).await;
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "failed");
            assert!(fin["error"].as_str().unwrap().contains("checksum mismatch"), "{fin}");
            assert!(!pool_dir.join("wrong.qcow2").exists());

            // 404 is reported plainly.
            let (_, job) = fetch(json!({"url": format!("{base}/missing.iso"), "sha256": digest})).await;
            let fin = finish(&r, &job).await;
            assert!(fin["error"].as_str().unwrap().contains("404"), "{fin}");

            // Quota: known length refused before writing; unknown length
            // stopped while streaming.
            let used: u64 = sv.pools.list(Some(&img), "default").await.unwrap().images.iter().map(|i| i.actual_size).sum();
            sv.pools.update("default", Some(used + (1 << 20)), false).unwrap();
            let (_, job) = fetch(json!({"url": format!("{base}/disk.qcow2"), "name": "big.qcow2", "sha256": digest})).await;
            let fin = finish(&r, &job).await;
            if qcow2.len() as u64 > (1 << 20) {
                assert_eq!(fin["status"], "failed", "{fin}");
            }
            let (_, job) = fetch(json!({"url": format!("{base}/stream.bin"), "name": "stream.bin", "sha256": "a".repeat(64)})).await;
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "failed");
            assert!(fin["error"].as_str().unwrap().contains("exceeded"), "{fin}");
            sv.pools.update("default", None, false).unwrap();

            // Cancellation of a trickling download.
            let (_, job) = fetch(json!({"url": format!("{base}/slow.bin"), "name": "slow.bin", "sha256": "a".repeat(64)})).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            let (s, _) = call(&r, "POST", &format!("/api/v1/jobs/{}/cancel", job["id"].as_str().unwrap()), None).await;
            assert_eq!(s, 202);
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "cancelled", "{fin}");

            // No partial files survive any of this.
            for _ in 0..50 {
                if partials(&pool_dir).is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(partials(&pool_dir).is_empty(), "{:?}", partials(&pool_dir));
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
