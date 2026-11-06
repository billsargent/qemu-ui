//! Storage and job routes (M2-01 … M2-03).
//!
//! Pools:  GET/POST /pools, PUT/DELETE /pools/{pool}, GET /pools/{pool}
//! Images: POST /pools/{pool}/images (job), GET/DELETE /pools/{pool}/images/{name},
//!         POST …/{name}/check, POST …/{name}/measure, POST /pools/{pool}/measure
//! Jobs:   GET /jobs, GET /jobs/{id}, POST /jobs/{id}/cancel, GET /jobs/{id}/log

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::storage::{CheckResult, ImageDetail, Job, MeasureResult, PoolSummary};
use qemu_ui_core::vm::PoolInfo;
use serde::Deserialize;
use serde_json::json;

use crate::api::{ApiError, ApiResult, App};
use crate::images::Pools;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/pools", get(list_pools).post(add_pool))
        .route(
            "/pools/{pool}",
            get(pool).put(update_pool).delete(remove_pool),
        )
        .route("/pools/{pool}/measure", post(measure_new))
        .route("/pools/{pool}/images", post(create_image))
        .route(
            "/pools/{pool}/images/{name}",
            get(image).delete(delete_image),
        )
        .route("/pools/{pool}/images/{name}/check", post(check_image))
        .route("/pools/{pool}/images/{name}/measure", post(measure_image))
        .route("/pools/{pool}/images/{name}/convert", post(convert_image))
        .route(
            "/pools/{pool}/images/{name}/template",
            axum::routing::put(set_template),
        )
        .route("/pools/{pool}/images/{name}/clone", post(clone_image))
        .route("/pools/{pool}/images/{name}/flatten", post(flatten_image))
        .route("/pools/{pool}/images/{name}/commit", post(commit_image))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/jobs/{id}/log", get(job_log))
}

fn bad(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))
}

fn not_found(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{e:#}"))
}

async fn qemu_img(app: &App) -> Result<PathBuf, ApiError> {
    app.supervisor.qemu_img().await.ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "qemu-img is not installed".into(),
        )
    })
}

// ------------------------------------------------------------------ pools

async fn list_pools(State(app): State<Arc<App>>) -> ApiResult<Vec<PoolSummary>> {
    let q = app.supervisor.qemu_img().await;
    app.supervisor
        .pools
        .summaries(q.as_deref())
        .await
        .map(Json)
        .map_err(bad)
}

#[derive(Deserialize)]
struct NewPool {
    name: String,
    path: String,
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    readonly: bool,
}

async fn add_pool(
    State(app): State<Arc<App>>,
    Json(b): Json<NewPool>,
) -> Result<StatusCode, ApiError> {
    app.supervisor
        .pools
        .add(
            &b.name,
            std::path::Path::new(&b.path),
            b.quota_bytes,
            b.readonly,
        )
        .map(|_| StatusCode::CREATED)
        .map_err(bad)
}

#[derive(Deserialize)]
struct PoolSettings {
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    readonly: bool,
}

async fn update_pool(
    State(app): State<Arc<App>>,
    UrlPath(pool): UrlPath<String>,
    Json(b): Json<PoolSettings>,
) -> Result<StatusCode, ApiError> {
    app.supervisor
        .pools
        .update(&pool, b.quota_bytes, b.readonly)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(bad)
}

async fn remove_pool(
    State(app): State<Arc<App>>,
    UrlPath(pool): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    let (used, _) = app.supervisor.image_users();
    let users: Vec<String> = used
        .iter()
        .filter(|(r, _)| r.pool == pool)
        .flat_map(|(_, v)| v.clone())
        .collect();
    if !users.is_empty() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("pool '{pool}' has images used by: {}", users.join(", ")),
        ));
    }
    app.supervisor
        .pools
        .remove(&pool)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(bad)
}

async fn pool(State(app): State<Arc<App>>, UrlPath(pool): UrlPath<String>) -> ApiResult<PoolInfo> {
    let q = app.supervisor.qemu_img().await;
    let mut info = app
        .supervisor
        .pools
        .list(q.as_deref(), &pool)
        .await
        .map_err(not_found)?;
    let (used, _) = app.supervisor.image_users();
    for img in &mut info.images {
        img.used_by = used
            .get(&ImageRef {
                pool: img.pool.clone(),
                name: img.name.clone(),
            })
            .cloned()
            .unwrap_or_default();
    }
    Ok(Json(info))
}

