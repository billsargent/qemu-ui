//! qemu-ui: command-line client. Speaks the same HTTP API as the browser
//! (plan.md §3); full parity with the UI is M7-06.
//!
//! Authenticates with the local API token the daemon writes to its data
//! directory (`api-token`), or `QEMU_UI_TOKEN`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "qemu-ui", version, about = "qemu-ui command-line client")]
struct Cli {
    /// Daemon base URL.
    #[arg(long, env = "QEMU_UI_URL", default_value = "http://127.0.0.1:7480")]
    url: String,
    /// Daemon data directory, to find the API token.
    #[arg(long, env = "QEMU_UI_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// API token (overrides the token file).
    #[arg(long, env = "QEMU_UI_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what this host's QEMU installation can do.
    Host {
        /// Print the raw JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Re-probe every QEMU binary.
    Rescan,
    /// List VMs.
    Vms,
    /// Start a VM (by name or id).
    Start { vm: String },
    /// Ask a VM to shut down (ACPI power button).
    Stop { vm: String },
    /// Stop a VM immediately.
    ForceStop { vm: String },
    /// Pause a VM.
    Pause { vm: String },
    /// Resume a paused VM.
    Resume { vm: String },
}

struct Client {
    base: String,
    token: String,
}

impl Client {
    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut res = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .with_context(|| format!("contacting the daemon at {url}; is qemu-uid running?"))?;
        check(res.status().as_u16(), res.body_mut().read_json()?)
    }

    fn post(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let mut res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .config()
            .http_status_as_error(false)
            .build()
            .send_empty()
            .with_context(|| format!("contacting the daemon at {url}; is qemu-uid running?"))?;
        check(
            res.status().as_u16(),
            res.body_mut().read_json().unwrap_or(Value::Null),
        )
    }

    /// Resolve a VM name to its id (ids pass through).
    fn vm_id(&self, vm: &str) -> Result<String> {
        let vms = self.get("/vms")?;
        let list = vms.as_array().cloned().unwrap_or_default();
        list.iter()
            .find(|v| v["id"] == vm || v["name"] == vm)
            .and_then(|v| v["id"].as_str().map(String::from))
            .with_context(|| format!("no VM named '{vm}'"))
    }
}

fn check(status: u16, body: Value) -> Result<Value> {
    if status >= 400 {
        let msg = body["error"]
            .as_str()
            .unwrap_or("request failed")
            .to_string();
        let diags: Vec<String> = body["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                format!(
                    "  {}: {}",
                    d["field"].as_str().unwrap_or(""),
                    d["message"].as_str().unwrap_or("")
                )
            })
            .collect();
        bail!(
            "{msg} (HTTP {status}){}{}",
            if diags.is_empty() { "" } else { "\n" },
            diags.join("\n")
        );
    }
    Ok(body)
}

fn token(cli: &Cli) -> Result<String> {
    if let Some(t) = &cli.token {
        return Ok(t.clone());
    }
    let dir = match &cli.data_dir {
        Some(d) => d.clone(),
        None => dirs::data_local_dir()
            .context("no local data directory")?
            .join("qemu-ui"),
    };
    let path = dir.join("api-token");
    let t = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading the API token from {}; pass --data-dir (or QEMU_UI_DATA_DIR) if the daemon uses another \
             data directory",
            path.display()
        )
    })?;
    Ok(t.trim().to_string())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client {
        base: format!("{}/api/v1", cli.url.trim_end_matches('/')),
        token: token(&cli)?,
    };
    match &cli.cmd {
        Cmd::Host { json } => {
            let v = client.get("/host/capabilities")?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_summary(&v);
            }
        }
        Cmd::Rescan => {
            let v = client.post("/host/rescan?force=true")?;
            println!(
                "scan started ({} binaries)",
                v["total"].as_u64().unwrap_or(0)
            );
        }
        Cmd::Vms => {
            for v in client.get("/vms")?.as_array().into_iter().flatten() {
                let reason = v["pause_reason"]
                    .as_str()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                println!(
                    "{:<10} {:<24} {:<10} {}{}",
                    v["id"].as_str().unwrap_or("?"),
                    v["name"].as_str().unwrap_or("?"),
                    v["arch"].as_str().unwrap_or("?"),
                    v["state"].as_str().unwrap_or("?"),
                    reason
                );
            }
        }
        Cmd::Start { vm }
        | Cmd::Stop { vm }
        | Cmd::ForceStop { vm }
        | Cmd::Pause { vm }
        | Cmd::Resume { vm } => {
            let action = match &cli.cmd {
                Cmd::Start { .. } => "start",
                Cmd::Stop { .. } => "stop",
                Cmd::ForceStop { .. } => "force-stop",
                Cmd::Pause { .. } => "pause",
                _ => "resume",
            };
            let id = client.vm_id(vm)?;
            let v = client.post(&format!("/vms/{id}/actions/{action}"))?;
            println!(
                "{}: {}",
                v["name"].as_str().unwrap_or(vm),
                v["state"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

fn print_summary(v: &Value) {
    let scan = &v["scan"];
    if scan["running"].as_bool() == Some(true) {
        println!(
            "scan in progress: {}/{}",
            scan["done"].as_u64().unwrap_or(0),
            scan["total"].as_u64().unwrap_or(0)
        );
    }
    let Some(caps) = v["caps"].as_object() else {
        println!("no capability data yet");
        return;
    };
    println!(
        "host: {} {}",
        caps["host"]["os"].as_str().unwrap_or("?"),
        caps["host"]["arch"].as_str().unwrap_or("?")
    );
    for b in caps["binaries"].as_array().into_iter().flatten() {
        let accel = b["accels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["available"].as_bool() == Some(true))
            .filter_map(|a| a["name"].as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "  {:<14} {:<8} {:>4} machines  accel: {}",
            b["arch"].as_str().unwrap_or("?"),
            b["binary"]["version"].as_str().unwrap_or("?"),
            b["machines"].as_array().map_or(0, Vec::len),
            accel
        );
    }
    for f in caps["failed"].as_array().into_iter().flatten() {
        println!(
            "  FAILED {}: {}",
            f["binary"]["path"].as_str().unwrap_or("?"),
            f["error"].as_str().unwrap_or("?")
        );
    }
}
