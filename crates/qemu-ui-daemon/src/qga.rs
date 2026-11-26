//! QEMU guest agent client (M7-02).
//!
//! The agent listens on a virtio-serial port; QEMU exposes it on the unix
//! socket of chardev `qui-ga0`. The protocol is QMP-like JSON lines without
//! a greeting. Each call opens its own connection, resynchronises with
//! `guest-sync` (a previous client may have left a reply unread) and runs one
//! command under a timeout, so a guest without a running agent only ever
//! costs that timeout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qemu_ui_core::guest::{AGENT_CHARDEV, GuestInfo, parse_interfaces};
use qemu_ui_qmp::Qmp;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long one agent call may take.
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// The agent's socket, when the running VM has a `qui-ga0` unix chardev.
pub async fn socket(qmp: &Qmp) -> Option<PathBuf> {
    let v = qmp.execute("query-chardev", None).await.ok()?;
    let dev = v.as_array()?.iter().find(|c| c["label"] == AGENT_CHARDEV)?;
    // e.g. "unix:/run/qui/vm/ga.sock,server=on", with a "disconnected:"
    // prefix while no client is attached.
    let f = dev["filename"].as_str()?;
    let f = f
        .strip_prefix("disconnected:")
        .unwrap_or(f)
        .strip_prefix("unix:")?;
    Some(PathBuf::from(f.split(",server").next().unwrap_or(f)))
}

/// Run `command` in the guest agent. `expect_reply: false` is for commands
/// that answer only on failure (`guest-shutdown`): success is "no error
/// within a short wait".
pub async fn call(
    sock: &Path,
    command: &str,
    arguments: Option<Value>,
    expect_reply: bool,
    timeout: Duration,
) -> Result<Value> {
    let fut = async {
        let stream = qemu_ui_qmp::transport::connect_unix(sock)
            .await
            .context("connecting to the guest agent channel")?;
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        let token: u32 = rand::random::<u32>() & 0x7fff_ffff;
        let sync = json!({"execute": "guest-sync", "arguments": {"id": token}});
        w.write_all(format!("{sync}\n").as_bytes()).await?;
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("the guest agent closed the channel"))?;
            if serde_json::from_str::<Value>(&line).is_ok_and(|v| v["return"] == token) {
                break;
            }
        }
        let mut req = json!({"execute": command});
        if let Some(a) = arguments {
            req["arguments"] = a;
        }
        w.write_all(format!("{req}\n").as_bytes()).await?;
        if !expect_reply {
            return match tokio::time::timeout(Duration::from_millis(500), lines.next_line()).await {
                Ok(Ok(Some(line))) => reply(&line),
                _ => Ok(Value::Null),
            };
        }
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("the guest agent closed the channel"))?;
        reply(&line)
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| {
        anyhow!("the guest agent did not answer (is qemu-guest-agent running in the guest?)")
    })?
}

fn reply(line: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(line)?;
    if let Some(e) = v.get("error") {
        bail!("guest agent: {}", e["desc"].as_str().unwrap_or("error"));
    }
    Ok(v["return"].clone())
}

/// Everything the agent tells about the guest; `connected: false` with the
/// reason when it does not answer.
pub async fn info(sock: &Path) -> GuestInfo {
    let mut g = GuestInfo {
        agent: true,
        running: true,
        ..Default::default()
    };
    if let Err(e) = call(sock, "guest-ping", None, true, TIMEOUT).await {
        g.error = Some(format!("{e:#}"));
        return g;
    }
    g.connected = true;
    // Each of these may be disabled in the guest's agent; missing ones stay empty.
    if let Ok(v) = call(sock, "guest-get-host-name", None, true, TIMEOUT).await {
        g.hostname = v["host-name"].as_str().map(String::from);
    }
    if let Ok(v) = call(sock, "guest-get-osinfo", None, true, TIMEOUT).await {
        g.os_name = v["pretty-name"]
            .as_str()
            .or(v["name"].as_str())
            .map(String::from);
        g.os_version = v["version"].as_str().map(String::from);
        g.kernel = v["kernel-release"].as_str().map(String::from);
    }
    if let Ok(v) = call(sock, "guest-network-get-interfaces", None, true, TIMEOUT).await {
        g.interfaces = parse_interfaces(&v);
    }
    if let Ok(v) = call(sock, "guest-fsfreeze-status", None, true, TIMEOUT).await {
        g.fsfreeze = v.as_str().map(String::from);
    }
    g
}

