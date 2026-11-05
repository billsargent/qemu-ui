//! Image pools, M1 subset: one `default` pool directory. Full pools, quotas,
//! uploads and jobs are M2 (plan.md §9).
//!
//! Every path handed to QEMU is derived from a validated `ImageRef` and
//! checked to stay inside the pool after canonicalisation, so a spec can
//! never point QEMU at an arbitrary host file (that is what passthrough is
//! for, and it is gated separately).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::spec::{ImageRef, valid_image_name};
use qemu_ui_core::vm::{ImageInfo, PoolInfo};
use serde::Deserialize;

use crate::probe::command;
use crate::scan::strip_verbatim;

pub const DEFAULT_POOL: &str = "default";

pub struct Pools {
    pub default_dir: PathBuf,
}

#[derive(Deserialize)]
struct QemuImgInfo {
    format: String,
    #[serde(rename = "virtual-size")]
    virtual_size: u64,
    #[serde(rename = "actual-size", default)]
    actual_size: u64,
}

impl Pools {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let default_dir = data_dir.join("pools").join(DEFAULT_POOL);
        std::fs::create_dir_all(&default_dir)?;
        Ok(Pools { default_dir })
    }

    pub fn pool_dir(&self, pool: &str) -> Result<&Path> {
        if pool == DEFAULT_POOL {
            Ok(&self.default_dir)
        } else {
            bail!("unknown pool '{pool}' (only '{DEFAULT_POOL}' exists until M2)")
        }
    }

    /// Host path of an image, guaranteed to be inside its pool.
    pub fn resolve(&self, r: &ImageRef) -> Result<PathBuf> {
        if !valid_image_name(&r.name) {
            bail!("invalid image name '{}'", r.name);
        }
        let dir = std::fs::canonicalize(self.pool_dir(&r.pool)?)?;
        let path = dir.join(&r.name);
        let canon = std::fs::canonicalize(&path)
            .with_context(|| format!("image '{}' not found in pool '{}'", r.name, r.pool))?;
        if !canon.starts_with(&dir) {
            bail!("image '{}' resolves outside its pool", r.name);
        }
        if !canon.is_file() {
            bail!("image '{}' is not a regular file", r.name);
        }
        Ok(PathBuf::from(strip_verbatim(&canon.display().to_string())))
    }

    /// `qemu-img info` (with `-U` so images in use can be inspected).
    pub async fn info(qemu_img: &Path, path: &Path) -> Result<(String, u64, u64)> {
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
        let i: QemuImgInfo = serde_json::from_slice(&out.stdout)?;
        Ok((i.format, i.virtual_size, i.actual_size))
    }

    pub async fn list(&self, qemu_img: Option<&Path>, pool: &str) -> Result<PoolInfo> {
        let dir = self.pool_dir(pool)?;
        let mut images = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| valid_image_name(n))
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let (format, virtual_size, actual_size) = match qemu_img {
                Some(q) => Self::info(q, &path).await.unwrap_or_else(|_| {
                    (
                        "unknown".into(),
                        0,
                        std::fs::metadata(&path).map_or(0, |m| m.len()),
                    )
                }),
                None => (
                    "unknown".into(),
                    0,
                    std::fs::metadata(&path).map_or(0, |m| m.len()),
                ),
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
            path: strip_verbatim(&std::fs::canonicalize(dir)?.display().to_string()),
            images,
        })
    }

    /// Create an empty disk image.
    pub async fn create(
        &self,
        qemu_img: &Path,
        r: &ImageRef,
        size_bytes: u64,
        format: &str,
    ) -> Result<()> {
        if !valid_image_name(&r.name) {
            bail!("invalid image name '{}'", r.name);
        }
        if !matches!(format, "qcow2" | "raw") {
            bail!("format must be qcow2 or raw");
        }
        if size_bytes < 1024 * 1024 {
            bail!("size must be at least 1 MiB");
        }
        let path = self.pool_dir(&r.pool)?.join(&r.name);
        if path.exists() {
            bail!("'{}' already exists", r.name);
        }
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

    #[test]
    fn resolve_stays_inside_pool() {
        let tmp = std::env::temp_dir().join(format!("qui-pool-{}", std::process::id()));
        let pools = Pools::new(&tmp).unwrap();
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
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