// ----------------------------------------------------------------- images

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

/// Create an empty disk image, as a job.
async fn create_image(
    State(app): State<Arc<App>>,
    UrlPath(pool): UrlPath<String>,
    Json(b): Json<CreateImage>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let q = qemu_img(&app).await?;
    let r = ImageRef {
        pool: pool.clone(),
        name: b.name.clone(),
    };
    // Validate up front so obvious mistakes fail the request, not the job.
    app.supervisor.pools.new_image_path(&r).map_err(bad)?;
    let pools = app.supervisor.pools.clone();
    let title = format!(
        "Create {} ({}, {} bytes) in {}",
        b.name, b.format, b.size_bytes, pool
    );
    let job = app.supervisor.jobs.spawn(
        "image.create",
        &title,
        json!({"pool": pool, "name": b.name}),
        false,
        move |ctx| async move {
            ctx.progress(None, Some("Creating"));
            pools.create(&q, &r, b.size_bytes, &b.format).await?;
            Ok(format!("Created {}", r.name))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
) -> ApiResult<ImageDetail> {
    let q = qemu_img(&app).await?;
    let r = ImageRef { pool, name };
    let (info, chain) = app
        .supervisor
        .pools
        .detail(&q, &r)
        .await
        .map_err(not_found)?;
    let (used, open) = app.supervisor.image_users();
    let path = app.supervisor.pools.resolve(&r).map_err(not_found)?;
    let dependents = app
        .supervisor
        .pools
        .dependents(&q, &path)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|d| format!("{}/{}", d.pool, d.name))
        .collect();
    Ok(Json(ImageDetail {
        template: app.supervisor.pools.is_template(&r),
        dependents,
        format: info["format"].as_str().unwrap_or("raw").into(),
        virtual_size: info["virtual-size"].as_u64().unwrap_or(0),
        actual_size: info["actual-size"].as_u64().unwrap_or(0),
        format_specific: info["format-specific"]["data"].clone(),
        backing_chain: chain,
        snapshots: info["snapshots"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        used_by: used.get(&r).cloned().unwrap_or_default(),
        in_use_by: open.get(&r).cloned().unwrap_or_default(),
        pool: r.pool,
        name: r.name,
    }))
}

/// Delete an image file. Refused while any VM references it or another
/// image uses it as a backing file.
async fn delete_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let r = ImageRef { pool, name };
    let path = app.supervisor.pools.resolve(&r).map_err(not_found)?;
    if app
        .supervisor
        .pools
        .get(&r.pool)
        .map_err(not_found)?
        .readonly
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("pool '{}' is read-only", r.pool),
        ));
    }
    let (used, _) = app.supervisor.image_users();
    if let Some(vms) = used.get(&r) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("'{}' is used by: {}", r.name, vms.join(", ")),
        ));
    }
    let q = qemu_img(&app).await?;
    let deps = app
        .supervisor
        .pools
        .dependents(&q, &path)
        .await
        .map_err(bad)?;
    if !deps.is_empty() {
        let names: Vec<String> = deps
            .iter()
            .map(|d| format!("{}/{}", d.pool, d.name))
            .collect();
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("'{}' is the backing file of: {}", r.name, names.join(", ")),
        ));
    }
    std::fs::remove_file(&path).map_err(|e| bad(e.into()))?;
    let _ = app.supervisor.pools.set_template(&r, false);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ConvertBody {
    target_pool: String,
    target_name: String,
    #[serde(default = "qcow2")]
    format: String,
    /// qcow2 compression.
    #[serde(default)]
    compress: bool,
}

