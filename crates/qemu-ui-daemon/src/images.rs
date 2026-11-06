//! Storage pools and the image registry (plan.md §9.1, M2-01/M2-02).
//!
//! A pool is a directory registered in the database; the built-in `default`
//! pool lives in the data directory. Images are the regular files in a pool;
//! the filesystem is the truth and `qemu-img info` is the authority on each
//! image (cached by path, size and mtime).
//!
//! Every path handed to QEMU is derived from a validated `ImageRef` and
//! checked to stay inside its pool after canonicalisation, so a spec can
//! never point QEMU at an arbitrary host file (passthrough is the gated
//! exception). Pools may not overlap, so an image belongs to exactly one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::spec::{ImageRef, valid_image_name};
use qemu_ui_core::storage::{ChainLink, CheckResult, MeasureResult, PoolSummary, valid_pool_name};
use qemu_ui_core::vm::{ImageInfo, PoolInfo};
use serde_json::Value;

use crate::probe::command;
use crate::scan::strip_verbatim;
use crate::store::{PoolRow, Store};

pub const DEFAULT_POOL: &str = "default";

pub struct Pools {
    store: Store,
    pub default_dir: PathBuf,
    /// `qemu-img info` results by (path, size, mtime).
    info_cache: Mutex<HashMap<(PathBuf, u64, u64), Value>>,
}

fn canon(p: &Path) -> Result<PathBuf> {
    let c = std::fs::canonicalize(p).with_context(|| format!("{} does not exist", p.display()))?;
    Ok(PathBuf::from(strip_verbatim(&c.display().to_string())))
}

fn file_key(path: &Path) -> Option<(PathBuf, u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((path.to_path_buf(), m.len(), mtime))
}

impl Pools {
    pub fn new(data_dir: &Path, store: Store) -> Result<Self> {
        let default_dir = data_dir.join("pools").join(DEFAULT_POOL);
        std::fs::create_dir_all(&default_dir)?;
        let default_dir = canon(&default_dir)?;
        let pools = Pools {
            store,
            default_dir,
            info_cache: Mutex::default(),
        };
        if !pools
            .store
            .list_pools()?
            .iter()
            .any(|p| p.name == DEFAULT_POOL)
        {
            pools.store.insert_pool(
                &PoolRow {
                    name: DEFAULT_POOL.into(),
                    path: pools.default_dir.display().to_string(),
                    quota_bytes: None,
                    readonly: false,
                },
                0,
            )?;
        }
        Ok(pools)
    }

    pub fn rows(&self) -> Result<Vec<PoolRow>> {
        self.store.list_pools()
    }

    pub fn get(&self, pool: &str) -> Result<PoolRow> {
        self.rows()?
            .into_iter()
            .find(|p| p.name == pool)
            .ok_or_else(|| anyhow!("unknown pool '{pool}'"))
    }

