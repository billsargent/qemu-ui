//! Shared folders (plan.md §9.6, M2-13): share roots and their API.
//!
//! A VM spec never holds a host path for a share. It names a registered
//! *share root* plus a folder relative to it; the daemon canonicalises the
//! result and confines it to the root, exactly like images and pools.
//! Share roots may not overlap storage pools: a guest writing through a share
//! into a pool could rewrite other VMs' disks.
//!
//! Backends: virtio-9p is built into Linux QEMU builds. virtiofs is used for
//! writable shares when a `virtiofsd` binary is installed and the QEMU binary
//! supports `vhost-user-fs-pci`; the supervisor runs one `virtiofsd` sidecar
//! per such share (see `Supervisor::start`). QEMU on Windows hosts has no
//! folder sharing at all, so the feature is refused there with that reason.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use qemu_ui_core::spec::{VmSpec, valid_share_path};
use qemu_ui_core::storage::{ShareRoot, SharesInfo, valid_pool_name};
use serde::Deserialize;

use crate::api::{ApiError, App};
use crate::scan::strip_verbatim;
use crate::store::Store;

/// Why folder sharing is unavailable on this host, if it is.
pub fn host_unsupported_reason() -> Option<&'static str> {
    cfg!(windows).then_some(
        "QEMU on Windows hosts has no host folder sharing (no 9p or virtiofs support). \
         Use an SMB share or SFTP inside the guest instead.",
    )
}

pub struct ShareRoots {
    store: Store,
}

fn canon(p: &Path) -> Result<PathBuf> {
    let c = std::fs::canonicalize(p).with_context(|| format!("{} does not exist", p.display()))?;
    Ok(PathBuf::from(strip_verbatim(&c.display().to_string())))
}

impl ShareRoots {
    pub fn new(store: Store) -> Self {
        ShareRoots { store }
    }

    pub fn list(&self) -> Result<Vec<ShareRoot>> {
        Ok(self
            .store
            .list_share_roots()?
            .into_iter()
            .map(|(name, path)| ShareRoot { name, path })
            .collect())
    }

    /// Register a folder as a share root.
    pub fn add(&self, name: &str, path: &Path) -> Result<ShareRoot> {
        if let Some(why) = host_unsupported_reason() {
            bail!("{why}");
        }
        if !valid_pool_name(name) {
            bail!("share root names use letters, digits, '-' and '_' (up to 40)");
        }
        let roots = self.list()?;
        if roots.iter().any(|r| r.name == name) {
            bail!("a share root named '{name}' already exists");
        }
        let dir = canon(path)?;
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        for r in &roots {
            let other = Path::new(&r.path);
            if dir.starts_with(other) || other.starts_with(&dir) {
                bail!(
                    "{} overlaps share root '{}' ({})",
                    dir.display(),
                    r.name,
                    r.path
                );
            }
        }
        for p in self.store.list_pools()? {
            let pool = Path::new(&p.path);
            if dir.starts_with(pool) || pool.starts_with(&dir) {
                bail!(
                    "{} overlaps storage pool '{}': a guest could rewrite VM disks through the share",
                    dir.display(),
                    p.name
                );
            }
        }
        let root = ShareRoot {
            name: name.into(),
            path: dir.display().to_string(),
        };
        self.store
            .insert_share_root(&root.name, &root.path, now())?;
        Ok(root)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        if !self.list()?.iter().any(|r| r.name == name) {
            bail!("unknown share root '{name}'");
        }
        self.store.delete_share_root(name)
    }

    /// Host folder for `root` + relative `path`, guaranteed inside the root.
    pub fn resolve(&self, root: &str, rel: &str) -> Result<PathBuf> {
        if !valid_share_path(rel) {
            bail!("invalid folder '{rel}'");
        }
        let r = self
            .list()?
            .into_iter()
            .find(|r| r.name == root)
            .ok_or_else(|| anyhow!("unknown share root '{root}'"))?;
        let base = canon(Path::new(&r.path))?;
        let mut target = base.clone();
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            target.push(part);
        }
        let target = canon(&target)
            .with_context(|| format!("folder '{rel}' not found in share root '{root}'"))?;
        if !target.starts_with(&base) {
            bail!("folder '{rel}' resolves outside share root '{root}'");
        }
        if !target.is_dir() {
            bail!("'{rel}' is not a folder");
        }
        Ok(target)
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Locate a `virtiofsd` binary (Linux). Distributions install it outside
/// `PATH`, typically in `/usr/libexec`.
pub fn find_virtiofsd() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if let Some(p) = std::env::var_os("QEMU_UI_VIRTIOFSD").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("virtiofsd"))
                .collect()
        })
        .unwrap_or_default();
    for d in [
        "/usr/libexec/virtiofsd",
        "/usr/lib/qemu/virtiofsd",
        "/usr/lib/virtiofsd",
    ] {
        candidates.push(PathBuf::from(d));
    }
    candidates.into_iter().find(|p| p.is_file())
}