/// Copy or convert an image into a (possibly different) pool, as a job.
/// The reference job kind: qemu-img progress, cancellation, quota check.
async fn convert_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<ConvertBody>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    if !matches!(
        b.format.as_str(),
        "qcow2" | "raw" | "vmdk" | "vdi" | "vhdx" | "vpc"
    ) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("unsupported format '{}'", b.format),
        ));
    }
    let q = qemu_img(&app).await?;
    let src_ref = ImageRef { pool, name };
    let src = app.supervisor.pools.resolve(&src_ref).map_err(not_found)?;
    let dst_ref = ImageRef {
        pool: b.target_pool.clone(),
        name: b.target_name.clone(),
    };
    let dst = app.supervisor.pools.new_image_path(&dst_ref).map_err(bad)?;
    let (_, open) = app.supervisor.image_users();
    if let Some(vms) = open.get(&src_ref) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "'{}' is open in running VM(s) {}; stop them first for a consistent copy",
                src_ref.name,
                vms.join(", ")
            ),
        ));
    }
    let need = Pools::measure(&q, Some(&src), None, &b.format)
        .await
        .map(|m| m.required)
        .unwrap_or(0);
    app.supervisor
        .pools
        .check_quota(Some(&q), &dst_ref.pool, need)
        .await
        .map_err(bad)?;
    let title = format!(
        "Convert {}/{} to {}/{} ({})",
        src_ref.pool, src_ref.name, dst_ref.pool, dst_ref.name, b.format
    );
    let job = app.supervisor.jobs.spawn(
        "image.convert",
        &title,
        json!({"pool": dst_ref.pool, "name": dst_ref.name, "source": {"pool": src_ref.pool, "name": src_ref.name}}),
        true,
        move |ctx| async move {
            let (s, d) = (src.display().to_string(), dst.display().to_string());
            let mut args = vec!["convert", "-O", b.format.as_str()];
            if b.compress && b.format == "qcow2" {
                args.push("-c");
            }
            args.extend([s.as_str(), d.as_str()]);
            let res = ctx.run_qemu_img(&q, &args).await;
            if res.is_err() {
                // Never leave a half-written image behind.
                let _ = std::fs::remove_file(&dst);
            }
            res.map(|_| format!("Converted to {}", dst_ref.name))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn check_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
) -> ApiResult<CheckResult> {
    let q = qemu_img(&app).await?;
    app.supervisor
        .pools
        .check(&q, &ImageRef { pool, name })
        .await
        .map(Json)
        .map_err(bad)
}

#[derive(Deserialize)]
struct MeasureBody {
    #[serde(default = "qcow2")]
    format: String,
    #[serde(default)]
    size_bytes: Option<u64>,
}

async fn measure_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<MeasureBody>,
) -> ApiResult<MeasureResult> {
    let q = qemu_img(&app).await?;
    let path = app
        .supervisor
        .pools
        .resolve(&ImageRef { pool, name })
        .map_err(not_found)?;
    Pools::measure(&q, Some(&path), None, &b.format)
        .await
        .map(Json)
        .map_err(bad)
}

async fn measure_new(
    State(app): State<Arc<App>>,
    UrlPath(_pool): UrlPath<String>,
    Json(b): Json<MeasureBody>,
) -> ApiResult<MeasureResult> {
    let q = qemu_img(&app).await?;
    let size = b.size_bytes.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "size_bytes is required".into(),
        )
    })?;
    Pools::measure(&q, None, Some(size), &b.format)
        .await
        .map(Json)
        .map_err(bad)
}

// ------------------------------------------------------------------- jobs

#[derive(Deserialize)]
struct JobsQuery {
    #[serde(default)]
    limit: Option<u32>,
}

async fn list_jobs(State(app): State<Arc<App>>, Query(q): Query<JobsQuery>) -> ApiResult<Vec<Job>> {
    app.supervisor
        .jobs
        .list(q.limit.unwrap_or(100).min(500))
        .map(Json)
        .map_err(bad)
}

async fn get_job(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> ApiResult<Job> {
    app.supervisor
        .jobs
        .get(&id)
        .map_err(bad)?
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such job".into()))
}

async fn cancel_job(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    app.supervisor
        .jobs
        .cancel(&id)
        .map(|_| StatusCode::ACCEPTED)
        .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))
}

