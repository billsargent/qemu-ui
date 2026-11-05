//! Find QEMU binaries (plan.md §4.1).
//!
//! Search order: project-local `.tools/qemu` (and `QEMU_UI_QEMU_DIR`), then
//! directories from `--qemu-dir`, then `PATH`. The first occurrence of a
//! canonical path wins; side-by-side installs are separate entries.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use qemu_ui_core::arch;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    /// `None` for qemu-img.
    pub arch: Option<String>,
    pub source: &'static str,
    pub size: u64,
    /// Seconds since epoch; together with size, the cache key for the file hash.
    pub mtime: u64,
}

pub struct SearchDirs {
    pub dirs: Vec<(PathBuf, &'static str)>,
}

impl SearchDirs {
    pub fn new(project: &[PathBuf], configured: &[PathBuf]) -> Self {
        let mut dirs: Vec<(PathBuf, &'static str)> = Vec::new();
        dirs.extend(project.iter().map(|d| (d.clone(), "project")));
        dirs.extend(configured.iter().map(|d| (d.clone(), "config")));
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path).map(|d| (d, "path")));
        }
        SearchDirs { dirs }
    }
}

/// Scan all search dirs. Returns system emulators and qemu-img binaries.
pub fn discover(search: &SearchDirs) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (dir, source) in &search.dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        let set: HashSet<&str> = names.iter().map(String::as_str).collect();
        for name in &names {
            let is_img = name == "qemu-img" || name == "qemu-img.exe";
            let target = arch::target_from_filename(name);
            if !is_img && target.is_none() {
                continue;
            }
            if cfg!(windows) && !name.ends_with(".exe") {
                continue;
            }
            if let Some(t) = target
                && is_windowless_duplicate(name, t, &set)
            {
                continue;
            }
            let path = dir.join(name);
            let Ok(canon) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !seen.insert(canon) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            out.push(Candidate {
                arch: target.map(String::from),
                source,
                size: meta.len(),
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
                path,
            });
        }
    }
    out
}

/// The Windows build ships `qemu-system-x86_64w.exe` (no console window) next
/// to `qemu-system-x86_64.exe`. Treat the `w` variant as a duplicate.
fn is_windowless_duplicate(name: &str, target: &str, names: &HashSet<&str>) -> bool {
    let Some(base) = target.strip_suffix('w') else {
        return false;
    };
    let console = if name.ends_with(".exe") {
        format!("qemu-system-{base}.exe")
    } else {
        format!("qemu-system-{base}")
    };
    names.contains(console.as_str())
}

/// Directories that may hold firmware descriptors for binaries in `bin_dir`.
pub fn firmware_descriptor_dirs(bin_dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        bin_dir.join("share").join("firmware"), // upstream Windows build
        bin_dir
            .join("..")
            .join("share")
            .join("qemu")
            .join("firmware"), // /usr/bin -> /usr/share
    ];
    if cfg!(unix) {
        v.push(PathBuf::from("/etc/qemu/firmware"));
        v.push(PathBuf::from("/usr/share/qemu/firmware"));
    }
    v
}

/// Directories where firmware blobs may live, for resolving descriptor paths.
pub fn firmware_blob_dirs(bin_dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        bin_dir.join("share"),
        bin_dir.join("..").join("share").join("qemu"),
    ];
    if cfg!(unix) {
        for d in [
            "/usr/share/qemu",
            "/usr/share/OVMF",
            "/usr/share/AAVMF",
            "/usr/share/edk2",
        ] {
            v.push(PathBuf::from(d));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowless_variants_are_duplicates() {
        let names: HashSet<&str> = ["qemu-system-x86_64.exe", "qemu-system-x86_64w.exe"]
            .into_iter()
            .collect();
        assert!(is_windowless_duplicate(
            "qemu-system-x86_64w.exe",
            "x86_64w",
            &names
        ));
        assert!(!is_windowless_duplicate(
            "qemu-system-x86_64.exe",
            "x86_64",
            &names
        ));
        // A hypothetical arch ending in `w` with no console twin is kept.
        assert!(!is_windowless_duplicate(
            "qemu-system-foow.exe",
            "foow",
            &names
        ));
    }
}
