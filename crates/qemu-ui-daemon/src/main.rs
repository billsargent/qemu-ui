//! qemu-uid: the qemu-ui daemon (plan.md §3).

mod api;
mod discovery;
mod probe;
mod scan;
mod store;

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
    let scanner = scan::Scanner::new(store, project_dirs, args.qemu_dirs);
    scanner.start(args.rescan).await;

    let web_dir = args.web_dir.or_else(|| {
        let d = std::env::current_dir().ok()?.join("web").join("dist");
        d.is_dir().then_some(d)
    });
    let app = api::router(api::App::new(scanner), web_dir);

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