async fn job_log(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<String, ApiError> {
    let path = app
        .supervisor
        .jobs
        .log_path(&id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such job".into()))?;
    Ok(std::fs::read_to_string(path).unwrap_or_default())
}

// ------------------------------------------------- templates, clones (M2-05)

fn conflict(msg: String) -> ApiError {
    ApiError(StatusCode::CONFLICT, msg)
}

#[derive(Deserialize)]
struct TemplateBody {
    template: bool,
}

/// Mark or unmark a template. Unmarking is refused while linked clones
/// depend on it: they would be corrupted by the next write.
async fn set_template(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<TemplateBody>,
) -> Result<StatusCode, ApiError> {
    let r = ImageRef { pool, name };
    let path = app.supervisor.pools.resolve(&r).map_err(not_found)?;
    if b.template {
        let (_, open) = app.supervisor.image_users();
        if let Some(vms) = open.get(&r) {
            return Err(conflict(format!(
                "'{}' is open in running VM(s) {}; stop them first",
                r.name,
                vms.join(", ")
            )));
        }
    } else {
        let q = qemu_img(&app).await?;
        let deps = app
            .supervisor
            .pools
            .dependents(&q, &path)
            .await
            .map_err(bad)?;
        if !deps.is_empty() {
            let names: Vec<String> = deps
                .iter()
                .map(|d| format!("{}/{}", d.pool, d.name))
                .collect();
            return Err(conflict(format!(
                "'{}' is the base of linked clones ({}); it must stay a template while they exist",
                r.name,
                names.join(", ")
            )));
        }
    }
    app.supervisor
        .pools
        .set_template(&r, b.template)
        .map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CloneBody {
    target_pool: String,
    target_name: String,
}

/// Linked clone of a template image: an O(1) qcow2 overlay.
async fn clone_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
    Json(b): Json<CloneBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let base = ImageRef { pool, name };
    app.supervisor.pools.resolve(&base).map_err(not_found)?;
    if !app.supervisor.pools.is_template(&base) {
        return Err(conflict(format!(
            "linked clones are built on templates, which never change; mark '{}' as a template first, or copy it",
            base.name
        )));
    }
    let q = qemu_img(&app).await?;
    let target = ImageRef {
        pool: b.target_pool,
        name: b.target_name,
    };
    app.supervisor
        .pools
        .linked_clone(&q, &base, &target)
        .await
        .map_err(bad)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"pool": target.pool, "name": target.name})),
    ))
}

/// Copy the backing chain's data into the image so it stands alone. Offline
/// with `qemu-img rebase -b ""`; live (image open in a running VM) with the
/// QMP block-stream job on its format node.
async fn flatten_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let r = ImageRef { pool, name };
    let path = app.supervisor.pools.resolve(&r).map_err(not_found)?;
    if app
        .supervisor
        .pools
        .get(&r.pool)
        .map_err(not_found)?
        .readonly
    {
        return Err(conflict(format!("pool '{}' is read-only", r.pool)));
    }
    let q = qemu_img(&app).await?;
    let backing = app.supervisor.pools.backing_of(&q, &path).await;
    if backing.is_none() {
        return Err(conflict(format!(
            "'{}' has no backing file; nothing to flatten",
            r.name
        )));
    }
    let need = Pools::measure(&q, Some(&path), None, "qcow2")
        .await
        .map_or(0, |m| m.required);
    app.supervisor
        .pools
        .check_quota(Some(&q), &r.pool, need)
        .await
        .map_err(bad)?;
    let target = json!({"pool": r.pool, "name": r.name});
    let job = match app.supervisor.find_open_disk(&r) {
        Some((vm_id, disk)) => {
            let qmp = app
                .supervisor
                .live_qmp(&vm_id)
                .ok_or_else(|| conflict("the VM using this image is not reachable".into()))?;
            app.supervisor.jobs.spawn(
                "image.flatten",
                &format!("Flatten {}/{} (live)", r.pool, r.name),
                target,
                true,
                move |ctx| async move {
                    block_stream(&ctx, &qmp, &format!("qui-disk{disk}")).await?;
                    Ok("Flattened while running".into())
                },
            )
        }
        None => app.supervisor.jobs.spawn(
            "image.flatten",
            &format!("Flatten {}/{}", r.pool, r.name),
            target,
            true,
            move |ctx| async move {
                let p = path.display().to_string();
                // Safe rebase onto nothing: copies all backing data in.
                ctx.run_qemu_img(&q, &["rebase", "-f", "qcow2", "-b", "", &p])
                    .await?;
                Ok("Flattened".into())
            },
        ),
    };
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Run QMP block-stream on `node` and wait for it, reporting progress.
async fn block_stream(
    ctx: &crate::jobs::JobCtx,
    qmp: &qemu_ui_qmp::Qmp,
    node: &str,
) -> anyhow::Result<()> {
    let job_id = format!("qui-stream-{}", ctx.id);
    let mut events = qmp.events();
    qmp.execute(
        "block-stream",
        Some(json!({"job-id": job_id, "device": node})),
    )
    .await?;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(300));
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Ok(ev) = ev else { continue };
                if ev.data["device"] != job_id.as_str() {
                    continue;
                }
                match ev.event.as_str() {
                    "BLOCK_JOB_COMPLETED" => {
                        if let Some(err) = ev.data["error"].as_str() {
                            anyhow::bail!("block-stream failed: {err}");
                        }
                        return Ok(());
                    }
                    "BLOCK_JOB_CANCELLED" => anyhow::bail!("cancelled"),
                    "BLOCK_JOB_ERROR" => anyhow::bail!("block-stream I/O error: {}", ev.data),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if let Ok(jobs) = qmp.execute("query-block-jobs", None).await
                    && let Some(j) = jobs.as_array().and_then(|a| a.iter().find(|j| j["device"] == job_id.as_str()))
                {
                    let (off, len) = (j["offset"].as_f64().unwrap_or(0.0), j["len"].as_f64().unwrap_or(0.0));
                    if len > 0.0 {
                        ctx.progress(Some(off / len), Some("Streaming backing data"));
                    }
                }
            }
            _ = ctx.cancelled() => {
                let _ = qmp.execute("block-job-cancel", Some(json!({"device": job_id}))).await;
                anyhow::bail!("cancelled");
            }
        }
    }
}

