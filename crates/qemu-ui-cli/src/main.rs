//! qemu-ui: command-line client. Speaks the same HTTP API as the browser
//! (plan.md §3); full parity with the UI is M7-06.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "qemu-ui", version, about = "qemu-ui command-line client")]
struct Cli {
    /// Daemon base URL.
    #[arg(long, env = "QEMU_UI_URL", default_value = "http://127.0.0.1:7480")]
    url: String,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let base = format!("{}/api/v1", cli.url.trim_end_matches('/'));
    match cli.cmd {
        Cmd::Host { json } => {
            let v: Value = get(&format!("{base}/host/capabilities"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_summary(&v);
            }
        }
        Cmd::Rescan => {
            let v: Value = ureq::post(&format!("{base}/host/rescan?force=true"))
                .send_empty()
                .context("contacting the daemon")?
                .body_mut()
                .read_json()?;
            println!(
                "scan started ({} binaries)",
                v["total"].as_u64().unwrap_or(0)
            );
        }
    }
    Ok(())
}

fn get(url: &str) -> Result<Value> {
    ureq::get(url)
        .call()
        .with_context(|| format!("contacting the daemon at {url}; is qemu-uid running?"))?
        .body_mut()
        .read_json()
        .context("decoding response")
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
