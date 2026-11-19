//! Import and export (M4-07): `POST /vms/import` turns a QEMU command line
//! or a libvirt domain XML into a spec to review (nothing is created), and
//! `GET /vms/{id}/export/libvirt` renders a VM as libvirt XML.
//!
//! The importers in core are pure; this layer adds what needs the host:
//! pinning the machine to this QEMU, choosing a firmware descriptor for
//! UEFI, and mapping referenced files that already sit in a pool.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use qemu_ui_core::caps::HostCaps;
use qemu_ui_core::compile::pin_machine;
use qemu_ui_core::export_libvirt::{ExportContext, ExportUefi, export_libvirt};
use qemu_ui_core::import::{FileUse, FirmwareHint, ImportFormat, ImportResult};
use qemu_ui_core::spec::FirmwareSpec;
use serde::Deserialize;

use crate::api::{ApiError, App};
use crate::images::Pools;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/vms/import", post(import))
        .route("/vms/{id}/export/libvirt", get(export))
}

/// Imports larger than this are certainly not a command line or a domain.
const MAX_IMPORT: usize = 1 << 20;

#[derive(Deserialize)]
struct ImportBody {
    format: ImportFormat,
    text: String,
}

async fn import(
    State(app): State<Arc<App>>,
    Json(b): Json<ImportBody>,
) -> Result<Json<ImportResult>, ApiError> {
    if b.text.len() > MAX_IMPORT {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the text to import is larger than 1 MiB".into(),
        ));
    }
    let mut r = match b.format {
        ImportFormat::Argv => qemu_ui_core::import_argv::import_argv(&b.text),
        ImportFormat::Libvirt => qemu_ui_core::import_libvirt::import_libvirt(&b.text),
    }
    .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let caps = app.scanner.caps.read().await.clone();
    adapt(&mut r, caps.as_ref(), &app.supervisor.pools);
    Ok(Json(r))
}

/// Fit an imported spec to this host: machine, firmware, pool files.
pub fn adapt(r: &mut ImportResult, caps: Option<&HostCaps>, pools: &Pools) {
    let spec = &mut r.spec;
    let bin = caps.and_then(|c| c.binaries.iter().find(|b| b.arch == spec.arch));
    match bin {
        None => r.warnings.push(format!(
            "no qemu-system-{} is installed here; install it before creating this VM",
            spec.arch
        )),
        Some(bin) => {
            let wanted = spec.machine.type_name.clone();
            let family = machine_family(&wanted);
            let pinned = if let Ok(m) = pin_machine(&wanted, bin) {
                Ok(m)
            } else if let Ok(m) = pin_machine(family, bin) {
                r.warnings.push(format!(
                    "machine '{wanted}' is not available here; using the same family, '{m}'"
                ));
                Ok(m)
            } else if let Some(m) = bin.default_machine() {
                r.warnings.push(format!(
                    "machine '{wanted}' is not available here; using the default, '{}' (check the hardware)",
                    m.name
                ));
                Ok(m.name.clone())
            } else {
                Err(format!(
                    "machine '{wanted}' is not available in qemu-system-{}",
                    spec.arch
                ))
            };
            match pinned {
                Ok(m) => spec.machine.type_name = m,
                Err(e) => r.warnings.push(e),
            }
        }
    }
    if let Some(hint) = r.firmware_hint {
        let secure = hint == FirmwareHint::UefiSecureBoot;
        let pick = caps.and_then(|c| {
            let mut fws: Vec<_> = c
                .firmware
                .iter()
                .filter(|f| f.complete && f.interface_types.iter().any(|t| t == "uefi"))
                .filter(|f| f.supports(&spec.arch, &spec.machine.type_name))
                .collect();
            // Prefer the requested Secure Boot state, then descriptor order.
            fws.sort_by_key(|f| (f.secure_boot() != secure, f.id.clone()));
            fws.first().map(|f| (f.id.clone(), f.secure_boot()))
        });
        match pick {
            Some((id, sb)) => {
                if sb != secure {
                    r.warnings.push(format!(
                        "no {} UEFI firmware for this machine; using '{id}'",
                        if secure {
                            "Secure Boot"
                        } else {
                            "non-Secure Boot"
                        }
                    ));
                }
                spec.firmware = FirmwareSpec::Descriptor { id };
            }
            None => r.warnings.push(
                "the source boots with UEFI, but no matching UEFI firmware is installed here; \
                 it uses the machine's default firmware"
                    .into(),
            ),
        }
    }
    for f in &mut r.files_to_map {
        if f.usage != FileUse::Image {
            continue;
        }
        if let Some(found) = pools.locate(Path::new(&f.path)) {
            spec.disks[f.disk].image = found.clone();
            f.mapped = Some(found);
        }
    }
}