    pub fn pool_dir(&self, pool: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.get(pool)?.path))
    }

    /// Host path of an existing image, guaranteed to be inside its pool.
    pub fn resolve(&self, r: &ImageRef) -> Result<PathBuf> {
        if !valid_image_name(&r.name) {
            bail!("invalid image name '{}'", r.name);
        }
        let dir = canon(&self.pool_dir(&r.pool)?)?;
        let path = canon(&dir.join(&r.name))
            .with_context(|| format!("image '{}' not found in pool '{}'", r.name, r.pool))?;
        if !path.starts_with(&dir) {
            bail!("image '{}' resolves outside its pool", r.name);
        }
        if !path.is_file() {
            bail!("image '{}' is not a regular file", r.name);
        }
        Ok(path)
    }

    /// Host path for a new image: valid name, writable pool, does not exist.
    pub fn new_image_path(&self, r: &ImageRef) -> Result<PathBuf> {
        if !valid_image_name(&r.name) {
            bail!("invalid image name '{}'", r.name);
        }
        let pool = self.get(&r.pool)?;
        if pool.readonly {
            bail!("pool '{}' is read-only", r.pool);
        }
        let path = PathBuf::from(&pool.path).join(&r.name);
        if path.exists() {
            bail!("'{}' already exists in pool '{}'", r.name, r.pool);
        }
        Ok(path)
    }

    /// Which pool image (if any) is this host path?
    pub fn locate(&self, path: &Path) -> Option<ImageRef> {
        let path = canon(path).ok()?;
        self.rows().ok()?.into_iter().find_map(|p| {
            let rel = path.strip_prefix(&p.path).ok()?;
            let name = rel.to_str()?;
            (valid_image_name(name) && !name.contains(['/', '\\'])).then(|| ImageRef {
                pool: p.name.clone(),
                name: name.to_string(),
            })
        })
    }

    // ------------------------------------------------------------- pool CRUD

    pub fn add(
        &self,
        name: &str,
        path: &Path,
        quota_bytes: Option<u64>,
        readonly: bool,
    ) -> Result<PoolRow> {
        if !valid_pool_name(name) {
            bail!("pool names use letters, digits, '-' and '_' (up to 40)");
        }
        let rows = self.rows()?;
        if rows.iter().any(|p| p.name == name) {
            bail!("a pool named '{name}' already exists");
        }
        let dir = canon(path)?;
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        for p in &rows {
            let other = Path::new(&p.path);
            if dir.starts_with(other) || other.starts_with(&dir) {
                bail!("{} overlaps pool '{}' ({})", dir.display(), p.name, p.path);
            }
        }
        let row = PoolRow {
            name: name.into(),
            path: dir.display().to_string(),
            quota_bytes,
            readonly,
        };
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.store.insert_pool(&row, now)?;
        Ok(row)
    }

    pub fn update(&self, name: &str, quota_bytes: Option<u64>, readonly: bool) -> Result<()> {
        self.get(name)?;
        self.store.update_pool(name, quota_bytes, readonly)
    }

    /// Unregister a pool. Files are never deleted.
    pub fn remove(&self, name: &str) -> Result<()> {
        if name == DEFAULT_POOL {
            bail!("the default pool cannot be removed");
        }
        self.get(name)?;
        self.store.delete_pool(name)
    }

    // --------------------------------------------------------- image queries

    /// `qemu-img info --output=json -U`, cached until the file changes.
    pub async fn info_json(&self, qemu_img: &Path, path: &Path) -> Result<Value> {
        let key = file_key(path);
        if let Some(k) = &key
            && let Some(v) = self.info_cache.lock().unwrap().get(k)
        {
            return Ok(v.clone());
        }
        let out = command(qemu_img)
            .args(["info", "-U", "--output=json"])
            .arg(path)
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "qemu-img info: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        if let Some(k) = key {
            let mut c = self.info_cache.lock().unwrap();
            if c.len() > 4096 {
                c.clear();
            }
            c.insert(k, v.clone());
        }
        Ok(v)
    }

    /// (format, virtual size, actual size).
    pub async fn info(&self, qemu_img: &Path, path: &Path) -> Result<(String, u64, u64)> {
        let v = self.info_json(qemu_img, path).await?;
        Ok((
            v["format"].as_str().unwrap_or("raw").to_string(),
            v["virtual-size"].as_u64().unwrap_or(0),
            v["actual-size"].as_u64().unwrap_or(0),
        ))
    }

    fn image_names(&self, dir: &Path) -> Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| valid_image_name(n))
            .collect();
        names.sort();
        Ok(names)
    }

    pub async fn list(&self, qemu_img: Option<&Path>, pool: &str) -> Result<PoolInfo> {
        let dir = self.pool_dir(pool)?;
        let mut images = Vec::new();
        for name in self.image_names(&dir)? {
            let path = dir.join(&name);
            let fallback = || {
                (
                    "unknown".to_string(),
                    0,
                    std::fs::metadata(&path).map_or(0, |m| m.len()),
                )
            };
            let (format, virtual_size, actual_size) = match qemu_img {
                Some(q) => self.info(q, &path).await.unwrap_or_else(|_| fallback()),
                None => fallback(),
            };
            images.push(ImageInfo {
                pool: pool.into(),
                name,
                format,
                virtual_size,
                actual_size,
                used_by: vec![],
            });
        }
        Ok(PoolInfo {
            name: pool.into(),
            path: dir.display().to_string(),
            images,
        })
    }

    pub async fn summaries(&self, qemu_img: Option<&Path>) -> Result<Vec<PoolSummary>> {
        let disks = sysinfo::Disks::new_with_refreshed_list().ok();
        let mut out = Vec::new();
        for row in self.rows()? {
            let info = self.list(qemu_img, &row.name).await.unwrap_or(PoolInfo {
                name: row.name.clone(),
                path: row.path.clone(),
                images: vec![],
            });
            let fs_free_bytes = disks
                .iter()
                .flat_map(|d| d.iter())
                .filter(|d| Path::new(&row.path).starts_with(d.mount_point()))
                .max_by_key(|d| d.mount_point().as_os_str().len())
                .map(|d| d.available_space());
            out.push(PoolSummary {
                builtin: row.name == DEFAULT_POOL,
                used_bytes: info.images.iter().map(|i| i.actual_size).sum(),
                image_count: info.images.len() as u32,
                name: row.name,
                path: row.path,
                quota_bytes: row.quota_bytes,
                fs_free_bytes,
                readonly: row.readonly,
            });
        }
        Ok(out)
    }

    /// Refuse an operation that would add `additional` bytes beyond quota.
    pub async fn check_quota(
        &self,
        qemu_img: Option<&Path>,
        pool: &str,
        additional: u64,
    ) -> Result<()> {
        let row = self.get(pool)?;
        let Some(quota) = row.quota_bytes else {
            return Ok(());
        };
        let used: u64 = self
            .list(qemu_img, pool)
            .await?
            .images
            .iter()
            .map(|i| i.actual_size)
            .sum();
        if used.saturating_add(additional) > quota {
            bail!(
                "pool '{pool}' quota exceeded: {used} bytes used + {additional} needed > {quota} bytes allowed"
            );
        }
        Ok(())
    }

    /// Full detail including the backing chain (top first, excluding self).
    pub async fn detail(&self, qemu_img: &Path, r: &ImageRef) -> Result<(Value, Vec<ChainLink>)> {
        let path = self.resolve(r)?;
        let top = self.info_json(qemu_img, &path).await?;
        let out = command(qemu_img)
            .args(["info", "-U", "--backing-chain", "--output=json"])
            .arg(&path)
            .output()
            .await?;
        let mut chain = Vec::new();
        if out.status.success()
            && let Ok(Value::Array(levels)) = serde_json::from_slice::<Value>(&out.stdout)
        {
            for level in levels.iter().skip(1) {
                let filename = level["filename"].as_str().unwrap_or_default().to_string();
                let loc = self.locate(Path::new(&filename));
                chain.push(ChainLink {
                    format: level["format"].as_str().map(String::from),
                    pool: loc.as_ref().map(|l| l.pool.clone()),
                    name: loc.map(|l| l.name),
                    filename,
                    missing: false,
                });
            }
        } else if let Some(b) = top["full-backing-filename"]
            .as_str()
            .or(top["backing-filename"].as_str())
        {
            // The chain could not be opened: show the first missing link.
            chain.push(ChainLink {
                filename: b.to_string(),
                format: top["backing-filename-format"].as_str().map(String::from),
                pool: None,
                name: None,
                missing: true,
            });
        }
        Ok((top, chain))
    }

    pub async fn check(&self, qemu_img: &Path, r: &ImageRef) -> Result<CheckResult> {
        let path = self.resolve(r)?;
        let out = command(qemu_img)
            .args(["check", "-U", "--output=json"])
            .arg(&path)
            .output()
            .await?;
        // Exit codes: 0 ok, 2 corruptions, 3 leaks only; 1 = could not check.
        if out.status.code() == Some(1) || out.stdout.is_empty() {
            bail!(
                "qemu-img check: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let corruptions =
            v["corruptions"].as_u64().unwrap_or(0) + v["check-errors"].as_u64().unwrap_or(0);
        let leaks = v["leaks"].as_u64().unwrap_or(0);
        Ok(CheckResult {
            ok: corruptions == 0 && leaks == 0,
            corruptions,
            leaks,
            allocated_clusters: v["allocated-clusters"].as_u64(),
            total_clusters: v["total-clusters"].as_u64(),
            output: serde_json::to_string_pretty(&v).unwrap_or_default(),
        })
    }

    /// `qemu-img measure` for converting an existing image, or for a new
    /// image of `size_bytes`.
    pub async fn measure(
        qemu_img: &Path,
        source: Option<&Path>,
        size_bytes: Option<u64>,
        format: &str,
    ) -> Result<MeasureResult> {
        let mut cmd = command(qemu_img);
        cmd.args(["measure", "--output=json", "-O", format]);
        match (source, size_bytes) {
            (Some(p), _) => {
                cmd.arg("-U").arg(p);
            }
            (None, Some(s)) => {
                cmd.args(["--size", &s.to_string()]);
            }
            _ => bail!("measure needs a source image or a size"),
        }
        let out = cmd.output().await?;
        if !out.status.success() {
            bail!(
                "qemu-img measure: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)?;
        Ok(MeasureResult {
            required: v["required"].as_u64().unwrap_or(0),
            fully_allocated: v["fully-allocated"].as_u64().unwrap_or(0),
        })
    }

    /// Images (across all pools) whose backing file is `target`.
    pub async fn dependents(&self, qemu_img: &Path, target: &Path) -> Result<Vec<ImageRef>> {
        let target = canon(target)?;
        let mut out = Vec::new();
        for row in self.rows()? {
            let dir = PathBuf::from(&row.path);
            for name in self.image_names(&dir).unwrap_or_default() {
                let path = dir.join(&name);
                let Ok(v) = self.info_json(qemu_img, &path).await else {
                    continue;
                };
                let backing = v["full-backing-filename"]
                    .as_str()
                    .or(v["backing-filename"].as_str());
                if let Some(b) = backing {
                    let b = if Path::new(b).is_absolute() {
                        PathBuf::from(b)
                    } else {
                        dir.join(b)
                    };
                    if canon(&b).ok().as_ref() == Some(&target) {
                        out.push(ImageRef {
                            pool: row.name.clone(),
                            name,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Create an empty disk image (run inside a job by the API).
    pub async fn create(
        &self,
        qemu_img: &Path,
        r: &ImageRef,
        size_bytes: u64,
        format: &str,
    ) -> Result<()> {
        if !matches!(format, "qcow2" | "raw") {
            bail!("format must be qcow2 or raw");
        }
        if size_bytes < 1024 * 1024 {
            bail!("size must be at least 1 MiB");
        }
        let path = self.new_image_path(r)?;
        let need = Self::measure(qemu_img, None, Some(size_bytes), format)
            .await
            .map_or(0, |m| m.required);
        self.check_quota(Some(qemu_img), &r.pool, need).await?;
        let out = command(qemu_img)
            .args(["create", "-q", "-f", format])
            .arg(&path)
            .arg(size_bytes.to_string())
            .output()
            .await?;
        if !out.status.success() {
            return Err(anyhow!(
                "qemu-img create: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qui-pool-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolve_stays_inside_pool() {
        let data = tmp("res");
        let pools = Pools::new(&data, Store::memory().unwrap()).unwrap();
        std::fs::write(pools.default_dir.join("ok.qcow2"), b"x").unwrap();
        let r = |n: &str| ImageRef {
            pool: DEFAULT_POOL.into(),
            name: n.into(),
        };
        assert!(pools.resolve(&r("ok.qcow2")).is_ok());
        assert!(pools.resolve(&r("../qemu-ui.db")).is_err());
        assert!(pools.resolve(&r("missing.qcow2")).is_err());
        assert!(
            pools
                .resolve(&ImageRef {
                    pool: "other".into(),
                    name: "ok.qcow2".into()
                })
                .is_err()
        );
        assert_eq!(
            pools.locate(&pools.default_dir.join("ok.qcow2")),
            Some(r("ok.qcow2"))
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn pools_cannot_overlap_and_default_is_permanent() {
        let data = tmp("ovl");
        let pools = Pools::new(&data, Store::memory().unwrap()).unwrap();
        let extra = tmp("extra");
        pools.add("extra", &extra, Some(1 << 30), false).unwrap();
        assert!(
            pools.add("extra2", &extra, None, false).is_err(),
            "same dir"
        );
        let inner = extra.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        assert!(pools.add("inner", &inner, None, false).is_err(), "nested");
        assert!(pools.add("bad/name", &tmp("bn"), None, false).is_err());
        assert!(pools.remove(DEFAULT_POOL).is_err());
        pools.update("extra", None, true).unwrap();
        assert!(
            pools
                .new_image_path(&ImageRef {
                    pool: "extra".into(),
                    name: "x.qcow2".into()
                })
                .is_err(),
            "read-only"
        );
        pools.remove("extra").unwrap();
        assert!(extra.exists(), "files are never deleted");
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&extra);
    }
}
