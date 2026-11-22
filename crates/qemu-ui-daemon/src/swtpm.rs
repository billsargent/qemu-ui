//! swtpm sidecars (M7-07): one emulated TPM 2.0 per VM with a TPM.
//!
//! `swtpm socket` keeps the TPM's state in the VM's directory (so it
//! survives restarts and goes away with the VM) and serves QEMU's `-tpmdev
//! emulator` on a unix socket in the run directory. With `--terminate` it
//! exits when QEMU disconnects; like passt it is recorded as a run sidecar,
//! detached so it survives a daemon restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::procs::{self, ProcId};

/// Locate `swtpm`: `QEMU_UI_SWTPM`, then `PATH`, then the usual install
/// locations. There is no mainstream Windows build.
pub fn find_swtpm() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("QEMU_UI_SWTPM").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let exe = if cfg!(windows) { "swtpm.exe" } else { "swtpm" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect())
        .unwrap_or_default();
    for d in ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        candidates.push(Path::new(d).join(exe));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Why no TPM can be attached on this host for a binary, if none can.
pub fn unavailable_reason(
    caps: &qemu_ui_core::caps::BinaryCaps,
    swtpm: Option<&Path>,
) -> Option<String> {
    if let Some(why) = qemu_ui_core::compile::tpm_unsupported_reason(caps) {
        return Some(why);
    }
    swtpm.is_none().then(|| {
        "swtpm is not installed; install the swtpm package (or set QEMU_UI_SWTPM)".to_string()
    })
}

/// `swtpm socket` arguments for a TPM 2.0 with state in `state` serving
/// `socket`.
pub fn args(state: &Path, socket: &str) -> Vec<String> {
    vec![
        "socket".into(),
        "--tpm2".into(),
        "--tpmstate".into(),
        format!("dir={},mode=0600", state.display()),
        "--ctrl".into(),
        format!("type=unixio,path={socket},mode=0600"),
        "--terminate".into(),
    ]
}

/// Start swtpm and wait for its socket.
pub async fn start(bin: Option<&Path>, state: &Path, socket: &str, log: &Path) -> Result<ProcId> {
    let Some(bin) = bin else {
        bail!(
            "this VM has a TPM, but swtpm is not installed; install the swtpm package (or set QEMU_UI_SWTPM)"
        );
    };
    std::fs::create_dir_all(state)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(socket);
    let (child, id) = procs::spawn_detached(&bin.display().to_string(), &args(state, socket), log)?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    let sock = PathBuf::from(socket);
    for _ in 0..100 {
        if sock.exists() {
            return Ok(id);
        }
        if !procs::alive(id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    procs::terminate(id, true);
    bail!("swtpm did not start: {}", procs::log_tail(log, 5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_keep_state_private_and_exit_with_qemu() {
        let a = args(Path::new("/data/vms/v1/tpm"), "/run/qui/v1/swtpm.sock");
        assert_eq!(a[0], "socket");
        assert!(a.contains(&"--tpm2".to_string()));
        assert!(a.contains(&"--terminate".to_string()));
        assert!(a.contains(&"dir=/data/vms/v1/tpm,mode=0600".to_string()));
        assert!(a.contains(&"type=unixio,path=/run/qui/v1/swtpm.sock,mode=0600".to_string()));
    }

    /// A real swtpm (when installed) serves the socket QEMU will use.
    #[test]
    fn real_swtpm_starts_and_serves_its_socket() {
        let Some(bin) = find_swtpm() else {
            eprintln!("skipping: swtpm not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("qui-swtpm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("swtpm.sock").display().to_string();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let id = rt
            .block_on(start(
                Some(&bin),
                &dir.join("state"),
                &socket,
                &dir.join("swtpm.log"),
            ))
            .unwrap();
        assert!(Path::new(&socket).exists());
        procs::terminate(id, true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
