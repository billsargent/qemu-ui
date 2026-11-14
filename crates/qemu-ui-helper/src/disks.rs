//! Host disk discovery. Parsing lives in `qemu_ui_core::helper` (pure,
//! tested with fixtures); this module only runs the platform tools.

use anyhow::{Context, Result, bail};
use qemu_ui_core::helper::HostDisk;

/// List the host's disks.
pub async fn list() -> Result<Vec<HostDisk>> {
    if cfg!(windows) {
        list_windows().await
    } else {
        list_linux().await
    }
}

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
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .context("timed out")?
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn list_windows() -> Result<Vec<HostDisk>> {
    const DISKS: &str = "Get-Disk | Select-Object Number,UniqueId,FriendlyName,BusType,IsSystem,IsBoot,\
                         IsOffline,IsReadOnly,Size | ConvertTo-Json -Compress";
    const PARTS: &str = "Get-Partition | Select-Object DiskNumber,PartitionNumber,DriveLetter | \
                         ConvertTo-Json -Compress";
    async fn ps(script: &str) -> Result<String> {
        run(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", script],
        )
        .await
    }
    let (disks, parts) = tokio::try_join!(ps(DISKS), ps(PARTS))?;
    qemu_ui_core::helper::parse_windows_disks(&disks, &parts).map_err(anyhow::Error::msg)
}

async fn list_linux() -> Result<Vec<HostDisk>> {
    let json = run(
        "lsblk",
        &[
            "--json",
            "--bytes",
            "-o",
            "NAME,PATH,SIZE,TYPE,TRAN,RM,RO,MODEL,MOUNTPOINTS",
        ],
    )
    .await?;
    let mut by_id = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/disk/by-id") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Ok(target) = std::fs::canonicalize(e.path()) {
                by_id.push((name, target.display().to_string()));
            }
        }
    }
    let mut disks = qemu_ui_core::helper::parse_lsblk(&json, &by_id).map_err(anyhow::Error::msg)?;
    // Devices held by device-mapper, MD or LVM are in use even when nothing
    // is mounted from them directly.
    for d in &mut disks {
        if let Ok(real) = std::fs::canonicalize(&d.device_path)
            && let Some(name) = real.file_name().and_then(|n| n.to_str())
            && let Ok(holders) = std::fs::read_dir(format!("/sys/block/{name}/holders"))
        {
            for h in holders.flatten() {
                d.in_use
                    .push(format!("held by {}", h.file_name().to_string_lossy()));
            }
        }
    }
    Ok(disks)
}
