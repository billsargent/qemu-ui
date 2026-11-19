//! Native SPICE client handoff (plan.md §8.1 `spice:native`, M5-05).
//!
//! A VM with `display.spice` gets a SPICE server on a loopback port. The
//! supervisor picks the port and a per-run password at start, records them
//! here, and sets the password over QMP before the guest runs. The
//! `console.vv` endpoint hands remote-viewer a connection file. The password
//! never appears in logs or in the command line.

use std::sync::Arc;

use anyhow::{Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, App};
use crate::store::Store;

const KEY: &str = "spice:";

/// Where a running VM's SPICE server listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpiceInfo {
    pub port: u16,
    pub password: String,
}

pub fn record(store: &Store, vm: &str, info: &SpiceInfo) {
    if let Ok(s) = serde_json::to_string(info) {
        let _ = store.put_kv(&format!("{KEY}{vm}"), &s);
    }
}

pub fn lookup(store: &Store, vm: &str) -> Option<SpiceInfo> {
    store
        .get_kv(&format!("{KEY}{vm}"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn clear(store: &Store, vm: &str) {
    let _ = store.put_kv(&format!("{KEY}{vm}"), "null");
}

/// A free loopback port for SPICE, in 6900 … 7399 (VNC uses 6000 … 6899).
pub fn free_port() -> Result<u16> {
    let start = rand::random::<u16>() % 500;
    for i in 0..500u16 {
        let port = 6900 + (start + i) % 500;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free SPICE port in 6900–7399")
}

/// SPICE passwords: random, 16 characters, unambiguous alphabet.
pub fn password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..16)
        .map(|_| ALPHABET[rand::random::<u32>() as usize % ALPHABET.len()] as char)
        .collect()
}

/// The remote-viewer connection file.
pub fn vv_file(title: &str, info: &SpiceInfo) -> String {
    // Titles are user-chosen VM names: keep them on one line.
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    format!(
        "[virt-viewer]\n\
         type=spice\n\
         host=127.0.0.1\n\
         port={}\n\
         password={}\n\
         title={title} - qemu-ui\n\
         delete-this-file=1\n\
         fullscreen=0\n\
         release-cursor=shift+f12\n",
        info.port, info.password
    )
}

// --------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/vms/{id}/console.vv", get(console_vv))
}

/// Only served to this machine (the API guard already requires a loopback
/// Host); the SPICE server itself listens on loopback only.
async fn console_vv(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Response {
    let Ok(d) = app.supervisor.detail(&id) else {
        return ApiError(StatusCode::NOT_FOUND, "no such VM".into()).into_response();
    };
    let spice = app
        .supervisor
        .live_qmp(&id)
        .and_then(|_| lookup(&app.supervisor.store, &id));
    let Some(info) = spice else {
        return ApiError(
            StatusCode::CONFLICT,
            "SPICE is available while the VM runs with SPICE enabled (Settings → Display)".into(),
        )
        .into_response();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                "application/x-virt-viewer".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"console.vv\"".to_string(),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Body::from(vv_file(&d.summary.name, &info)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vv_file_has_what_remote_viewer_needs() {
        let info = SpiceInfo {
            port: 6901,
            password: "s3cret".into(),
        };
        let vv = vv_file("web\nserver", &info);
        assert!(vv.starts_with("[virt-viewer]\n"));
        for line in [
            "type=spice",
            "host=127.0.0.1",
            "port=6901",
            "password=s3cret",
            "delete-this-file=1",
            "fullscreen=0",
        ] {
            assert!(vv.lines().any(|l| l == line), "{line} in {vv}");
        }
        assert!(
            vv.lines().any(|l| l == "title=webserver - qemu-ui"),
            "no newline injection: {vv}"
        );
    }

    #[test]
    fn records_roundtrip() {
        let store = Store::memory().unwrap();
        assert!(lookup(&store, "v").is_none());
        let info = SpiceInfo {
            port: 7000,
            password: password(),
        };
        assert_eq!(info.password.len(), 16);
        record(&store, "v", &info);
        assert_eq!(lookup(&store, "v"), Some(info));
        clear(&store, "v");
        assert!(lookup(&store, "v").is_none());
    }
}
