//! qemu-ui: command-line client. Speaks the same HTTP API as the browser
//! (plan.md §3, M7-06).
//!
//! Authenticates with an API token: `--token` / `QEMU_UI_TOKEN` (create one
//! with `qemu-ui tokens create`, or on the System page), else the local
//! bootstrap token in the daemon's data directory (this machine only).
//! For `https://` URLs it trusts the daemon's CA from `--ca-cert`, or
//! `<data-dir>/tls/ca.pem` when present, else the platform's roots.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

mod apply;
mod client;
mod service;

use client::{Client, Trust};

#[derive(Parser)]
#[command(name = "qemu-ui", version, about = "qemu-ui command-line client")]
struct Cli {
    /// Daemon base URL.
    #[arg(long, env = "QEMU_UI_URL", default_value = "http://127.0.0.1:7480")]
    url: String,
    /// Daemon data directory, to find the local token and CA.
    #[arg(long, env = "QEMU_UI_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// API token (overrides the token file).
    #[arg(long, env = "QEMU_UI_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// CA certificate (PEM) to trust for https URLs.
    #[arg(long, env = "QEMU_UI_CA_CERT")]
    ca_cert: Option<PathBuf>,
    /// Print raw JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what this host's QEMU installation can do.
    Host,
    /// Re-probe every QEMU binary.
    Rescan,
    /// List VMs.
    Vms,
    /// Work with one VM.
    Vm {
        #[command(subcommand)]
        action: VmAction,
    },
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
    /// Reset a VM (hard reboot).
    Reset { vm: String },
    /// Networks.
    Networks {
        #[command(subcommand)]
        action: Option<NetAction>,
    },
    /// Snapshots of a VM.
    Snapshots {
        #[command(subcommand)]
        action: SnapAction,
    },
    /// Storage pools.
    Pools,
    /// Backup bundles (a VM's configuration, disks, UEFI vars and TPM state).
    Backups {
        #[command(subcommand)]
        action: Option<BackupAction>,
    },
    /// Background jobs.
    Jobs {
        #[command(subcommand)]
        action: Option<JobAction>,
    },
    /// API tokens.
    Tokens {
        #[command(subcommand)]
        action: Option<TokenAction>,
    },
    /// Recent audit log entries.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Write networks and VMs as a manifest (config-as-code).
    Export {
        /// Output file (default: stdout). A `.toml` name writes TOML.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Write TOML to stdout.
        #[arg(long)]
        toml: bool,
    },
    /// Make the daemon match a manifest (JSON or `.toml`).
    Apply {
        file: PathBuf,
        /// Show the plan without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Also delete networks and VMs the manifest doesn't list.
        #[arg(long)]
        prune: bool,
    },
    /// Install the daemon as a system service (systemd / Windows service).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum VmAction {
    /// Show a VM's state and spec.
    Show { vm: String },
    /// Create a VM from a spec file (JSON).
    Create { file: PathBuf },
    /// Replace a VM's spec from a file (JSON).
    Update { vm: String, file: PathBuf },
    /// Delete a VM.
    Delete { vm: String },
    /// Clone a VM.
    Clone {
        vm: String,
        name: String,
        /// Full copies instead of linked overlays.
        #[arg(long)]
        full: bool,
    },
    /// Print the QEMU command line the VM would start with.
    Argv { vm: String },
    /// Print the serial log.
    SerialLog { vm: String },
}

#[derive(Subcommand)]
enum NetAction {
    /// List networks (default).
    List,
    /// Delete a network (by name or id).
    Delete { network: String },
}

#[derive(Subcommand)]
enum SnapAction {
    /// List a VM's snapshots.
    List { vm: String },
    /// Take a snapshot.
    Take {
        vm: String,
        name: String,
        /// Include RAM (the VM must be running).
        #[arg(long)]
        memory: bool,
        /// External (overlay) snapshot.
        #[arg(long)]
        external: bool,
    },
    /// Revert to a snapshot (by name or id).
    Revert {
        vm: String,
        snapshot: String,
        /// Skip the safety snapshot of the current state.
        #[arg(long)]
        no_safety: bool,
    },
    /// Delete a snapshot (by name or id).
    Delete { vm: String, snapshot: String },
}

