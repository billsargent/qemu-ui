//! Host side of tap-backed networks (M3-05/06): what the host offers
//! (`GET /host/net-adapters`), TAP-Windows adapters, and the neighbour table
//! used to find bridged guests' addresses. Parsing lives in core.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use qemu_ui_core::guest_net::{
    LINUX_NEIGH_CMD, Neighbor, WINDOWS_NEIGH_PS, parse_ip_neigh, parse_net_neighbor_json,
};
use qemu_ui_core::hostnet::{HostNetAdapter, HostNetInfo, WINDOWS_ADAPTERS_PS, parse_net_adapters};

use crate::api::App;

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    // A null stdin: PowerShell otherwise waits on an inherited one.
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .context("timed out")?
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn powershell(script: &str) -> Result<String> {
    run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .await
}

/// Windows network adapters, TAP-Windows ones first (empty elsewhere).
pub async fn adapters() -> Result<Vec<HostNetAdapter>> {
    if !cfg!(windows) {
        return Ok(vec![]);
    }
    parse_net_adapters(&powershell(WINDOWS_ADAPTERS_PS).await?).map_err(anyhow::Error::msg)
}

/// The host's neighbour (ARP/NDP) table.
pub async fn neighbors() -> Result<Vec<Neighbor>> {
    if cfg!(windows) {
        Ok(parse_net_neighbor_json(
            &powershell(WINDOWS_NEIGH_PS).await?,
        ))
    } else {
        let (prog, args) = LINUX_NEIGH_CMD.split_first().expect("non-empty");
        Ok(parse_ip_neigh(&run(prog, args).await?))
    }
}

/// `/dev/vhost-net` can be opened: QEMU may use the in-kernel data path.
pub fn vhost_net_usable() -> bool {
    cfg!(target_os = "linux")
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vhost-net")
            .is_ok()
}

pub const TAP_WINDOWS_SETUP: &str = "No TAP-Windows adapter is installed. Install the TAP-Windows \
driver (tap-windows6, shipped with OpenVPN), then, as an administrator, open Network Connections, \
select the TAP adapter and your physical adapter, and choose \"Bridge Connections\" to put guests \
on your LAN (or leave it unbridged for a host-only link). Then create a TAP network naming the \
adapter.";

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/host/net-adapters", get(info))
}

async fn info(State(app): State<Arc<App>>) -> Json<HostNetInfo> {
    let os = std::env::consts::OS.to_string();
    if cfg!(windows) {
        let (adapters, reason) = match adapters().await {
            Ok(a) if a.iter().any(|x| x.is_tap) => (a, None),
            Ok(a) => (a, Some(TAP_WINDOWS_SETUP.to_string())),
            Err(e) => (vec![], Some(format!("listing network adapters: {e:#}"))),
        };
        return Json(HostNetInfo {
            os,
            adapters,
            helper: false,
            bridges: vec![],
            host_only: false,
            reason,
        });
    }
    let helper = app.supervisor.helper();
    let (helper_ok, bridges, host_only, reason) = if cfg!(target_os = "linux") {
        match helper.network_policy().await {
            Ok((b, h)) => {
                let reason = (b.is_empty() && !h).then(|| {
                    "the qemu-ui helper allows no bridges and no host-only network; an administrator \
                     must add [network] bridges or [network.host_only] to helper.toml"
                        .to_string()
                });
                (true, b, h, reason)
            }
            Err(e) => (false, vec![], false, Some(format!("{e:#}"))),
        }
    } else {
        (
            false,
            vec![],
            false,
            Some("tap networks need a Linux or Windows host".into()),
        )
    };
    Json(HostNetInfo {
        os,
        adapters: vec![],
        helper: helper_ok,
        bridges,
        host_only,
        reason,
    })
}
