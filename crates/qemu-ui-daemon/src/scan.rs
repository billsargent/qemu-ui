//! Host capability scan: discovery → hashing → probe (or cache) → firmware →
//! parity refinement. Runs in the background; progress is observable.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use qemu_ui_core::caps::{Binary, BinaryCaps, HostCaps, HostInfo, ProbeError, ScanStatus};
use qemu_ui_core::firmware::{self, Firmware};
use qemu_ui_core::{arch, parity};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, Semaphore};

use crate::discovery::{self, Candidate, SearchDirs};
use crate::probe;
use crate::store::Store;

const LAST_SCAN_KEY: &str = "host_caps";

pub struct Scanner {
    pub store: Store,
    pub project_dirs: Vec<PathBuf>,
    pub configured_dirs: Vec<PathBuf>,
    pub caps: RwLock<Option<HostCaps>>,
    pub status: RwLock<ScanStatus>,
}

pub fn host_info() -> HostInfo {
    HostInfo {
        os: std::env::consts::OS.to_string(),
        arch: arch::host_to_qemu(std::env::consts::ARCH).to_string(),
    }
}

impl Scanner {
    pub fn new(
        store: Store,
        project_dirs: Vec<PathBuf>,
        configured_dirs: Vec<PathBuf>,
    ) -> Arc<Self> {
        let cached = store
            .get_kv(LAST_SCAN_KEY)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<HostCaps>(&s).ok());
        Arc::new(Scanner {
            store,
            project_dirs,
            configured_dirs,
            caps: RwLock::new(cached),
            status: RwLock::default(),
        })
    }

    /// Start a scan in the background unless one is running. `force` re-probes
    /// binaries whose snapshot is cached.
    pub async fn start(self: &Arc<Self>, force: bool) -> ScanStatus {
        {
            let mut st = self.status.write().await;
            if st.running {
                return st.clone();
            }
            *st = ScanStatus {
                running: true,
                ..Default::default()
            };
        }
        let this = self.clone();
        tokio::spawn(async move {
            let result = this.run(force).await;
            let mut st = this.status.write().await;
            st.running = false;
            st.current = None;
            if let Err(e) = result {
                tracing::error!("capability scan failed: {e:#}");
                st.error = Some(format!("{e:#}"));
            }
        });
        self.status.read().await.clone()
    }

    async fn run(self: &Arc<Self>, force: bool) -> Result<()> {
        let search = SearchDirs::new(&self.project_dirs, &self.configured_dirs);
        let candidates = tokio::task::spawn_blocking(move || discovery::discover(&search)).await?;
        let (img, systems): (Vec<_>, Vec<_>) =
            candidates.into_iter().partition(|c| c.arch.is_none());
        self.status.write().await.total = systems.len() as u32;
        tracing::info!(binaries = systems.len(), "capability scan started");

        let host = host_info();
        let parallel =
            std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        let sem = Arc::new(Semaphore::new(parallel));
        let mut tasks = Vec::new();
        for c in systems {
            let this = self.clone();
            let sem = sem.clone();
            let host = host.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                this.status.write().await.current = Some(c.path.display().to_string());
                let r = this.probe_one(&c, &host, force).await;
                this.status.write().await.done += 1;
                r
            }));
        }
        let mut binaries = Vec::new();
        let mut failed = Vec::new();
        for t in tasks {
            match t.await? {
                Ok(b) => binaries.push(b),
                Err(pe) => {
                    tracing::warn!(path = %pe.binary.path, "probe failed: {}", pe.error);
                    failed.push(*pe);
                }
            }
        }
        binaries.sort_by(|a, b| {
            (!a.native, &a.arch, &a.binary.path).cmp(&(!b.native, &b.arch, &b.binary.path))
        });

        let qemu_img = match img.first() {
            Some(c) => {
                let mut b = self.binary_for(c).await?;
                b.version = probe::qemu_img_version(&c.path).await.unwrap_or_default();
                Some(b)
            }
            None => None,
        };

        let bin_dirs: Vec<PathBuf> = binaries
            .iter()
            .filter_map(|b| Path::new(&b.binary.path).parent().map(Path::to_path_buf))
            .collect();
        let firmware = tokio::task::spawn_blocking(move || scan_firmware(&bin_dirs)).await?;
        let parity = parity::refine(parity::ledger(), &host.os, &binaries);

        let caps = HostCaps {
            host,
            binaries,
            failed,
            qemu_img,
            firmware,
            parity,
            scanned_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        };
        self.store
            .put_kv(LAST_SCAN_KEY, &serde_json::to_string(&caps)?)?;
        tracing::info!(
            ok = caps.binaries.len(),
            failed = caps.failed.len(),
            firmware = caps.firmware.len(),
            "capability scan finished"
        );
        *self.caps.write().await = Some(caps);
        Ok(())
    }

    async fn probe_one(
        &self,
        c: &Candidate,
        host: &HostInfo,
        force: bool,
    ) -> Result<BinaryCaps, Box<ProbeError>> {
        let binary = match self.binary_for(c).await {
            Ok(b) => b,
            Err(e) => {
                let b = Binary {
                    id: String::new(),
                    path: c.path.display().to_string(),
                    arch: c.arch.clone(),
                    version: String::new(),
                    sha256: String::new(),
                    source: c.source.into(),
                };
                return Err(Box::new(ProbeError {
                    binary: b,
                    error: format!("{e:#}"),
                }));
            }
        };
        if !force
            && let Ok(Some(json)) = self.store.snapshot(&binary.sha256)
            && let Ok(mut caps) = serde_json::from_str::<BinaryCaps>(&json)
            && caps.probe_version == qemu_ui_core::caps::PROBE_VERSION
        {
            // Same bytes, possibly a different location.
            caps.binary.path = binary.path.clone();
            caps.binary.source = binary.source.clone();
            caps.native = arch::is_native(&caps.arch, &host.arch);
            return Ok(caps);
        }
        match probe::probe_binary(binary.clone(), host).await {
            Ok(caps) => {
                if let Ok(json) = serde_json::to_string(&caps) {
                    let _ = self
                        .store
                        .put_snapshot(&binary.sha256, caps.probed_at, &json);
                }
                Ok(caps)
            }
            Err(e) => Err(Box::new(ProbeError {
                binary,
                error: format!("{e:#}"),
            })),
        }
    }

    /// Build a `Binary`, hashing the file only when size/mtime changed.
    async fn binary_for(&self, c: &Candidate) -> Result<Binary> {
        let path = c.path.display().to_string();
        let sha256 = match self.store.cached_hash(&path, c.size, c.mtime)? {
            Some(h) => h,
            None => {
                let p = c.path.clone();
                let h = tokio::task::spawn_blocking(move || sha256_file(&p)).await??;
                self.store.put_hash(&path, c.size, c.mtime, &h)?;
                h
            }
        };
        Ok(Binary {
            id: sha256[..16].to_string(),
            path,
            arch: c.arch.clone(),
            version: String::new(),
            sha256,
            source: c.source.into(),
        })
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
}