// ---------------------------------------------------------------------- API

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/shares", get(info))
        .route("/shares/roots", axum::routing::post(add_root))
        .route("/shares/roots/{name}", delete(remove_root))
}

async fn info(State(app): State<Arc<App>>) -> Result<Json<SharesInfo>, ApiError> {
    let roots = app
        .supervisor
        .shares
        .list()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let reason = host_unsupported_reason();
    Ok(Json(SharesInfo {
        roots,
        supported: reason.is_none(),
        reason: reason.map(String::from),
        virtiofs: app.supervisor.virtiofsd.is_some(),
    }))
}

#[derive(Deserialize)]
struct NewRoot {
    name: String,
    path: String,
}

async fn add_root(
    State(app): State<Arc<App>>,
    Json(b): Json<NewRoot>,
) -> Result<(StatusCode, Json<ShareRoot>), ApiError> {
    app.supervisor
        .shares
        .add(&b.name, Path::new(&b.path))
        .map(|r| (StatusCode::CREATED, Json(r)))
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

async fn remove_root(
    State(app): State<Arc<App>>,
    UrlPath(name): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    let users: Vec<String> = app
        .supervisor
        .store
        .list_vms()
        .unwrap_or_default()
        .into_iter()
        .filter(|v| {
            serde_json::from_str::<VmSpec>(&v.spec_json)
                .is_ok_and(|s| s.shares.iter().any(|sh| sh.root == name))
        })
        .map(|v| v.name)
        .collect();
    if !users.is_empty() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("share root '{name}' is used by: {}", users.join(", ")),
        ));
    }
    app.supervisor
        .shares
        .remove(&name)
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qui-share-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolve_is_confined_to_the_root() {
        let store = Store::memory().unwrap();
        let root = tmp("root");
        std::fs::create_dir_all(root.join("projects/web")).unwrap();
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        // Register directly: `add` refuses on Windows hosts by design.
        let canon_root = canon(&root).unwrap();
        store
            .insert_share_root("src", &canon_root.display().to_string(), 0)
            .unwrap();
        let roots = ShareRoots::new(store);

        assert_eq!(roots.resolve("src", "").unwrap(), canon_root);
        assert_eq!(
            roots.resolve("src", "projects/web").unwrap(),
            canon_root.join("projects").join("web")
        );
        assert!(roots.resolve("src", "../").is_err());
        assert!(roots.resolve("src", "projects/../..").is_err());
        assert!(roots.resolve("src", "missing").is_err());
        assert!(
            roots.resolve("src", "file.txt").is_err(),
            "files are not folders"
        );
        assert!(roots.resolve("other", "").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_root() {
        let store = Store::memory().unwrap();
        let root = tmp("sym");
        let outside = tmp("outside");
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        store
            .insert_share_root("src", &canon(&root).unwrap().display().to_string(), 0)
            .unwrap();
        let roots = ShareRoots::new(store);
        assert!(roots.resolve("src", "escape").is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn roots_may_not_overlap_pools_or_each_other() {
        let store = Store::memory().unwrap();
        let pool = tmp("pool");
        store
            .insert_pool(
                &crate::store::PoolRow {
                    name: "p".into(),
                    path: canon(&pool).unwrap().display().to_string(),
                    quota_bytes: None,
                    readonly: false,
                },
                0,
            )
            .unwrap();
        let roots = ShareRoots::new(store);
        let inner = pool.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        if host_unsupported_reason().is_some() {
            // Windows hosts: registering is refused outright, with the reason.
            let e = roots.add("x", &tmp("any")).unwrap_err().to_string();
            assert!(e.contains("Windows"), "{e}");
        } else {
            assert!(
                roots
                    .add("in-pool", &inner)
                    .unwrap_err()
                    .to_string()
                    .contains("pool")
            );
            assert!(roots.add("over-pool", pool.parent().unwrap()).is_err());
            let ok = tmp("ok");
            roots.add("ok", &ok).unwrap();
            assert!(roots.add("ok2", &ok).is_err(), "same folder");
            assert!(roots.add("bad/name", &tmp("bn")).is_err());
            roots.remove("ok").unwrap();
        }
        let _ = std::fs::remove_dir_all(&pool);
    }
}

/// Shared folders against a real QEMU. Runs only where the QEMU binary has
/// virtio-9p (Linux builds; the Windows build has no fsdev support).
#[cfg(test)]
mod real {
    use crate::supervisor::test_support::*;
    use qemu_ui_core::compile;
    use qemu_ui_core::spec::ShareSpec;
    use qemu_ui_core::vm::VmState;
    use qemu_ui_qmp::Qmp;
    use serde_json::{Value, json};

    /// `/machine/peripheral` children: (id, type).
    async fn peripherals(sock: &std::path::Path) -> Vec<(String, String)> {
        let q = Qmp::connect_unix(sock).await.unwrap();
        let v: Value = q
            .execute("qom-list", Some(json!({"path": "/machine/peripheral"})))
            .await
            .unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap_or("").to_string(),
                    p["type"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn vm_with_shared_folders() {
        let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
            eprintln!("skipping: qemu not found");
            return;
        };
        let data = short_data_dir("shr");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let caps = host_caps(&qemu, &img).await;
            if compile::ninep_device(&caps.binaries[0]).is_none() {
                eprintln!("skipping: this QEMU has no virtio-9p (e.g. the Windows build)");
                return;
            }
            let sv = make(&data, &caps).await;
            let folder = data.join("shared");
            std::fs::create_dir_all(folder.join("sub")).unwrap();
            sv.shares.add("src", &folder).unwrap();
            assert!(
                sv.shares.add("pool", &sv.pools.default_dir).is_err(),
                "roots may not overlap pools"
            );

            // Read-only share: always 9p.
            let id = small_vm(&sv, &caps, "shares").await;
            let mut spec = sv.detail(&id).ok().unwrap().spec;
            spec.shares = vec![ShareSpec {
                root: "src".into(),
                path: "sub".into(),
                tag: "hostsrc".into(),
                readonly: true,
            }];
            sv.update(&id, spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
            let run = sv.store.last_run(&id).unwrap().unwrap();
            let args: Vec<String> = serde_json::from_str(&run.args_json).unwrap();
            assert!(
                args.iter()
                    .any(|a| a.starts_with("local,id=qui-fs0,") && a.ends_with(",readonly=on")),
                "{args:?}"
            );
            let sock = std::path::Path::new(&run.run_dir).join("qmp-user.sock");
            let p = peripherals(&sock).await;
            assert!(
                p.iter()
                    .any(|(n, t)| n == "qui-share0" && t.contains("virtio-9p")),
                "{p:?}"
            );
            sv.force_stop(&id)
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            wait_state(&sv, &id, VmState::Stopped).await;

            // An unknown root is refused at start with a field-pinned error.
            spec.shares[0].root = "nope".into();
            sv.update(&id, spec.clone())
                .await
                .map_err(|e| format!("{e:?}"))
                .unwrap();
            let err = format!("{:?}", sv.start(&id).await.err().unwrap());
            assert!(
                err.contains("share.unresolved") && err.contains("shares[0]"),
                "{err}"
            );

            // Writable share: virtiofs when virtiofsd is installed.
            if sv.virtiofsd.is_some() && compile::virtiofs_supported(&caps.binaries[0]) {
                spec.shares = vec![ShareSpec {
                    root: "src".into(),
                    path: String::new(),
                    tag: "rw".into(),
                    readonly: false,
                }];
                sv.update(&id, spec.clone())
                    .await
                    .map_err(|e| format!("{e:?}"))
                    .unwrap();
                sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
                let run = sv.store.last_run(&id).unwrap().unwrap();
                let sidecars = sv.store.sidecars(run.id).unwrap();
                assert_eq!(sidecars.len(), 1, "one virtiofsd");
                let helper = crate::procs::ProcId {
                    pid: sidecars[0].1,
                    start_time: sidecars[0].2,
                };
                assert!(crate::procs::alive(helper));
                let sock = std::path::Path::new(&run.run_dir).join("qmp-user.sock");
                let p = peripherals(&sock).await;
                assert!(
                    p.iter()
                        .any(|(n, t)| n == "qui-share0" && t.contains("vhost-user-fs")),
                    "{p:?}"
                );
                sv.force_stop(&id)
                    .await
                    .map_err(|e| format!("{e:?}"))
                    .unwrap();
                wait_state(&sv, &id, VmState::Stopped).await;
                for _ in 0..50 {
                    if !crate::procs::alive(helper) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                assert!(!crate::procs::alive(helper), "virtiofsd is gone after stop");
            } else {
                eprintln!("virtiofs part skipped: no virtiofsd or no vhost-user-fs-pci");
            }
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&data);
    }
}
