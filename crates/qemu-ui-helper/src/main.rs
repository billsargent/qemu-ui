//! qemu-ui-helper: the privileged helper (docs/design/privileged-helper.md).
//!
//! Runs with privileges (Linux: root via a system unit; Windows: an elevated
//! service) and accepts a closed set of requests from the unprivileged
//! daemon over an authenticated AF_UNIX socket.

mod disks;
mod exports;
mod server;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use qemu_ui_core::helper::HelperConfig;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "qemu-ui-helper", version, about = "qemu-ui privileged helper")]
struct Args {
    /// Administrator-owned configuration (helper.toml).
    #[arg(long, env = "QEMU_UI_HELPER_CONFIG")]
    config: Option<PathBuf>,
    /// State directory: control socket, export sockets, logs.
    #[arg(long, env = "QEMU_UI_HELPER_STATE")]
    state_dir: Option<PathBuf>,
    /// qemu-storage-daemon binary (default: searched in QEMU_UI_QEMU_DIR and PATH).
    #[arg(long)]
    qemu_storage_daemon: Option<PathBuf>,
}

fn default_config() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into()))
            .join("qemu-ui")
            .join("helper.toml")
    } else {
        PathBuf::from("/etc/qemu-ui/helper.toml")
    }
}

fn default_state() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into()))
            .join("qemu-ui")
            .join("helper")
    } else {
        PathBuf::from("/run/qemu-ui-helper")
    }
}

fn find_qsd() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "qemu-storage-daemon.exe"
    } else {
        "qemu-storage-daemon"
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("QEMU_UI_QEMU_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Resolve a user name to its uid from /etc/passwd (no libc calls).
#[cfg_attr(not(unix), allow(dead_code))]
fn uid_of(user: &str) -> Option<u32> {
    std::fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|l| {
            let mut f = l.split(':');
            (f.next()? == user).then(|| f.nth(1)?.parse().ok())?
        })
}

#[cfg_attr(unix, allow(dead_code))]
fn load_or_create_secret(dir: &Path) -> Result<String> {
    let path = dir.join("secret");
    if let Ok(s) = std::fs::read_to_string(&path)
        && s.trim().len() >= 32
    {
        return Ok(s.trim().to_string());
    }
    let s: String = (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    // The installer restricts this directory's ACL to SYSTEM, Administrators
    // and the daemon's account (M6-09).
    std::fs::write(&path, &s)?;
    Ok(s)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(default_config);
    let config: HelperConfig = match std::fs::read_to_string(&config_path) {
        Ok(s) => {
            toml::from_str(&s).with_context(|| format!("parsing {}", config_path.display()))?
        }
        Err(_) => {
            tracing::warn!(
                "no configuration at {}; nothing can be exported",
                config_path.display()
            );
            HelperConfig::default()
        }
    };
    let state = args.state_dir.unwrap_or_else(default_state);
    std::fs::create_dir_all(&state)?;
    let qsd = args
        .qemu_storage_daemon
        .or_else(find_qsd)
        .context("qemu-storage-daemon not found")?;
    let exports = exports::Exports::open(&state.join("exports"), &qsd)?;

    #[cfg(unix)]
    let (secret, allowed_uid) = {
        let uid = match &config.daemon_user {
            Some(u) => Some(uid_of(u).with_context(|| format!("unknown daemon_user '{u}'"))?),
            None => None,
        };
        (None, uid)
    };
    #[cfg(windows)]
    let (secret, allowed_uid) = (Some(load_or_create_secret(&state)?), None);

    let helper = Arc::new(server::Helper::new(
        config,
        exports,
        secret,
        allowed_uid,
        state.join("audit.log"),
    ));
    let socket = state.join("helper.sock");
    qemu_ui_qmp::transport::check_unix_path(&socket)?;
    let _ = std::fs::remove_file(&socket);
    tracing::info!("listening on {}", socket.display());
    serve(helper, &socket).await
}

#[cfg(unix)]
async fn serve(helper: Arc<server::Helper>, socket: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let listener = tokio::net::UnixListener::bind(socket)?;
    // Only the owner (root) and, via chown, the daemon user may connect.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    if let Some(uid) = helper.allowed_uid {
        std::os::unix::fs::chown(socket, Some(uid), None)?;
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let uid = stream.peer_cred().ok().map(|c| c.uid());
        let h = helper.clone();
        tokio::spawn(async move {
            if let Err(e) = h.serve(stream, server::Peer { uid }).await {
                tracing::warn!("connection: {e:#}");
            }
        });
    }
}

#[cfg(windows)]
async fn serve(helper: Arc<server::Helper>, socket: &Path) -> Result<()> {
    let listener = uds_windows::UnixListener::bind(socket)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<uds_windows::UnixStream>(16);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(s) => {
                    if tx.blocking_send(s).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("accept: {e}"),
            }
        }
    });
    while let Some(s) = rx.recv().await {
        let stream = qemu_ui_qmp::transport::bridge(s)?;
        let h = helper.clone();
        tokio::spawn(async move {
            if let Err(e) = h.serve(stream, server::Peer { uid: None }).await {
                tracing::warn!("connection: {e:#}");
            }
        });
    }
    anyhow::bail!("listener stopped")
}