/// `pc-q35-8.2` → `q35`, `pc-i440fx-6.2`/`pc-1.0` → `pc`, `virt-9.0` → `virt`.
fn machine_family(m: &str) -> &str {
    if m.starts_with("pc-q35-") {
        "q35"
    } else if m.starts_with("pc-i440fx-") || m.starts_with("pc-") {
        "pc"
    } else if let Some((base, ver)) = m.rsplit_once('-')
        && ver.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !ver.is_empty()
    {
        base
    } else {
        m
    }
}

async fn export(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let sv = &app.supervisor;
    let detail = sv
        .detail(&id)
        .map_err(|_| ApiError(StatusCode::NOT_FOUND, "no such VM".into()))?;
    let spec = detail.spec;
    let qemu_img = sv.qemu_img().await;
    let mut ctx = ExportContext::default();
    for d in &spec.disks {
        let resolved = match sv.pools.resolve(&d.image) {
            Ok(p) if p.is_file() => {
                let format = match &qemu_img {
                    Some(q) => sv
                        .pools
                        .info(q, &p)
                        .await
                        .map(|(f, _, _)| f)
                        .unwrap_or_else(|_| "raw".into()),
                    None => "raw".into(),
                };
                Some((p.display().to_string(), format))
            }
            _ => None,
        };
        ctx.disks.push(resolved);
    }
    if let FirmwareSpec::Descriptor { id: fw } = &spec.firmware {
        let caps = app.scanner.caps.read().await;
        let secure_boot = caps
            .as_ref()
            .and_then(|c| c.firmware.iter().find(|f| &f.id == fw))
            .is_some_and(|f| f.secure_boot());
        ctx.uefi = Some(ExportUefi { secure_boot });
    }
    ctx.networks = sv
        .networks
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.id, n.name))
        .collect();
    let e = export_libvirt(&spec, &ctx);
    let file = format!("attachment; filename=\"{}.xml\"", spec.name);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/xml; charset=utf-8".to_string(),
            ),
            (header::CONTENT_DISPOSITION, file),
        ],
        e.xml,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn maps_files_already_in_a_pool() {
        let data = std::env::temp_dir().join(format!(
            "qui-import-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&data).unwrap();
        let pools = Pools::new(&data, Store::memory().unwrap()).unwrap();
        let dir = pools.pool_dir("default").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("in-pool.qcow2");
        std::fs::write(&disk, b"").unwrap();
        let line = format!(
            "qemu-system-x86_64 -m 1G -drive file='{}',if=virtio -cdrom /elsewhere/x.iso",
            disk.display()
        );
        let mut r = qemu_ui_core::import_argv::import_argv(&line).unwrap();
        adapt(&mut r, None, &pools);
        assert_eq!(
            r.files_to_map[0].mapped.as_ref().map(|m| m.name.as_str()),
            Some("in-pool.qcow2")
        );
        assert_eq!(r.spec.disks[0].image.pool, "default");
        assert!(r.files_to_map[1].mapped.is_none(), "not in any pool");
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("no qemu-system-x86_64"))
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn machine_families() {
        assert_eq!(machine_family("pc-q35-8.2"), "q35");
        assert_eq!(machine_family("pc-i440fx-6.2"), "pc");
        assert_eq!(machine_family("virt-9.0"), "virt");
        assert_eq!(machine_family("pseries-8.0"), "pseries");
        assert_eq!(machine_family("q35"), "q35");
        assert_eq!(machine_family("s390-ccw-virtio"), "s390-ccw-virtio");
    }
}