/// Ask the guest to power off through the agent.
pub async fn shutdown(sock: &Path) -> Result<()> {
    // Make sure someone is listening first: without an agent the command
    // would be swallowed and we would report success.
    call(sock, "guest-ping", None, true, TIMEOUT).await?;
    call(
        sock,
        "guest-shutdown",
        Some(json!({"mode": "powerdown"})),
        false,
        TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// Set the guest clock to the host's (after the host slept, X-02).
pub async fn set_time(sock: &Path) -> Result<()> {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("host clock before 1970: {e}"))?
        .as_nanos() as u64;
    call(
        sock,
        "guest-set-time",
        Some(json!({ "time": ns })),
        true,
        TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// Freeze the guest's file systems; returns how many were frozen.
pub async fn freeze(sock: &Path) -> Result<u64> {
    // Freezing waits for in-flight writes; allow more than a plain call.
    let v = call(
        sock,
        "guest-fsfreeze-freeze",
        None,
        true,
        Duration::from_secs(30),
    )
    .await?;
    Ok(v.as_u64().unwrap_or(0))
}

/// Thaw them again; safe to call when nothing is frozen.
pub async fn thaw(sock: &Path) -> Result<()> {
    call(
        sock,
        "guest-fsfreeze-thaw",
        None,
        true,
        Duration::from_secs(10),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    //! A fake agent on a unix socket: answers `guest-sync` and a few
    //! commands like qemu-ga would.
    use super::*;

    /// Accept connections on `path` and serve each like qemu-ga.
    fn spawn_fake_agent(path: &Path) {
        #[cfg(unix)]
        {
            let l = tokio::net::UnixListener::bind(path).unwrap();
            tokio::spawn(async move {
                while let Ok((s, _)) = l.accept().await {
                    tokio::spawn(serve_one(s));
                }
            });
        }
        #[cfg(windows)]
        {
            let l = uds_windows::UnixListener::bind(path).unwrap();
            let rt = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                while let Ok((s, _)) = l.accept() {
                    let _g = rt.enter();
                    let s = qemu_ui_qmp::transport::bridge(s).unwrap();
                    rt.spawn(serve_one(s));
                }
            });
        }
    }

    async fn serve_one<S>(stream: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        // A stale reply from an earlier client, which guest-sync must skip.
        w.write_all(b"{\"return\": {}}\n").await.unwrap();
        while let Ok(Some(line)) = lines.next_line().await {
            let v: Value = serde_json::from_str(&line).unwrap();
            let out = match v["execute"].as_str().unwrap() {
                "guest-sync" => json!({"return": v["arguments"]["id"]}),
                "guest-ping" => json!({"return": {}}),
                "guest-get-host-name" => json!({"return": {"host-name": "web1"}}),
                "guest-get-osinfo" => {
                    json!({"return": {"pretty-name": "Test OS 1", "version": "1", "kernel-release": "6.1"}})
                }
                "guest-network-get-interfaces" => json!({"return": [
                    {"name": "eth0", "hardware-address": "52:54:00:00:00:01",
                     "ip-addresses": [{"ip-address": "192.0.2.10", "prefix": 24}]}]}),
                "guest-fsfreeze-status" => json!({"return": "thawed"}),
                "guest-fsfreeze-freeze" => json!({"return": 2}),
                "guest-fsfreeze-thaw" => json!({"return": 2}),
                "guest-shutdown" => continue, // no reply on success
                _ => json!({"error": {"class": "CommandNotFound", "desc": "nope"}}),
            };
            w.write_all(format!("{out}\n").as_bytes()).await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn talks_to_a_fake_agent() {
        let dir = std::env::temp_dir().join(format!("qui-qga-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("ga.sock");
        spawn_fake_agent(&sock);
        let g = info(&sock).await;
        assert!(g.connected, "{g:?}");
        assert_eq!(g.hostname.as_deref(), Some("web1"));
        assert_eq!(g.os_name.as_deref(), Some("Test OS 1"));
        assert_eq!(g.interfaces[0].addresses, ["192.0.2.10/24"]);
        assert_eq!(g.fsfreeze.as_deref(), Some("thawed"));
        assert_eq!(freeze(&sock).await.unwrap(), 2);
        thaw(&sock).await.unwrap();
        shutdown(&sock).await.unwrap();
        let e = call(&sock, "guest-exec", None, true, TIMEOUT).await;
        assert!(e.unwrap_err().to_string().contains("nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_agent_is_reported_not_hung() {
        let sock = std::env::temp_dir().join(format!("qui-qga-none-{}.sock", std::process::id()));
        let g = info(&sock).await;
        assert!(!g.connected);
        assert!(g.error.is_some());
    }
}
