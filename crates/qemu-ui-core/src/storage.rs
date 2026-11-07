//! Storage and job API types (plan.md §9, M2-01 … M2-03), plus small pure
//! helpers shared by the daemon.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A storage pool: a directory images live in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct PoolSummary {
    pub name: String,
    /// Host directory (canonical).
    pub path: String,
    /// Upper bound on bytes used on disk by the pool's images; `None` = unlimited.
    #[ts(type = "number | null")]
    pub quota_bytes: Option<u64>,
    /// Bytes currently used on disk.
    #[ts(type = "number")]
    pub used_bytes: u64,
    /// Free space on the filesystem holding the pool.
    #[ts(type = "number | null")]
    pub fs_free_bytes: Option<u64>,
    /// No writes: images can be attached read-only only.
    pub readonly: bool,
    pub image_count: u32,
    /// The built-in pool; cannot be removed.
    pub builtin: bool,
}

/// One element of an image's backing chain, top first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ChainLink {
    /// File name as recorded in the image header.
    pub filename: String,
    pub format: Option<String>,
    /// Pool + name when the file lies inside a pool.
    pub pool: Option<String>,
    pub name: Option<String>,
    /// The backing file could not be found or opened.
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ImageDetail {
    pub pool: String,
    pub name: String,
    pub format: String,
    #[ts(type = "number")]
    pub virtual_size: u64,
    #[ts(type = "number")]
    pub actual_size: u64,
    /// qcow2: cluster size, compat level, encryption, dirty flag …
    #[ts(type = "Record<string, unknown>")]
    pub format_specific: serde_json::Value,
    /// Backing files below this image, nearest first.
    pub backing_chain: Vec<ChainLink>,
    /// Internal snapshots (qcow2).
    pub snapshots: Vec<String>,
    /// VMs whose spec references this image.
    pub used_by: Vec<String>,
    /// VMs currently running with this image open.
    pub in_use_by: Vec<String>,
    /// A template: never opened writable; clone it instead.
    pub template: bool,
    /// Other images (pool/name) that use this one as their backing file.
    pub dependents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct CheckResult {
    pub ok: bool,
    #[ts(type = "number")]
    pub corruptions: u64,
    #[ts(type = "number")]
    pub leaks: u64,
    #[ts(type = "number | null")]
    pub allocated_clusters: Option<u64>,
    #[ts(type = "number | null")]
    pub total_clusters: Option<u64>,
    /// Raw `qemu-img check` output for the curious.
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct MeasureResult {
    /// Bytes needed for the converted image as it is now.
    #[ts(type = "number")]
    pub required: u64,
    /// Bytes needed if fully allocated.
    #[ts(type = "number")]
    pub fully_allocated: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The daemon stopped while the job ran.
    Interrupted,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> JobStatus {
        match s {
            "queued" => JobStatus::Queued,
            "running" => JobStatus::Running,
            "succeeded" => JobStatus::Succeeded,
            "cancelled" => JobStatus::Cancelled,
            "interrupted" => JobStatus::Interrupted,
            _ => JobStatus::Failed,
        }
    }

    pub fn is_finished(self) -> bool {
        !matches!(self, JobStatus::Queued | JobStatus::Running)
    }
}

/// A long-running operation (image conversion, download, upload …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct Job {
    pub id: String,
    /// Dotted kind, e.g. `image.create`, `image.convert`.
    pub kind: String,
    /// Human description, e.g. "Create debian.qcow2 (20 GiB) in default".
    pub title: String,
    pub status: JobStatus,
    /// 0.0 … 1.0, or `None` when unknown.
    pub progress: Option<f64>,
    /// Latest status line.
    pub message: Option<String>,
    pub error: Option<String>,
    /// What the job acts on, for linking in the UI (`{pool, name}` or `{vm}`).
    #[ts(type = "Record<string, unknown>")]
    pub target: serde_json::Value,
    #[ts(type = "number")]
    pub created_at: u64,
    #[ts(type = "number | null")]
    pub started_at: Option<u64>,
    #[ts(type = "number | null")]
    pub finished_at: Option<u64>,
    pub cancellable: bool,
}

/// A registered share root: a host folder VMs may share subfolders of.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct ShareRoot {
    pub name: String,
    /// Canonical host folder.
    pub path: String,
}

/// `GET /shares`: share roots plus what this host supports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct SharesInfo {
    pub roots: Vec<ShareRoot>,
    /// Folder sharing can work on this host (per VM it still depends on the
    /// QEMU binary; the editor's diagnostics say so).
    pub supported: bool,
    /// Why not, when `supported` is false.
    pub reason: Option<String>,
    /// A `virtiofsd` was found: writable shares use virtiofs, else 9p.
    pub virtiofs: bool,
}

/// Parse a `qemu-img … -p` progress update such as `    (12.34/100%)`.
/// The tool redraws the line with `\r`; pass any fragment.
pub fn parse_qemu_img_progress(s: &str) -> Option<f64> {
    let start = s.rfind('(')?;
    let rest = &s[start + 1..];
    let (num, _) = rest.split_once('/')?;
    let v: f64 = num.trim().parse().ok()?;
    (0.0..=100.0).contains(&v).then_some(v / 100.0)
}

/// Is `name` usable as a pool name?
pub fn valid_pool_name(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(x) if x.is_ascii_alphanumeric())
        && name.len() <= 40
        && c.all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines() {
        assert_eq!(parse_qemu_img_progress("    (12.50/100%)"), Some(0.125));
        assert_eq!(
            parse_qemu_img_progress("\r    (0.00/100%)\r    (100.00/100%)"),
            Some(1.0)
        );
        assert_eq!(parse_qemu_img_progress("no progress here"), None);
        assert_eq!(parse_qemu_img_progress("(150/100%)"), None);
    }

    #[test]
    fn pool_names() {
        assert!(valid_pool_name("default"));
        assert!(valid_pool_name("big-disk_2"));
        assert!(!valid_pool_name("-x"));
        assert!(!valid_pool_name("a/b"));
        assert!(!valid_pool_name(""));
    }

    #[test]
    fn job_status_roundtrip() {
        for s in [
            "queued",
            "running",
            "succeeded",
            "failed",
            "cancelled",
            "interrupted",
        ] {
            assert_eq!(JobStatus::parse(s).as_str(), s);
        }
        assert!(JobStatus::Succeeded.is_finished());
        assert!(!JobStatus::Running.is_finished());
    }
}
