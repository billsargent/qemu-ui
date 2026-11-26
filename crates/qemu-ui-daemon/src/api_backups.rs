//! Backup/export bundles API (X-03).
//!
//! - `GET    /backups`                 bundles in the daemon's backups directory
//! - `POST   /vms/{id}/export`         `{snapshots?}` → 202 `{job}`
//! - `POST   /backups/import`          `{bundle, name, pool?}` → 202 `{job}`
//! - `GET    /backups/{name}`          download a bundle
//! - `PUT    /backups/{name}`          upload a bundle (raw body; never overwrites)
//! - `DELETE /backups/{name}`

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use qemu_ui_core::backup::BackupInfo;
use qemu_ui_core::storage::Job;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::api::{ApiError, App};
use crate::api_vms::{OpErr, op_err};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/backups", get(list))
        .route("/backups/import", post(import))
        .route("/backups/{name}", get(download).put(upload).delete(remove))
        .route("/vms/{id}/export", post(export))
}

fn internal(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

async fn list(State(app): State<Arc<App>>) -> Result<Json<Vec<BackupInfo>>, ApiError> {
    app.supervisor
        .list_backups()
        .await
        .map(Json)
        .map_err(internal)
}

#[derive(Deserialize, Default)]
struct ExportBody {
    #[serde(default)]
    snapshots: bool,
}

async fn export(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<ExportBody>>,
) -> Result<(StatusCode, Json<Value>), OpErr> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let job: Job = app
        .supervisor
        .export_vm(&id, b.snapshots)
        .await
        .map_err(op_err)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": job}))))
}

#[derive(Deserialize)]
struct ImportBody {
    bundle: String,
    name: String,
    #[serde(default = "default_pool")]
    pool: String,
}

fn default_pool() -> String {
    crate::images::DEFAULT_POOL.into()
}

async fn import(
    State(app): State<Arc<App>>,
    Json(b): Json<ImportBody>,
) -> Result<(StatusCode, Json<Value>), OpErr> {
    let job = app
        .supervisor
        .import_bundle(&b.bundle, b.name, b.pool)
        .await
        .map_err(op_err)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job": job}))))
}

async fn remove(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
) -> Result<StatusCode, OpErr> {
    app.supervisor
        .delete_backup(&name)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(op_err)
}

async fn download(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
) -> Result<Response, OpErr> {
    let path = app.supervisor.backup_path(&name).map_err(op_err)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| op_err(crate::supervisor::OpError::NotFound))?;
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
            (header::CONTENT_TYPE, "application/x-tar".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        body,
    )
        .into_response();
    // The name is a validated bundle name: no quotes or separators.
    if let Ok(v) = format!("attachment; filename=\"{name}\"").parse() {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// Store an uploaded bundle. The body is streamed to a `.part` file and
/// checked to be a readable bundle before it appears under its name.
async fn upload(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
    body: Body,
) -> Result<(StatusCode, Json<BackupInfo>), OpErr> {
    let path = app.supervisor.backup_path(&name).map_err(op_err)?;
    if path.exists() {
        return Err(op_err(crate::supervisor::OpError::Conflict(format!(
            "a bundle named '{name}' already exists"
        ))));
    }
    let dir = app.supervisor.backups_dir();
    let fail = |e: anyhow::Error| op_err(crate::supervisor::OpError::Failed(e));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| fail(e.into()))?;
    let part = dir.join(format!(".upload-{:08x}.part", rand::random::<u32>()));
    let res: anyhow::Result<u64> = async {
        let mut out = tokio::fs::File::create(&part).await?;
        let mut stream = body.into_data_stream();
        let mut n = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow::anyhow!("reading the upload: {e}"))?;
            n += chunk.len() as u64;
            out.write_all(&chunk).await?;
        }
        out.sync_all().await?;
        Ok(n)
    }
    .await;
    let size = match res {
        Ok(n) => n,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(fail(e));
        }
    };
    let p = part.clone();
    let manifest = tokio::task::spawn_blocking(move || crate::supervisor::read_manifest(&p))
        .await
        .map_err(|e| fail(e.into()))?;
    let manifest = match manifest {
        Ok(m) => m,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(op_err(crate::supervisor::OpError::Invalid(vec![
                qemu_ui_core::compile::Diagnostic {
                    severity: qemu_ui_core::compile::Severity::Error,
                    field: "bundle".into(),
                    rule: "backup.bundle".into(),
                    message: format!("{e:#}"),
                },
            ])));
        }
    };
    if path.exists() {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(op_err(crate::supervisor::OpError::Conflict(format!(
            "a bundle named '{name}' already exists"
        ))));
    }
    tokio::fs::rename(&part, &path)
        .await
        .map_err(|e| fail(e.into()))?;
    Ok((
        StatusCode::CREATED,
        Json(BackupInfo {
            name,
            size,
            modified_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            manifest: Some(manifest),
            error: None,
        }),
    ))
}
