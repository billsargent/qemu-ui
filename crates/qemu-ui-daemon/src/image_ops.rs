//! Image operations (plan.md §9.1, M2-04): resize, compact, rename.
//!
//! * Resize: offline with `qemu-img resize` (grow; shrink only when asked
//!   explicitly), or live with QMP `block_resize` when a running VM has the
//!   image open (grow only).
//! * Compact: rewrite a closed qcow2/raw image into a hidden temp file in the
//!   same pool, keeping its backing file reference so chains stay intact,
//!   verify it with `qemu-img check`, then atomically replace the original.
//! * Rename: within a pool, refused while anything refers to the image.
//!
//! One operation per image at a time; a second one is refused with 409.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::storage::Job;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{ApiError, App};
use crate::images::Pools;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/pools/{pool}/images/{name}/resize", post(resize))
        .route("/pools/{pool}/images/{name}/compact", post(compact))
        .route("/pools/{pool}/images/{name}/rename", post(rename))
}

/// Images with an operation in flight.
static BUSY: Mutex<Option<HashSet<ImageRef>>> = Mutex::new(None);

/// Held while an operation runs on an image; released on drop.
struct BusyGuard(ImageRef);

impl BusyGuard {
    fn acquire(r: &ImageRef) -> Result<BusyGuard, ApiError> {
        let mut busy = BUSY.lock().unwrap();
        if !busy.get_or_insert_with(HashSet::new).insert(r.clone()) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("another operation on '{}' is still running", r.name),
            ));
        }
        Ok(BusyGuard(r.clone()))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(b) = BUSY.lock().unwrap().as_mut() {
            b.remove(&self.0);
        }
    }
}

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

fn bad(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))
}

async fn qemu_img(app: &App) -> Result<PathBuf, ApiError> {
    app.supervisor
        .qemu_img()
        .await
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "qemu-img is not installed"))
}

fn writable(app: &App, r: &ImageRef) -> Result<PathBuf, ApiError> {
    let path = app
        .supervisor
        .pools
        .resolve(r)
        .map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    if app.supervisor.pools.get(&r.pool).map_err(bad)?.readonly {
        return Err(err(
            StatusCode::CONFLICT,
            format!("pool '{}' is read-only", r.pool),
        ));
    }
    Ok(path)
}

async fn no_dependents(app: &App, q: &Path, path: &Path, what: &str) -> Result<(), ApiError> {
    let deps = app
        .supervisor
        .pools
        .dependents(q, path)
        .await
        .map_err(bad)?;
    if deps.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = deps
        .iter()
        .map(|d| format!("{}/{}", d.pool, d.name))
        .collect();
    Err(err(
        StatusCode::CONFLICT,
        format!(
            "cannot {what}: it is the backing file of {}",
            names.join(", ")
        ),
    ))
}

// ----------------------------------------------------------------- resize

#[derive(Deserialize)]
struct ResizeBody {
    size_bytes: u64,
    /// Allow making the image smaller. Data beyond the new end is lost.
    #[serde(default)]
    shrink: bool,
}