/// Write an overlay's changes into its base (`qemu-img commit`). Only when
/// nothing else could be hurt: the overlay is not open, and the base is not
/// a template, not used by any VM directly and has no other overlays.
async fn commit_image(
    State(app): State<Arc<App>>,
    UrlPath((pool, name)): UrlPath<(String, String)>,
) -> Result<(StatusCode, Json<Job>), ApiError> {
    let r = ImageRef { pool, name };
    let path = app.supervisor.pools.resolve(&r).map_err(not_found)?;
    let q = qemu_img(&app).await?;
    let (used, open) = app.supervisor.image_users();
    if let Some(vms) = open.get(&r) {
        return Err(conflict(format!(
            "'{}' is open in running VM(s) {}; stop them first",
            r.name,
            vms.join(", ")
        )));
    }
    let base_path = app
        .supervisor
        .pools
        .backing_of(&q, &path)
        .await
        .ok_or_else(|| conflict(format!("'{}' has no backing file to commit into", r.name)))?;
    let base = app.supervisor.pools.locate(&base_path).ok_or_else(|| {
        conflict("the backing file is outside every pool; refusing to modify it".into())
    })?;
    if app.supervisor.pools.is_template(&base) {
        return Err(conflict(format!(
            "'{}' is a template; templates never change",
            base.name
        )));
    }
    if app
        .supervisor
        .pools
        .get(&base.pool)
        .map_err(not_found)?
        .readonly
    {
        return Err(conflict(format!("pool '{}' is read-only", base.pool)));
    }
    if let Some(vms) = used.get(&base) {
        return Err(conflict(format!(
            "'{}' is attached to VM(s) {}; committing would change their disk",
            base.name,
            vms.join(", ")
        )));
    }
    let deps = app
        .supervisor
        .pools
        .dependents(&q, &base_path)
        .await
        .map_err(bad)?;
    if deps.len() > 1 {
        let others: Vec<String> = deps
            .iter()
            .filter(|d| **d != r)
            .map(|d| format!("{}/{}", d.pool, d.name))
            .collect();
        return Err(conflict(format!(
            "'{}' is also the base of {}; committing would corrupt them",
            base.name,
            others.join(", ")
        )));
    }
    let job = app.supervisor.jobs.spawn(
        "image.commit",
        &format!(
            "Commit {}/{} into {}/{}",
            r.pool, r.name, base.pool, base.name
        ),
        json!({"pool": base.pool, "name": base.name, "source": {"pool": r.pool, "name": r.name}}),
        false,
        move |ctx| async move {
            let p = path.display().to_string();
            ctx.run_qemu_img(&q, &["commit", &p]).await?;
            Ok(format!("Committed into {}", base.name))
        },
    );
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Storage API end to end through the real router and real qemu-img.
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
        let id = job["id"].as_str().unwrap();
        for _ in 0..200 {
            let (_, j) = call(router, "GET", &format!("/api/v1/jobs/{id}"), None).await;
            if !matches!(j["status"].as_str(), Some("queued" | "running")) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {id} did not finish");
    }

    #[test]
    fn pools_images_and_jobs() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("stor");
        let extra = data.join("extra-pool");
        std::fs::create_dir_all(&extra).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);

            // Pools
            let (s, _) = call(
                &r,
                "POST",
                "/api/v1/pools",
                Some(json!({"name": "extra", "path": extra.display().to_string()})),
            )
            .await;
            assert_eq!(s, 201);
            let (s, _) = call(
                &r,
                "POST",
                "/api/v1/pools",
                Some(json!({"name": "again", "path": extra.display().to_string()})),
            )
            .await;
            assert_eq!(s, 422, "overlapping pool refused");

            // Create a disk via a job.
            let (s, job) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images",
                Some(json!({"name": "base.qcow2", "size_bytes": 64 << 20})),
            )
            .await;
            assert_eq!(s, 202);
            assert_eq!(job["kind"], "image.create");
            assert_eq!(finish(&r, &job).await["status"], "succeeded");
            let (s, _) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images",
                Some(json!({"name": "../escape.qcow2", "size_bytes": 1 << 20})),
            )
            .await;
            assert_eq!(s, 422, "path traversal refused");

            // Convert into the other pool via a job with qemu-img progress.
            let (s, job) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/base.qcow2/convert",
                Some(json!({"target_pool": "extra", "target_name": "copy.raw", "format": "raw"})),
            )
            .await;
            assert_eq!(s, 202);
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "succeeded", "{fin}");
            let (_, d) = call(&r, "GET", "/api/v1/pools/extra/images/copy.raw", None).await;
            assert_eq!(
                (d["format"].as_str(), d["virtual_size"].as_u64()),
                (Some("raw"), Some(64 << 20))
            );

            // Backing chain: an overlay on base.qcow2.
            let base = sv
                .pools
                .resolve(&qemu_ui_core::spec::ImageRef {
                    pool: "default".into(),
                    name: "base.qcow2".into(),
                })
                .unwrap();
            let overlay = sv.pools.pool_dir("default").unwrap().join("overlay.qcow2");
            let ok = crate::probe::command(&img)
                .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
                .arg(&base)
                .arg(&overlay)
                .status()
                .await
                .unwrap();
            assert!(ok.success());
            let (_, d) = call(
                &r,
                "GET",
                "/api/v1/pools/default/images/overlay.qcow2",
                None,
            )
            .await;
            assert_eq!(d["backing_chain"][0]["name"], "base.qcow2", "{d}");
            assert_eq!(d["backing_chain"][0]["pool"], "default");

            // Deleting a backing file is refused; the overlay can go first.
            let (s, e) = call(
                &r,
                "DELETE",
                "/api/v1/pools/default/images/base.qcow2",
                None,
            )
            .await;
            assert_eq!(s, 409, "{e}");
            assert!(e["error"].as_str().unwrap().contains("overlay.qcow2"));

            // Check and measure.
            let (s, c) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/base.qcow2/check",
                None,
            )
            .await;
            assert_eq!((s, c["ok"].as_bool()), (200, Some(true)), "{c}");
            let (s, m) = call(
                &r,
                "POST",
                "/api/v1/pools/default/measure",
                Some(json!({"size_bytes": 1u64 << 30, "format": "raw"})),
            )
            .await;
            assert_eq!((s, m["fully_allocated"].as_u64()), (200, Some(1 << 30)));

            // Quota: a tiny quota refuses a conversion into the pool.
            let (s, _) = call(
                &r,
                "PUT",
                "/api/v1/pools/extra",
                Some(json!({"quota_bytes": 1024})),
            )
            .await;
            assert_eq!(s, 204);
            let (s, e) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/base.qcow2/convert",
                Some(json!({"target_pool": "extra", "target_name": "big.raw", "format": "raw"})),
            )
            .await;
            assert_eq!(s, 422, "{e}");
            assert!(e["error"].as_str().unwrap().contains("quota"));

            // Pool list carries usage; jobs are listed newest first.
            let (_, pools) = call(&r, "GET", "/api/v1/pools", None).await;
            let extra_p = pools
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == "extra")
                .unwrap()
                .clone();
            assert_eq!(extra_p["image_count"], 1);
            let (_, jobs) = call(&r, "GET", "/api/v1/jobs", None).await;
            assert_eq!(jobs.as_array().unwrap()[0]["kind"], "image.convert");

            // Delete works for an unreferenced leaf.
            let (s, _) = call(
                &r,
                "DELETE",
                "/api/v1/pools/default/images/overlay.qcow2",
                None,
            )
            .await;
            assert_eq!(s, 204);
            let (s, _) = call(&r, "DELETE", "/api/v1/pools/default", None).await;
            assert_eq!(s, 422, "default pool is permanent");
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// Templates, linked/full clones, flatten and commit against real QEMU
/// (M2-05). Skipped without QEMU.
#[cfg(test)]
mod clones {
    use crate::supervisor::test_support::*;
    use crate::supervisor::{CloneMode, CloneOutcome, OpError};
    use axum::body::Body;
    use axum::http::Request;
    use qemu_ui_core::spec::ImageRef;
    use qemu_ui_core::vm::VmState;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::path::Path;
    use std::time::Duration;
    use tower::ServiceExt;