/// Read every firmware descriptor reachable from the binaries' directories.
fn scan_firmware(bin_dirs: &[PathBuf]) -> Vec<Firmware> {
    let mut by_id: BTreeMap<String, Firmware> = BTreeMap::new();
    let mut seen_dirs = std::collections::HashSet::new();
    for bin in bin_dirs {
        if !seen_dirs.insert(bin.clone()) {
            continue;
        }
        let blob_dirs = discovery::firmware_blob_dirs(bin);
        for dir in discovery::firmware_descriptor_dirs(bin) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let id = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if by_id.contains_key(&id) {
                    continue; // earlier directories take precedence, as in QEMU's own search
                }
                let Ok(json) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let resolve = |declared: &str| resolve_blob(declared, &blob_dirs);
                match firmware::parse_descriptor(&id, &json, resolve) {
                    Ok(fw) => {
                        by_id.insert(id, fw);
                    }
                    Err(err) => tracing::warn!(path = %p.display(), "{err}"),
                }
            }
        }
    }
    by_id.into_values().collect()
}

/// Declared path if it exists, else the same file name in a known blob dir.
fn resolve_blob(declared: &str, blob_dirs: &[PathBuf]) -> Option<String> {
    let p = Path::new(declared);
    if p.is_file() {
        return Some(declared.to_string());
    }
    let name = declared.rsplit(['/', '\\']).next()?;
    blob_dirs
        .iter()
        .map(|d| d.join(name))
        .find(|c| c.is_file())
        .and_then(|c| std::fs::canonicalize(&c).ok().or(Some(c)))
        .map(|c| strip_verbatim(&c.display().to_string()))
}

/// `\\?\C:\x` → `C:\x` for display and for passing to QEMU.
pub fn strip_verbatim(s: &str) -> String {
    s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
}
