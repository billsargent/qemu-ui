//! qemu-uid: the qemu-ui daemon (plan.md §3).

mod api;
mod api_disks;
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
mod server_config;
mod shares;
mod spice;
mod store;
mod supervisor;
mod tls;
mod usb;

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

    /// HTTPS policy: `auto` serves HTTPS beyond loopback and plain HTTP on
    /// loopback; `on` always; `off` only on loopback.
    #[arg(long, env = "QEMU_UI_TLS", value_enum, default_value = "auto")]
    tls: server_config::TlsMode,

    /// Your own certificate chain (PEM, server certificate first) instead
    /// of the generated one. Needs --tls-key.
    #[arg(long, env = "QEMU_UI_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// Private key (PEM) for --tls-cert.
    #[arg(long, env = "QEMU_UI_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
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
    audit::init(
        store.clone(),
        qemu_ui_core::security::AuditRetention::default(),
    );

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
    let use_tls = server_config::want_tls(args.tls, args.listen).map_err(anyhow::Error::msg)?;
    server_config::set(server_config::ServerConfig {
        listen: args.listen,
        tls: use_tls,
    });
    let tls = if use_tls {
        let source = match (args.tls_cert, args.tls_key) {
            (Some(cert), Some(key)) => tls::Source::Provided { cert, key },
            _ => tls::Source::Generated {
                dir: data_dir.join("tls"),
            },
        };
        let t = tls::Tls::new(source, tls::default_names(args.listen))?;
        t.install();
        tls::reload_on_sighup(t.clone());
        Some(t)
    } else {
        None
    };
    let mut app = api::router(api::App::new(scanner, supervisor, token), web_dir);
    if tls.is_some() {
        app = app.layer(axum::middleware::from_fn(tls::hsts));
    }

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    audit::record(
        "daemon",
        "daemon.start",
        "daemon",
        serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "tls": use_tls }),
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match tls {
        Some(t) => {
            tracing::info!("listening on https://{}", args.listen);
            tls::serve(listener, app, t, shutdown).await?;
        }
        None => {
            tracing::info!("listening on http://{}", args.listen);
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown)
            .await?;
        }
    }
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