    const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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

    fn sha(path: &Path) -> String {
        hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
    }

    async fn backing(img: &Path, path: &Path) -> Option<String> {
        let out = crate::probe::command(img)
            .args(["info", "-U", "--output=json"])
            .arg(path)
            .output()
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["backing-filename"].as_str().map(String::from)
    }

    async fn qemu_io_write(io: &Path, path: &Path) {
        let ok = crate::probe::command(io)
            .args(["-f", "qcow2", "-c", "write -P 0xab 0 1M"])
            .arg(path)
            .status()
            .await
            .unwrap();
        assert!(ok.success(), "qemu-io write");
    }

    async fn overlay(img: &Path, base: &Path, path: &Path) {
        let ok = crate::probe::command(img)
            .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(base)
            .arg(path)
            .status()
            .await
            .unwrap();
        assert!(ok.success());
    }

    #[test]
    fn templates_clones_flatten_commit() {
        let (Some(qemu), Some(img), Some(io)) = (
            find("qemu-system-x86_64"),
            find("qemu-img"),
            find("qemu-io"),
        ) else {
            eprintln!("skipping: qemu, qemu-img or qemu-io not found");
            return;
        };
        let data = short_data_dir("clone");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            let sv = make(&data, &caps).await;
            let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
            let r = crate::api::router(app, None);
            let dref = |n: &str| ImageRef {
                pool: "default".into(),
                name: n.into(),
            };

            let base_id = small_vm(&sv, &caps, "base").await;
            let base_path = sv.pools.resolve(&dref("base.qcow2")).unwrap();
            qemu_io_write(&io, &base_path).await; // some content in the base

            // Linked clones need template disks.
            match sv
                .clone_vm(&base_id, "c1".into(), CloneMode::Linked, "default".into())
                .await
            {
                Err(OpError::Invalid(d)) => assert_eq!(d[0].rule, "clone.linked-needs-template"),
                other => panic!("expected refusal, got {:?}", other.is_ok()),
            }

            // Make the base disk a template; the base VM may no longer start.
            let (s, _) = call(
                &r,
                "PUT",
                "/api/v1/pools/default/images/base.qcow2/template",
                Some(json!({"template": true})),
            )
            .await;
            assert_eq!(s, 204);
            match sv.start(&base_id).await {
                Err(OpError::Invalid(d)) => assert_eq!(d[0].rule, "disk.template-writable"),
                other => panic!("template started writable: {:?}", other.is_ok()),
            }
            let before = sha(&base_path);

            // Linked clone VM: instant, overlay on the template, boots.
            let c1 = match sv
                .clone_vm(&base_id, "c1".into(), CloneMode::Linked, "default".into())
                .await
            {
                Ok(CloneOutcome::Vm(vm)) => vm,
                _ => panic!("linked clone failed"),
            };
            let c1_disk = c1.spec.disks[0].image.clone();
            assert_ne!(c1_disk, dref("base.qcow2"));
            let c1_path = sv.pools.resolve(&c1_disk).unwrap();
            assert!(
                backing(&img, &c1_path)
                    .await
                    .unwrap()
                    .ends_with("base.qcow2")
            );
            let base_mac = sv.detail(&base_id).ok().unwrap().spec.nics[0].mac.clone();
            assert_ne!(c1.spec.nics[0].mac, base_mac, "new MAC");
            let (_, d) = call(
                &r,
                "GET",
                &format!("/api/v1/pools/default/images/{}", c1_disk.name),
                None,
            )
            .await;
            assert_eq!(d["backing_chain"][0]["name"], "base.qcow2");
            let (_, d) = call(&r, "GET", "/api/v1/pools/default/images/base.qcow2", None).await;
            assert_eq!(
                (d["template"].as_bool(), d["dependents"][0].as_str()),
                (
                    Some(true),
                    Some(format!("default/{}", c1_disk.name).as_str())
                )
            );

            // Unmarking or deleting a template with clones is refused.
            let (s, _) = call(
                &r,
                "PUT",
                "/api/v1/pools/default/images/base.qcow2/template",
                Some(json!({"template": false})),
            )
            .await;
            assert_eq!(s, 409);

            // Writes to the clone never reach the template.
            qemu_io_write(&io, &c1_path).await;
            assert_eq!(sha(&base_path), before, "template unchanged");

            // The clone boots to running, then live flatten via block-stream.
            sv.start(&c1.summary.id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let (s, job) = call(
                &r,
                "POST",
                &format!("/api/v1/pools/default/images/{}/flatten", c1_disk.name),
                None,
            )
            .await;
            assert_eq!(s, 202, "{job}");
            assert!(job["title"].as_str().unwrap().contains("live"));
            let fin = finish(&r, &job).await;
            assert_eq!(fin["status"], "succeeded", "{fin}");
            sv.force_stop(&c1.summary.id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &c1.summary.id, VmState::Stopped).await;
            assert_eq!(
                backing(&img, &c1_path).await,
                None,
                "live flatten removed the backing file"
            );

            // Full clone: independent copy, made by a job.
            let job = match sv
                .clone_vm(&base_id, "c2".into(), CloneMode::Full, "default".into())
                .await
            {
                Ok(CloneOutcome::Job(j)) => j,
                _ => panic!("full clone did not start a job"),
            };
            let fin = sv
                .jobs
                .wait(&job.id, Duration::from_secs(60))
                .await
                .unwrap();
            assert_eq!(
                fin.status,
                qemu_ui_core::storage::JobStatus::Succeeded,
                "{:?}",
                fin.error
            );
            let vms = sv.list().unwrap();
            let c2 = vms.iter().find(|v| v.name == "c2").expect("c2 created");
            let c2_spec = sv.detail(&c2.id).ok().unwrap().spec;
            let c2_path = sv.pools.resolve(&c2_spec.disks[0].image).unwrap();
            assert_eq!(
                backing(&img, &c2_path).await,
                None,
                "full clone stands alone"
            );

            // Offline flatten of an image-level linked clone.
            let (s, _) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/base.qcow2/clone",
                Some(json!({"target_pool": "default", "target_name": "off.qcow2"})),
            )
            .await;
            assert_eq!(s, 201);
            let off = sv.pools.resolve(&dref("off.qcow2")).unwrap();
            assert!(backing(&img, &off).await.is_some());
            let (_, job) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/off.qcow2/flatten",
                None,
            )
            .await;
            assert_eq!(finish(&r, &job).await["status"], "succeeded");
            assert_eq!(backing(&img, &off).await, None, "offline flatten");

