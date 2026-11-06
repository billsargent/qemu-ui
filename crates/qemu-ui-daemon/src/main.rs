//! qemu-uid: the qemu-ui daemon (plan.md §3).

mod api;
mod api_snapshots;
mod api_storage;
mod api_vms;
mod consoles;
mod discovery;
mod guard;
mod image_ops;
mod images;
mod jobs;
mod probe;
mod procs;
mod scan;
mod store;
mod supervisor;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "qemu-uid", version, about = "qemu-ui daemon")]
struct Args {
    /// Address to listen on. Loopback only until LAN hardening (M6) ships.
    #[arg(long, env = "QEMU_UI_LISTEN", default_value = "127.0.0.1:7480")]
    listen: SocketAddr,

    /// Directory for the database, run directories and logs.
    #[arg(long, env = "QEMU_UI_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Extra directories to search for QEMU binaries (repeatable).
    #[arg(long = "qemu-dir")]
    qemu_dirs: Vec<PathBuf>,

    /// Directory of the built web UI (web/dist).
    #[arg(long, env = "QEMU_UI_WEB_DIR")]
    web_dir: Option<PathBuf>,

    /// Directory for per-VM sockets. Must be short: unix socket paths are
    /// limited to 108 bytes. Defaults to `<data-dir>/r`.
    #[arg(long, env = "QEMU_UI_RUN_DIR")]
    run_dir: Option<PathBuf>,

    /// Re-probe every binary at startup even if a cached snapshot exists.
    #[arg(long)]
    rescan: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    if !args.listen.ip().is_loopback() {
        bail!(
            "refusing to listen on {}: LAN exposure requires authentication and TLS, which arrive in \
             milestone M6. Use a loopback address.",
            args.listen
        );
    }

    let data_dir = match args.data_dir {
        Some(d) => d,
        None => dirs::data_local_dir()
            .context("no local data directory")?
            .join("qemu-ui"),
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let store = store::Store::open(&data_dir.join("qemu-ui.db"))?;

    let project_dirs = project_qemu_dirs();
    let scanner = scan::Scanner::new(store.clone(), project_dirs, args.qemu_dirs);
    scanner.start(args.rescan).await;

    let run_dir = args.run_dir.unwrap_or_else(|| data_dir.join("r"));
    let supervisor = supervisor::Supervisor::new(store, scanner.clone(), &data_dir, run_dir)?;
    // VMs kept running while the daemon was down: find them again.
    supervisor.reattach_all().await;

    let web_dir = args.web_dir.or_else(|| {
        let d = std::env::current_dir().ok()?.join("web").join("dist");
        d.is_dir().then_some(d)
    });
    let token = guard::load_or_create_token(&data_dir)?;
    let app = api::router(api::App::new(scanner, supervisor, token), web_dir);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!("listening on http://{}", args.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Project-local QEMU: `$QEMU_UI_QEMU_DIR` (set by scripts/env.sh) and
/// `./.tools/qemu` relative to the working directory.
fn project_qemu_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("QEMU_UI_QEMU_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let d = cwd.join(".tools").join("qemu");
        if d.is_dir() {
            v.push(d);
        }
    }
    v
}