async fn resize(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<ResizeBody>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let r = ImageRef { pool, name };
    let path = writable(&app, &r)?;
    let q = qemu_img(&app).await?;
    let (format, current, _) = app.supervisor.pools.info(&q, &path).await.map_err(bad)?;
    if b.size_bytes == current {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the image already has that size",
        ));
    }
    if b.size_bytes < 1024 * 1024 {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "size must be at least 1 MiB",
        ));
    }
    let shrinking = b.size_bytes < current;
    if shrinking && !b.shrink {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the new size is smaller than the current one; set shrink to confirm (data beyond the new end is lost)",
        ));
    }
    if !matches!(format.as_str(), "qcow2" | "raw") {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("resizing {format} images is not supported; convert to qcow2 first"),
        ));
    }
    // A backing file must keep its size, or its overlays become inconsistent.
    no_dependents(&app, &q, &path, "resize this image").await?;

    let live = app.supervisor.live_disks(&r);
    let open = app.supervisor.image_users().1;
    if live.is_empty() && open.contains_key(&r) {
        return Err(err(
            StatusCode::CONFLICT,
            "the VM using this image is starting or stopping; try again shortly",
        ));
    }
    if live.len() > 1 {
        let names: Vec<&str> = live.iter().map(|l| l.0.as_str()).collect();
        return Err(err(
            StatusCode::CONFLICT,
            format!("'{}' is open in several VMs ({})", r.name, names.join(", ")),
        ));
    }
    let guard = BusyGuard::acquire(&r)?;
    let target = json!({"pool": r.pool, "name": r.name});

    if let Some((vm, qmp, node, readonly)) = live.into_iter().next() {
        if shrinking {
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "'{}' is open in running VM {vm}; shrinking is only possible while it is stopped",
                    r.name
                ),
            ));
        }
        if readonly {
            return Err(err(
                StatusCode::CONFLICT,
                format!("'{}' is attached read-only to {vm}", r.name),
            ));
        }
        let title = format!(
            "Grow {}/{} to {} bytes (live, in {vm})",
            r.pool, r.name, b.size_bytes
        );
        let job = app.supervisor.jobs.spawn("image.resize", &title, target, false, move |ctx| async move {
            let _guard = guard;
            ctx.log_line(&format!("block_resize node-name={node} size={}", b.size_bytes));
            qmp.execute("block_resize", Some(json!({"node-name": node, "size": b.size_bytes})))
                .await
                .map_err(|e| anyhow::anyhow!("QEMU refused to resize: {e}"))?;
            Ok(format!("Grown to {} bytes; the guest sees the new size (extend its partition to use it)", b.size_bytes))
        });
        return Ok((StatusCode::ACCEPTED, Json(job)));
    }

    let title = format!(
        "{} {}/{} to {} bytes",
        if shrinking { "Shrink" } else { "Grow" },
        r.pool,
        r.name,
        b.size_bytes
    );
    let job = app.supervisor.jobs.spawn(
        "image.resize",
        &title,
        target,
        false,
        move |ctx| async move {
            let _guard = guard;
            let p = path.display().to_string();
            let size = b.size_bytes.to_string();
            let mut args = vec!["resize", "-f", format.as_str()];
            if shrinking {
                args.push("--shrink");
            }
            args.extend([p.as_str(), size.as_str()]);
            ctx.progress(None, Some("Resizing"));
            run_plain(&ctx, &q, &args).await?;
            Ok(format!("Resized to {} bytes", b.size_bytes))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Run qemu-img without `-p` (subcommands like `resize` do not support it).
async fn run_plain(ctx: &crate::jobs::JobCtx, q: &Path, args: &[&str]) -> anyhow::Result<()> {
    ctx.log_line(&format!("$ qemu-img {}", args.join(" ")));
    let out = crate::probe::command(q).args(args).output().await?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        ctx.log_line(stderr.trim());
    }
    if !out.status.success() {
        anyhow::bail!("qemu-img {}: {}", args[0], stderr.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------- compact

#[derive(Deserialize)]
struct CompactBody {
    /// qcow2 only: compress clusters (smaller, slower to read).
    #[serde(default)]
    compress: bool,
}

async fn compact(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<CompactBody>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let r = ImageRef { pool, name };
    let path = writable(&app, &r)?;
    let q = qemu_img(&app).await?;
    let (_, open) = app.supervisor.image_users();
    if let Some(vms) = open.get(&r) {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "'{}' is open in running VM(s) {}; stop them first",
                r.name,
                vms.join(", ")
            ),
        ));
    }
    let info = app
        .supervisor
        .pools
        .info_json(&q, &path)
        .await
        .map_err(bad)?;
    let format = info["format"].as_str().unwrap_or("raw").to_string();
    if !matches!(format.as_str(), "qcow2" | "raw") {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("compacting {format} images is not supported"),
        ));
    }
    if info["snapshots"].as_array().is_some_and(|s| !s.is_empty()) {
        return Err(err(
            StatusCode::CONFLICT,
            "this image has internal snapshots, which compaction would discard; delete them first",
        ));
    }
    // Keep the overlay's backing reference exactly as recorded (relative or
    // absolute), so the chain is unchanged.
    let backing = info["backing-filename"].as_str().map(String::from);
    let backing_fmt = info["backing-filename-format"].as_str().map(String::from);
    if backing.is_some() && format != "qcow2" {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "only qcow2 overlays can be compacted",
        ));
    }
    let need = Pools::measure(&q, Some(&path), None, &format)
        .await
        .map(|m| m.required)
        .unwrap_or(0);
    app.supervisor
        .pools
        .check_quota(Some(&q), &r.pool, need)
        .await
        .map_err(bad)?;
    let cluster = info["cluster-size"].as_u64();
    let before = info["actual-size"].as_u64().unwrap_or(0);

    let guard = BusyGuard::acquire(&r)?;
    let tmp = path.with_file_name(format!(
        ".{}.compact-{:06x}",
        r.name,
        rand::random::<u32>() & 0xff_ffff
    ));
    let title = format!(
        "Compact {}/{}{}",
        r.pool,
        r.name,
        if b.compress { " (compressed)" } else { "" }
    );
    let target = json!({"pool": r.pool, "name": r.name});
    let sv = app.supervisor.clone();
    let job = app.supervisor.jobs.spawn(
        "image.compact",
        &title,
        target,
        true,
        move |ctx| async move {
            let _guard = guard;
            let (src, dst) = (path.display().to_string(), tmp.display().to_string());
            let opts = cluster.map(|c| format!("cluster_size={c}"));
            let mut args: Vec<&str> = vec!["convert", "-O", format.as_str()];
            if format == "qcow2" {
                if let Some(o) = &opts {
                    args.extend(["-o", o.as_str()]);
                }
                if b.compress {
                    args.push("-c");
                }
            }
            if let Some(bf) = &backing {
                // Only data that differs from the backing file is copied.
                args.extend(["-B", bf.as_str()]);
                if let Some(f) = &backing_fmt {
                    args.extend(["-F", f.as_str()]);
                }
            }
            args.extend([src.as_str(), dst.as_str()]);
            let result = async {
                ctx.run_qemu_img(&q, &args).await?;
                ctx.progress(None, Some("Verifying"));
                run_check(&ctx, &q, &format, &tmp).await?;
                // The image must still be closed before it is replaced.
                if sv.image_users().1.contains_key(&r) {
                    anyhow::bail!(
                        "a VM opened the image during compaction; the original is unchanged"
                    );
                }
                std::fs::rename(&tmp, &path)?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result?;
            let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let after = sv.pools.info(&q, &path).await.map(|i| i.2).unwrap_or(after);
            Ok(format!("Compacted: {} → {} bytes on disk", before, after))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn run_check(
    ctx: &crate::jobs::JobCtx,
    q: &Path,
    format: &str,
    path: &Path,
) -> anyhow::Result<()> {
    if format != "qcow2" {
        return Ok(()); // raw images have no metadata to check
    }
    let out = crate::probe::command(q)
        .args(["check", "-f", "qcow2", "--output=json"])
        .arg(path)
        .output()
        .await?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    ctx.log_line(&format!("check: {v}"));
    let errors = v["corruptions"].as_u64().unwrap_or(0) + v["check-errors"].as_u64().unwrap_or(0);
    // Exit 1 = could not check; 2 = corruptions. Leaks alone (3) are harmless.
    if errors > 0 || out.status.code() == Some(1) {
        anyhow::bail!("the compacted copy failed its integrity check; the original is unchanged");
    }
    Ok(())
}

// ----------------------------------------------------------------- rename

#[derive(Deserialize)]
struct RenameBody {
    new_name: String,
}

/// Rename within the pool. Refused while a VM references the image or it is
/// another image's backing file, since both refer to it by name.
async fn rename(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<RenameBody>,
) -> Result<StatusCode, ApiError> {
    let r = ImageRef {
        pool: pool.clone(),
        name,
    };
    let path = writable(&app, &r)?;
    let to = ImageRef {
        pool,
        name: b.new_name,
    };
    let new_path = app.supervisor.pools.new_image_path(&to).map_err(bad)?;
    let (used, open) = app.supervisor.image_users();
    if let Some(vms) = used.get(&r).or(open.get(&r)) {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "'{}' is used by {}; detach it from those VMs before renaming",
                r.name,
                vms.join(", ")
            ),
        ));
    }
    let q = qemu_img(&app).await?;
    no_dependents(&app, &q, &path, "rename this image").await?;
    let _guard = BusyGuard::acquire(&r)?;
    std::fs::rename(&path, &new_path).map_err(|e| bad(e.into()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[path = "image_ops_test.rs"]
mod tests;