#[derive(Subcommand)]
enum BackupAction {
    /// List bundles (default).
    List,
    /// Export a VM to a bundle (a job; running VMs too).
    Export {
        vm: String,
        /// Keep qcow2 images' internal snapshots (stopped VMs).
        #[arg(long)]
        snapshots: bool,
    },
    /// Import a bundle as a new VM.
    Import {
        bundle: String,
        name: String,
        #[arg(long, default_value = "default")]
        pool: String,
    },
    /// Delete a bundle.
    Delete { bundle: String },
}

#[derive(Subcommand)]
enum JobAction {
    /// List jobs (default).
    List,
    /// Cancel a job.
    Cancel { id: String },
}

#[derive(Subcommand)]
enum TokenAction {
    /// List tokens (default).
    List,
    /// Create a token; it is printed once.
    Create { name: String },
    /// Revoke a token.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Register and start the service (needs root / an elevated shell).
    Install(InstallArgs),
    /// Stop and remove the service. Running VMs keep running; data is kept.
    Uninstall {
        /// Print the steps without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the service's state.
    Status,
}

#[derive(clap::Args)]
struct InstallArgs {
    /// Print the steps (and unit files) without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Linux: system user for the daemon. Windows: service account
    /// (default: the virtual account `NT SERVICE\qemu-ui`; `LocalSystem` to opt out).
    #[arg(long)]
    account: Option<String>,
    /// Data directory (default: /var/lib/qemu-ui, or %ProgramData%\qemu-ui).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7480")]
    listen: String,
    /// The daemon binary (default: `qemu-uid` next to this program).
    #[arg(long)]
    daemon_bin: Option<PathBuf>,
    /// Project-local QEMU (default: $QEMU_UI_QEMU_DIR, or `.tools/qemu` above this program).
    #[arg(long)]
    qemu_dir: Option<PathBuf>,
    /// Built web UI (default: `web/dist` above this program).
    #[arg(long)]
    web_dir: Option<PathBuf>,
    /// Linux: don't install the privileged helper unit.
    #[arg(long)]
    no_helper: bool,
}

fn data_dir(cli: &Cli) -> Option<PathBuf> {
    cli.data_dir
        .clone()
        .or_else(|| dirs::data_local_dir().map(|d| d.join("qemu-ui")))
}

fn token(cli: &Cli) -> Result<String> {
    if let Some(t) = &cli.token {
        return Ok(t.clone());
    }
    let path = data_dir(cli)
        .context("no local data directory; pass --token")?
        .join("api-token");
    let t = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading the API token from {}; pass --token (or QEMU_UI_TOKEN), or --data-dir if the daemon \
             uses another data directory",
            path.display()
        )
    })?;
    Ok(t.trim().to_string())
}

fn ca_path(cli: &Cli) -> Option<PathBuf> {
    if let Some(p) = &cli.ca_cert {
        return Some(p.clone());
    }
    if !cli.url.starts_with("https://") {
        return None;
    }
    data_dir(cli)
        .map(|d| d.join("tls").join("ca.pem"))
        .filter(|p| p.is_file())
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("-")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Service { action } = &cli.cmd {
        return service_cmd(action);
    }
    let ca = ca_path(&cli);
    let trust = ca.as_deref().map_or(Trust::Default, Trust::Ca);
    let c = Client::new(&cli.url, token(&cli)?, trust)?;
    run(&cli, &c)
}