            // Commit: refused into a template and into a shared base.
            let o2 = sv.pools.pool_dir("default").unwrap().join("o2.qcow2");
            overlay(&img, &base_path, &o2).await;
            let (s, e) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/o2.qcow2/commit",
                None,
            )
            .await;
            assert_eq!(s, 409, "{e}");
            assert!(e["error"].as_str().unwrap().contains("template"));
            sv.pools
                .create(&img, &dref("b2.qcow2"), 32 << 20, "qcow2")
                .await
                .unwrap();
            let b2 = sv.pools.resolve(&dref("b2.qcow2")).unwrap();
            let (x, y) = (b2.with_file_name("x.qcow2"), b2.with_file_name("y.qcow2"));
            overlay(&img, &b2, &x).await;
            overlay(&img, &b2, &y).await;
            qemu_io_write(&io, &x).await;
            let (s, e) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/x.qcow2/commit",
                None,
            )
            .await;
            assert_eq!(s, 409, "{e}");
            assert!(e["error"].as_str().unwrap().contains("y.qcow2"));
            std::fs::remove_file(&y).unwrap();
            let b2_before = sha(&b2);
            let (s, job) = call(
                &r,
                "POST",
                "/api/v1/pools/default/images/x.qcow2/commit",
                None,
            )
            .await;
            assert_eq!(s, 202, "{job}");
            assert_eq!(finish(&r, &job).await["status"], "succeeded");
            assert_ne!(sha(&b2), b2_before, "base received the overlay's data");
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
