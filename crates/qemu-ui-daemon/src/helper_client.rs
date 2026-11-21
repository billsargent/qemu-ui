//! Client for the privileged helper (docs/design/privileged-helper.md).
//!
//! The daemon works without the helper; host disks are then unavailable and
//! the UI says why. Each request opens a short connection: hello (with the
//! shared secret when the helper requires one), the request, done.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::helper::{
    DiskExport, Envelope, HostDisk, PROTOCOL_VERSION, Request, Response, TapInfo, TapNetwork,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct HelperClient {
    pub socket: PathBuf,
    pub secret_file: Option<PathBuf>,
}

impl HelperClient {
    /// The helper's default locations for this platform.
    pub fn default_paths() -> HelperClient {
        if cfg!(windows) {
            let state = PathBuf::from(
                std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into()),
            )
            .join("qemu-ui")
            .join("helper");
            HelperClient {
                socket: state.join("helper.sock"),
                secret_file: Some(state.join("secret")),
            }
        } else {
            HelperClient {
                socket: PathBuf::from("/run/qemu-ui-helper/helper.sock"),
                secret_file: None,
            }
        }
    }

    /// Is a helper listening?
    pub fn present(&self) -> bool {
        self.socket.exists()
    }

    async fn call(&self, req: Request) -> Result<Response> {
        if !self.present() {
            bail!(
                "the qemu-ui helper is not running (no socket at {}); host disks need it",
                self.socket.display()
            );
        }
        let secret = match &self.secret_file {
            Some(p) => Some(read_secret(p)?),
            None => None,
        };
        let fut = async {
            let stream = qemu_ui_qmp::transport::connect_unix(&self.socket).await?;
            let (r, mut w) = tokio::io::split(stream);
            let mut lines = BufReader::new(r).lines();
            let mut send = async |id: u64, body: Request| -> Result<Response> {
                let mut line = serde_json::to_vec(&Envelope { id, body })?;
                line.push(b'\n');
                w.write_all(&line).await?;
                w.flush().await?;
                let reply = lines
                    .next_line()
                    .await?
                    .ok_or_else(|| anyhow!("the helper closed the connection"))?;
                let env: Envelope<Response> = serde_json::from_str(&reply)?;
                Ok(env.body)
            };
            match send(
                1,
                Request::Hello {
                    version: PROTOCOL_VERSION,
                    secret,
                },
            )
            .await?
            {
                Response::Hello { .. } => {}
                Response::Error { message } => bail!("helper refused the connection: {message}"),
                other => bail!("unexpected helper reply: {other:?}"),
            }
            send(2, req).await
        };
        match tokio::time::timeout(TIMEOUT, fut)
            .await
            .context("the helper did not answer")??
        {
            Response::Error { message } => Err(anyhow!(message)),
            r => Ok(r),
        }
    }

    pub async fn list_disks(&self) -> Result<Vec<HostDisk>> {
        match self.call(Request::ListDisks).await? {
            Response::Disks { disks } => Ok(disks),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    pub async fn export(&self, disk_id: &str, writable: bool) -> Result<DiskExport> {
        match self
            .call(Request::ExportDisk {
                disk_id: disk_id.into(),
                writable,
            })
            .await?
        {
            Response::Exported { export } => Ok(export),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    /// Linux: put a running QEMU in its own cgroup with these limits (M6-05).
    pub async fn apply_vm_limits(
        &self,
        vm_id: &str,
        pid: u32,
        limits: &qemu_ui_core::limits::ResourceLimits,
    ) -> Result<String> {
        match self
            .call(Request::ApplyVmLimits {
                vm_id: vm_id.into(),
                pid,
                cpu_max_percent: limits.cpu_max_percent,
                cpu_weight: limits.cpu_weight,
                memory_max_mib: limits.memory_max_mib,
            })
            .await?
        {
            Response::LimitsApplied { cgroup } => Ok(cgroup),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    pub async fn release_vm_limits(&self, vm_id: &str) -> Result<()> {
        match self
            .call(Request::ReleaseVmLimits {
                vm_id: vm_id.into(),
            })
            .await?
        {
            Response::LimitsReleased => Ok(()),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    pub async fn unexport(&self, export_id: &str) -> Result<()> {
        match self
            .call(Request::Unexport {
                export_id: export_id.into(),
            })
            .await?
        {
            Response::Unexported => Ok(()),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    /// Linux: a tap for NIC `nic` of `vm_id` on a bridge (M3-05).
    pub async fn create_tap(
        &self,
        vm_id: &str,
        nic: u32,
        network: TapNetwork,
        multi_queue: bool,
    ) -> Result<TapInfo> {
        match self
            .call(Request::CreateTap {
                vm_id: vm_id.into(),
                nic,
                network,
                multi_queue,
            })
            .await?
        {
            Response::TapCreated { tap } => Ok(tap),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    pub async fn delete_tap(&self, ifname: &str) -> Result<()> {
        match self
            .call(Request::DeleteTap {
                ifname: ifname.into(),
            })
            .await?
        {
            Response::TapDeleted => Ok(()),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }

    /// Bridges the helper allows, and whether host-only is enabled.
    pub async fn network_policy(&self) -> Result<(Vec<String>, bool)> {
        match self.call(Request::NetworkPolicy).await? {
            Response::NetworkPolicy { bridges, host_only } => Ok((bridges, host_only)),
            other => bail!("unexpected helper reply: {other:?}"),
        }
    }
}

fn read_secret(p: &Path) -> Result<String> {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "reading the helper secret {} (installed alongside the helper)",
                p.display()
            )
        })
}