fn run(cli: &Cli, c: &Client) -> Result<()> {
    let json = cli.json;
    match &cli.cmd {
        Cmd::Service { .. } => unreachable!("handled before connecting"),
        Cmd::Host => {
            let v = c.get("/host/capabilities")?;
            if json {
                print_json(&v)?;
            } else {
                print_summary(&v);
            }
        }
        Cmd::Rescan => {
            let v = c.post("/host/rescan?force=true", None)?;
            println!(
                "scan started ({} binaries)",
                v["total"].as_u64().unwrap_or(0)
            );
        }
        Cmd::Vms => {
            let v = c.get("/vms")?;
            if json {
                return print_json(&v);
            }
            for v in v.as_array().into_iter().flatten() {
                let reason = v["pause_reason"]
                    .as_str()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                println!(
                    "{:<10} {:<24} {:<10} {}{}",
                    s(&v["id"]),
                    s(&v["name"]),
                    s(&v["arch"]),
                    s(&v["state"]),
                    reason
                );
            }
        }
        Cmd::Vm { action } => vm_cmd(c, action, json)?,
        Cmd::Start { vm }
        | Cmd::Stop { vm }
        | Cmd::ForceStop { vm }
        | Cmd::Pause { vm }
        | Cmd::Resume { vm }
        | Cmd::Reset { vm } => {
            let action = match &cli.cmd {
                Cmd::Start { .. } => "start",
                Cmd::Stop { .. } => "stop",
                Cmd::ForceStop { .. } => "force-stop",
                Cmd::Pause { .. } => "pause",
                Cmd::Reset { .. } => "reset",
                _ => "resume",
            };
            let id = c.vm_id(vm)?;
            let v = c.post(&format!("/vms/{id}/actions/{action}"), None)?;
            println!("{}: {}", v["name"].as_str().unwrap_or(vm), s(&v["state"]));
        }
        Cmd::Networks { action } => net_cmd(c, action.as_ref().unwrap_or(&NetAction::List), json)?,
        Cmd::Snapshots { action } => snap_cmd(c, action, json)?,
        Cmd::Pools => {
            let v = c.get("/pools")?;
            if json {
                return print_json(&v);
            }
            for p in v.as_array().into_iter().flatten() {
                println!(
                    "{:<16} {:>8} MiB used  {}",
                    s(&p["name"]),
                    p["used_bytes"].as_u64().unwrap_or(0) >> 20,
                    s(&p["path"])
                );
            }
        }
        Cmd::Backups { action } => {
            backup_cmd(c, action.as_ref().unwrap_or(&BackupAction::List), json)?
        }
        Cmd::Jobs { action } => match action.as_ref().unwrap_or(&JobAction::List) {
            JobAction::List => {
                let v = c.get("/jobs")?;
                if json {
                    return print_json(&v);
                }
                for j in v.as_array().into_iter().flatten() {
                    println!(
                        "{:<12} {:<12} {:<10} {}",
                        s(&j["id"]),
                        s(&j["kind"]),
                        s(&j["status"]),
                        j["message"].as_str().unwrap_or("")
                    );
                }
            }
            JobAction::Cancel { id } => {
                c.post(&format!("/jobs/{id}/cancel"), None)?;
                println!("cancel requested for job {id}");
            }
        },
        Cmd::Tokens { action } => {
            token_cmd(c, action.as_ref().unwrap_or(&TokenAction::List), json)?
        }
        Cmd::Audit { limit } => {
            let v = c.get(&format!("/audit?limit={limit}"))?;
            if json {
                return print_json(&v);
            }
            for e in v["entries"].as_array().into_iter().flatten() {
                println!(
                    "{:<11} {:<16} {:<28} {:<16} {}",
                    e["time"].as_u64().unwrap_or(0),
                    s(&e["actor"]),
                    s(&e["action"]),
                    e["target"].as_str().unwrap_or(""),
                    e["outcome"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Export { output, toml } => {
            let as_toml = *toml
                || output
                    .as_deref()
                    .and_then(Path::extension)
                    .is_some_and(|e| e == "toml");
            let text = apply::export(c, as_toml)?;
            match output {
                Some(p) => {
                    std::fs::write(p, text).with_context(|| format!("writing {}", p.display()))?;
                    println!("wrote {}", p.display());
                }
                None => print!("{text}"),
            }
        }
        Cmd::Apply {
            file,
            dry_run,
            prune,
        } => {
            let m = apply::load(file)?;
            let plan = apply::plan(c, &m, *prune)?;
            if plan.is_empty() {
                println!("no changes; the daemon already matches {}", file.display());
                return Ok(());
            }
            if *dry_run {
                for ch in &plan.changes {
                    println!("{}", ch.describe());
                }
                println!(
                    "{} change(s); run without --dry-run to apply",
                    plan.changes.len()
                );
                return Ok(());
            }
            apply::execute(c, &plan)?;
            println!("applied {} change(s)", plan.changes.len());
        }
    }
    Ok(())
}

fn net_cmd(c: &Client, action: &NetAction, json: bool) -> Result<()> {
    match action {
        NetAction::List => {
            let v = c.get("/networks")?;
            if json {
                return print_json(&v);
            }
            for n in v.as_array().into_iter().flatten() {
                let users: Vec<&str> = n["used_by"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                println!(
                    "{:<10} {:<20} {:<10} {}",
                    s(&n["id"]),
                    s(&n["name"]),
                    s(&n["config"]["kind"]),
                    users.join(",")
                );
            }
        }
        NetAction::Delete { network } => {
            let id = c.network_id(network)?;
            c.delete(&format!("/networks/{id}"))?;
            println!("deleted network {network}");
        }
    }
    Ok(())
}

fn token_cmd(c: &Client, action: &TokenAction, json: bool) -> Result<()> {
    match action {
        TokenAction::List => {
            let v = c.get("/auth/tokens")?;
            if json {
                return print_json(&v);
            }
            for t in v.as_array().into_iter().flatten() {
                println!("{:<12} {}", s(&t["id"]), s(&t["name"]));
            }
        }
        TokenAction::Create { name } => {
            let v = c.post("/auth/tokens", Some(&json!({ "name": name })))?;
            if json {
                return print_json(&v);
            }
            println!(
                "token {} ({name}) created; it is shown only once:",
                s(&v["id"])
            );
            println!("{}", s(&v["token"]));
        }
        TokenAction::Revoke { id } => {
            c.delete(&format!("/auth/tokens/{id}"))?;
            println!("revoked token {id}");
        }
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    // Accept a bare spec, or `{ "spec": ... }` as `vm show --json` prints.
    Ok(v.get("spec").cloned().unwrap_or(v))
}

fn vm_cmd(c: &Client, action: &VmAction, json: bool) -> Result<()> {
    match action {
        VmAction::Show { vm } => {
            let id = c.vm_id(vm)?;
            let v = c.get(&format!("/vms/{id}"))?;
            if json {
                return print_json(&v);
            }
            let sum = &v["summary"];
            println!("{} ({id})  {}", s(&sum["name"]), s(&sum["state"]));
            if let Some(e) = sum["last_error"].as_str() {
                println!("last error: {e}");
            }
            print_json(&v["spec"])?;
        }
        VmAction::Create { file } => {
            let spec = read_json(file)?;
            let v = c.post("/vms", Some(&json!({ "spec": spec })))?;
            println!("created {}", s(&v["summary"]["id"]));
        }
        VmAction::Update { vm, file } => {
            let id = c.vm_id(vm)?;
            let spec = read_json(file)?;
            c.put(&format!("/vms/{id}"), &json!({ "spec": spec }))?;
            println!("updated {vm}");
        }
        VmAction::Delete { vm } => {
            let id = c.vm_id(vm)?;
            c.delete(&format!("/vms/{id}"))?;
            println!("deleted {vm}");
        }
        VmAction::Clone { vm, name, full } => {
            let id = c.vm_id(vm)?;
            let mode = if *full { "full" } else { "linked" };
            let v = c.post(
                &format!("/vms/{id}/clone"),
                Some(&json!({"name": name, "mode": mode})),
            )?;
            if json {
                return print_json(&v);
            }
            println!("cloning {vm} as {name} ({mode})");
        }
        VmAction::Argv { vm } => {
            let id = c.vm_id(vm)?;
            let d = c.get(&format!("/vms/{id}"))?;
            let v = c.post(
                "/vms/compile",
                Some(&json!({"spec": d["spec"], "vm_id": id})),
            )?;
            if json {
                return print_json(&v);
            }
            match v["command"].as_str() {
                Some(cmd) => println!("{cmd}"),
                None => print_json(&v)?,
            }
        }
        VmAction::SerialLog { vm } => {
            let id = c.vm_id(vm)?;
            let v = c.get(&format!("/vms/{id}/serial-log"))?;
            match v.as_str() {
                Some(t) => print!("{t}"),
                None => print_json(&v)?,
            }
        }
    }
    Ok(())
}

/// A snapshot id by name or id.
fn snapshot_id(list: &Value, snap: &str) -> Result<String> {
    list["snapshots"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["id"] == snap || x["name"] == snap)
        .and_then(|x| x["id"].as_str().map(String::from))
        .with_context(|| format!("no snapshot named '{snap}'"))
}

fn snap_cmd(c: &Client, action: &SnapAction, json: bool) -> Result<()> {
    match action {
        SnapAction::List { vm } => {
            let id = c.vm_id(vm)?;
            let v = c.get(&format!("/vms/{id}/snapshots"))?;
            if json {
                return print_json(&v);
            }
            let head = v["head"].as_str();
            for x in v["snapshots"].as_array().into_iter().flatten() {
                println!(
                    "{} {:<10} {:<24} {:<9} {}",
                    if x["id"].as_str() == head { "*" } else { " " },
                    s(&x["id"]),
                    s(&x["name"]),
                    s(&x["kind"]),
                    if x["with_memory"].as_bool() == Some(true) {
                        "with memory"
                    } else {
                        ""
                    }
                );
            }
            if let Some(r) = v["unsupported_reason"].as_str() {
                println!("internal snapshots unavailable: {r}");
            }
        }
        SnapAction::Take {
            vm,
            name,
            memory,
            external,
        } => {
            let id = c.vm_id(vm)?;
            let v = c.post(
                &format!("/vms/{id}/snapshots"),
                Some(&json!({"name": name, "with_memory": memory, "external": external})),
            )?;
            println!("taking snapshot {name} (job {})", s(&v["id"]));
        }
        SnapAction::Revert {
            vm,
            snapshot,
            no_safety,
        } => {
            let id = c.vm_id(vm)?;
            let sid = snapshot_id(&c.get(&format!("/vms/{id}/snapshots"))?, snapshot)?;
            let v = c.post(
                &format!("/vms/{id}/snapshots/{sid}/revert"),
                Some(&json!({"safety_snapshot": !no_safety})),
            )?;
            println!("reverting to {snapshot} (job {})", s(&v["id"]));
        }
        SnapAction::Delete { vm, snapshot } => {
            let id = c.vm_id(vm)?;
            let sid = snapshot_id(&c.get(&format!("/vms/{id}/snapshots"))?, snapshot)?;
            c.delete(&format!("/vms/{id}/snapshots/{sid}"))?;
            println!("deleted snapshot {snapshot}");
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

fn service_cmd(action: &ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Status => service::status(),
        ServiceAction::Uninstall { dry_run } => {
            let plan = if cfg!(windows) {
                service::windows_uninstall_plan()
            } else {
                service::linux_uninstall_plan()
            };
            if !dry_run && !service::privileged() {
                anyhow::bail!(
                    "removing a service needs root / an elevated shell (or use --dry-run)"
                );
            }
            service::execute(&plan, *dry_run)?;
            if !dry_run {
                println!("removed; running VMs keep running and the data directory is kept");
            }
            Ok(())
        }
        ServiceAction::Install(a) => {
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .context("locating this program")?;
            let bin = |name: &str| exe_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            let abs = |p: PathBuf| service::plain(std::fs::canonicalize(&p).unwrap_or(p));
            let daemon = abs(a.daemon_bin.clone().unwrap_or_else(|| bin("qemu-uid")));
            if !daemon.is_file() && !a.dry_run {
                anyhow::bail!(
                    "daemon binary not found at {} (build it, or pass --daemon-bin)",
                    daemon.display()
                );
            }
            let helper = bin("qemu-ui-helper");
            let qemu_dir = a
                .qemu_dir
                .clone()
                .or_else(|| std::env::var_os("QEMU_UI_QEMU_DIR").map(PathBuf::from))
                .or_else(|| service::find_up(&exe_dir, &[".tools", "qemu"]))
                .map(abs);
            let web_dir = a
                .web_dir
                .clone()
                .or_else(|| service::find_up(&exe_dir, &["web", "dist"]))
                .map(abs);
            let plan = if cfg!(windows) {
                let data_dir = a.data_dir.clone().unwrap_or_else(|| {
                    PathBuf::from(
                        std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into()),
                    )
                    .join("qemu-ui")
                });
                service::windows_install_plan(&service::Install {
                    daemon,
                    helper: None,
                    helper_config: PathBuf::new(),
                    account: a
                        .account
                        .clone()
                        .unwrap_or_else(|| service::WINDOWS_DEFAULT_ACCOUNT.into()),
                    data_dir,
                    listen: a.listen.clone(),
                    qemu_dir,
                    web_dir,
                    kvm_group: false,
                })
            } else {
                service::linux_install_plan(&service::Install {
                    daemon,
                    helper: (!a.no_helper).then(|| abs(helper)),
                    helper_config: PathBuf::from("/etc/qemu-ui/helper.toml"),
                    account: a.account.clone().unwrap_or_else(|| "qemu-ui".into()),
                    data_dir: a
                        .data_dir
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/var/lib/qemu-ui")),
                    listen: a.listen.clone(),
                    qemu_dir,
                    web_dir,
                    kvm_group: std::fs::read_to_string("/etc/group")
                        .is_ok_and(|g| g.lines().any(|l| l.starts_with("kvm:"))),
                })
            };
            if !a.dry_run && !service::privileged() {
                anyhow::bail!(
                    "installing a service needs root / an elevated shell (or use --dry-run to see the steps)"
                );
            }
            service::execute(&plan, a.dry_run)
        }
    }
}

fn backup_cmd(c: &Client, action: &BackupAction, json: bool) -> Result<()> {
    match action {
        BackupAction::List => {
            let v = c.get("/backups")?;
            if json {
                return print_json(&v);
            }
            for b in v.as_array().into_iter().flatten() {
                let m = &b["manifest"];
                println!(
                    "{:<44} {:>8} MiB  {}{}",
                    s(&b["name"]),
                    b["size"].as_u64().unwrap_or(0) >> 20,
                    m["source_name"].as_str().unwrap_or("(unreadable)"),
                    if m["live"] == true { "  live" } else { "" }
                );
            }
        }
        BackupAction::Export { vm, snapshots } => {
            let id = c.vm_id(vm)?;
            let v = c.post(
                &format!("/vms/{id}/export"),
                Some(&json!({"snapshots": snapshots})),
            )?;
            if json {
                return print_json(&v);
            }
            println!("{} (job {})", s(&v["job"]["title"]), s(&v["job"]["id"]));
        }
        BackupAction::Import { bundle, name, pool } => {
            let v = c.post(
                "/backups/import",
                Some(&json!({"bundle": bundle, "name": name, "pool": pool})),
            )?;
            if json {
                return print_json(&v);
            }
            println!("{} (job {})", s(&v["job"]["title"]), s(&v["job"]["id"]));
        }
        BackupAction::Delete { bundle } => {
            c.delete(&format!("/backups/{bundle}"))?;
            println!("deleted {bundle}");
        }
    }
    Ok(())
}
