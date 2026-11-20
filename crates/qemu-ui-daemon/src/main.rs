//! qemu-uid: the qemu-ui daemon (plan.md §3).

mod api;
mod api_disks;
mod api_expert;
mod api_fetch;
mod api_forwards;
mod api_guest_net;
mod api_hardware;
mod api_import;
mod api_nics;
mod api_rules;
mod api_snapshots;
mod api_storage;
mod api_upload;
mod api_vms;
mod audit;
mod auth;
mod consoles;
mod discovery;
mod display;
mod guard;
mod guest_net;
mod helper_client;
mod hostnet;
mod image_ops;
mod images;
mod jobs;
mod lint_matrix;
mod netswitch;
mod networks;
mod passt;
mod probe;
mod procs;
mod recording;
mod scan;
mod screens;
mod shares;
mod spice;
mod store;
mod supervisor;
mod usb;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "qemu-uid", version, about = "qemu-ui daemon")]
struct Args {
    /// Address to listen on. Anything beyond loopback needs first-run setup
    /// (the admin account) to be complete.
    #[arg(long, env = "QEMU_UI_LISTEN", default_value = "127.0.0.1:7480")]
    listen: SocketAddr,

    /// The daemon is served over TLS (directly or by a reverse proxy):
    /// session cookies are marked `Secure`.
    #[arg(long, env = "QEMU_UI_TLS")]
    tls: bool,

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

    /// The privileged helper's socket (default: the platform's standard path).
    #[arg(long, env = "QEMU_UI_HELPER_SOCKET")]
    helper_socket: Option<PathBuf>,

    /// File holding the helper's shared secret (Windows helpers require it).
    #[arg(long, env = "QEMU_UI_HELPER_SECRET")]
    helper_secret: Option<PathBuf>,

    /// Allow URL downloads from private network ranges (RFC 1918, CGNAT,
    /// IPv6 ULA), e.g. a NAS on the LAN. Loopback and link-local addresses
    /// (including cloud metadata) stay refused.
    #[arg(long, env = "QEMU_UI_ALLOW_PRIVATE_FETCH")]
    allow_private_fetch: bool,

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

    let data_dir = match args.data_dir {
        Some(d) => d,
        None => dirs::data_local_dir()
            .context("no local data directory")?
            .join("qemu-ui"),
    };
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let store = store::Store::open(&data_dir.join("qemu-ui.db"))?;
    let lan = !args.listen.ip().is_loopback();
    if lan {
        if store.admin()?.is_none() {
            bail!(
                "refusing to listen on {}: complete first-run setup (create the admin account) \
                 on a loopback address first, e.g. http://127.0.0.1:7480",
                args.listen
            );
        }
        if !args.tls {
            tracing::warn!(
                "listening on {} without TLS: passwords and session cookies cross the network \
                 in clear text",
                args.listen
            );
        }
    }
    let auth = auth::Auth::new(store.clone(), args.tls, lan);

    let project_dirs = project_qemu_dirs();
    let scanner = scan::Scanner::new(store.clone(), project_dirs, args.qemu_dirs);
    scanner.start(args.rescan).await;

    let run_dir = args.run_dir.unwrap_or_else(|| data_dir.join("r"));
    let supervisor = supervisor::Supervisor::new(store, scanner.clone(), &data_dir, run_dir)?;
    if args.helper_socket.is_some() || args.helper_secret.is_some() {
        let mut h = helper_client::HelperClient::default_paths();
        if let Some(s) = args.helper_socket {
            h.socket = s;
        }
        if let Some(s) = args.helper_secret {
            h.secret_file = Some(s);
        }
        supervisor.set_helper(h);
    }
    // VMs kept running while the daemon was down: find them again.
    supervisor.reattach_all().await;

    let web_dir = args.web_dir.or_else(|| {
        let d = std::env::current_dir().ok()?.join("web").join("dist");
        d.is_dir().then_some(d)
    });
    api_fetch::set_allow_private(args.allow_private_fetch);
    let token = guard::load_or_create_token(&data_dir)?;
    let app = api::router(
        api::App::with_auth(scanner, supervisor, token, auth),
        web_dir,
    );

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!("listening on http://{}", args.listen);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
