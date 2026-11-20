//! `qemu-ui service install|uninstall|status` (M6-09).
//!
//! Registers the daemon with the platform's service manager: systemd units on
//! Linux (daemon + privileged helper), a Windows service elsewhere. Every
//! action is first built as a list of [`Step`]s, so `--dry-run` prints exactly
//! what a real run would do. Nothing is installed implicitly.
//!
//! VMs outlive the service on both platforms (plan.md §3.2, M1-06): the unit
//! uses `KillMode=process`, and on Windows QEMU is spawned detached, outside
//! any kill-on-close job. See docs/process-survival.md.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const DAEMON_UNIT: &str = "qemu-ui.service";
pub const HELPER_UNIT: &str = "qemu-ui-helper.service";
const DAEMON_TEMPLATE: &str = include_str!("../../../data/systemd/qemu-ui.service.in");
const HELPER_TEMPLATE: &str = include_str!("../../../data/systemd/qemu-ui-helper.service.in");

/// Where things are for this install.
#[derive(Debug, Clone)]
pub struct Install {
    pub daemon: PathBuf,
    pub helper: Option<PathBuf>,
    pub helper_config: PathBuf,
    /// Linux: the daemon's system user. Windows: the service account.
    pub account: String,
    pub data_dir: PathBuf,
    pub listen: String,
    pub qemu_dir: Option<PathBuf>,
    pub web_dir: Option<PathBuf>,
    /// Linux: add the daemon user to `kvm` (only when the group exists).
    pub kvm_group: bool,
}

/// One action of an install/uninstall plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    RemoveFile {
        path: PathBuf,
    },
    Run {
        program: String,
        args: Vec<String>,
    },
    /// Windows: register the service with the SCM.
    CreateService {
        name: String,
        display_name: String,
        binary: PathBuf,
        args: Vec<String>,
        account: String,
    },
    /// Windows: stop (if running) and delete the service.
    DeleteService {
        name: String,
    },
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::WriteFile { path, contents } => {
                writeln!(f, "write {}:", path.display())?;
                for line in contents.lines() {
                    writeln!(f, "    {line}")?;
                }
                Ok(())
            }
            Step::RemoveFile { path } => writeln!(f, "remove {}", path.display()),
            Step::Run { program, args } => writeln!(f, "run: {program} {}", args.join(" ")),
            Step::CreateService {
                name,
                display_name,
                binary,
                args,
                account,
            } => {
                writeln!(f, "create service {name} (\"{display_name}\"):")?;
                writeln!(f, "    binary:  {} {}", binary.display(), args.join(" "))?;
                writeln!(f, "    account: {account}")?;
                writeln!(f, "    start:   automatic")
            }
            Step::DeleteService { name } => writeln!(f, "stop and delete service {name}"),
        }
    }
}

// ------------------------------------------------------------------ systemd

/// Quote one word for a systemd command line or `Environment=` value.
pub fn sd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    if escaped.is_empty()
        || escaped
            .chars()
            .any(|c| c.is_whitespace() || c == ';' || c == '\'')
    {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn path_word(p: &Path) -> String {
    sd_quote(&p.display().to_string())
}

fn environment(i: &Install) -> String {
    match &i.qemu_dir {
        Some(q) => format!(
            "Environment={}",
            sd_quote(&format!("QEMU_UI_QEMU_DIR={}", q.display()))
        ),
        None => String::new(),
    }
}

/// Fill a template's `@PLACEHOLDERS@`.
fn fill(template: &str, pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .fold(template.to_string(), |s, (k, v)| s.replace(k, v))
}

pub fn render_daemon_unit(i: &Install) -> String {
    let mut extra = String::new();
    if let Some(w) = &i.web_dir {
        let _ = write!(extra, " --web-dir {}", path_word(w));
    }
    let unit = fill(
        DAEMON_TEMPLATE,
        &[
            ("@USER@", i.account.clone()),
            ("@DAEMON@", path_word(&i.daemon)),
            ("@DATA_DIR@", path_word(&i.data_dir)),
            ("@LISTEN@", sd_quote(&i.listen)),
            ("@EXTRA_ARGS@", extra),
            ("@WORK_DIR@", path_word(&i.data_dir)),
            ("@ENVIRONMENT@", environment(i)),
        ],
    );
    let unit = if i.kvm_group {
        unit
    } else {
        unit.replace("SupplementaryGroups=kvm\n", "")
    };
    drop_empty_placeholder_lines(&unit)
}

pub fn render_helper_unit(i: &Install, helper: &Path) -> String {
    let unit = fill(
        HELPER_TEMPLATE,
        &[
            ("@HELPER@", path_word(helper)),
            ("@HELPER_CONFIG@", path_word(&i.helper_config)),
            ("@ENVIRONMENT@", environment(i)),
        ],
    );
    drop_empty_placeholder_lines(&unit)
}

/// An unset `@ENVIRONMENT@` leaves a blank line; collapse runs of blank
/// lines so the unit stays tidy.
fn drop_empty_placeholder_lines(unit: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in unit.lines() {
        let is_blank = line.trim().is_empty();
        if !(is_blank && blank) {
            out.push_str(line);
            out.push('\n');
        }
        blank = is_blank;
    }
    out
}

fn unit_dir() -> PathBuf {
    PathBuf::from("/etc/systemd/system")
}

pub fn linux_install_plan(i: &Install) -> Vec<Step> {
    let user = &i.account;
    let mut steps = vec![
        Step::Run {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "id -u {u} >/dev/null 2>&1 || useradd --system --home-dir {d} --shell /usr/sbin/nologin {u}",
                    u = user,
                    d = sh_quote(&i.data_dir.display().to_string())
                ),
            ],
        },
        Step::Run {
            program: "install".into(),
            args: vec![
                "-d".into(),
                "-o".into(),
                user.clone(),
                "-g".into(),
                user.clone(),
                "-m".into(),
                "0750".into(),
                i.data_dir.display().to_string(),
            ],
        },
        Step::WriteFile {
            path: unit_dir().join(DAEMON_UNIT),
            contents: render_daemon_unit(i),
        },
    ];
    let mut units = vec![DAEMON_UNIT.to_string()];
    if let Some(h) = &i.helper {
        steps.push(Step::WriteFile {
            path: unit_dir().join(HELPER_UNIT),
            contents: render_helper_unit(i, h),
        });
        units.insert(0, HELPER_UNIT.to_string());
    }
    steps.push(Step::Run {
        program: "systemctl".into(),
        args: vec!["daemon-reload".into()],
    });
    let mut enable = vec!["enable".to_string(), "--now".to_string()];
    enable.extend(units);
    steps.push(Step::Run {
        program: "systemctl".into(),
        args: enable,
    });
    steps
}

pub fn linux_uninstall_plan() -> Vec<Step> {
    vec![
        Step::Run {
            program: "systemctl".into(),
            args: vec![
                "disable".into(),
                "--now".into(),
                DAEMON_UNIT.into(),
                HELPER_UNIT.into(),
            ],
        },
        Step::RemoveFile {
            path: unit_dir().join(DAEMON_UNIT),
        },
        Step::RemoveFile {
            path: unit_dir().join(HELPER_UNIT),
        },
        Step::Run {
            program: "systemctl".into(),
            args: vec!["daemon-reload".into()],
        },
    ]
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ------------------------------------------------------------------ Windows

pub const WINDOWS_SERVICE: &str = "qemu-ui";
/// A per-service virtual account: no password, no interactive logon, only
/// the rights granted to it explicitly (the data directory ACL below).
pub const WINDOWS_DEFAULT_ACCOUNT: &str = "NT SERVICE\\qemu-ui";

/// The service's command line after the binary.
pub fn windows_service_args(i: &Install) -> Vec<String> {
    let mut args = vec![
        "--service".to_string(),
        "--data-dir".into(),
        i.data_dir.display().to_string(),
        "--listen".into(),
        i.listen.clone(),
    ];
    // Services start in System32: give the daemon absolute locations for
    // what it otherwise finds relative to the working directory.
    if let Some(q) = &i.qemu_dir {
        args.push("--qemu-dir".into());
        args.push(q.display().to_string());
    }
    if let Some(w) = &i.web_dir {
        args.push("--web-dir".into());
        args.push(w.display().to_string());
    }
    args
}

pub fn windows_install_plan(i: &Install) -> Vec<Step> {
    let mut steps = vec![Step::Run {
        program: "cmd".into(),
        args: vec![
            "/c".into(),
            "mkdir".into(),
            i.data_dir.display().to_string(),
        ],
    }];
    steps.push(Step::CreateService {
        name: WINDOWS_SERVICE.into(),
        display_name: "qemu-ui daemon".into(),
        binary: i.daemon.clone(),
        args: windows_service_args(i),
        account: i.account.clone(),
    });
    if !i.account.eq_ignore_ascii_case("LocalSystem") {
        // The account owns its data (database, run dirs, logs). Pool
        // directories and ISOs elsewhere need the same grant (docs).
        steps.push(Step::Run {
            program: "icacls".into(),
            args: vec![
                i.data_dir.display().to_string(),
                "/grant".into(),
                format!("{}:(OI)(CI)M", i.account),
            ],
        });
    }
    steps.push(Step::Run {
        program: "sc.exe".into(),
        args: vec!["start".into(), WINDOWS_SERVICE.into()],
    });
    steps
}

pub fn windows_uninstall_plan() -> Vec<Step> {
    vec![Step::DeleteService {
        name: WINDOWS_SERVICE.into(),
    }]
}

// ---------------------------------------------------------------- execution

/// Print or perform a plan.
pub fn execute(steps: &[Step], dry_run: bool) -> Result<()> {
    for s in steps {
        if dry_run {
            print!("{s}");
            continue;
        }
        print!("{s}");
        match s {
            Step::WriteFile { path, contents } => std::fs::write(path, contents)
                .with_context(|| format!("writing {}", path.display()))?,
            Step::RemoveFile { path } => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            },
            Step::Run { program, args } => {
                let st = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .status()
                    .with_context(|| format!("running {program}"))?;
                // `mkdir` of an existing directory and `disable` of a missing
                // unit fail harmlessly; everything else must succeed.
                let tolerated = (program == "cmd" && args.get(1).is_some_and(|a| a == "mkdir"))
                    || (program == "systemctl" && args.first().is_some_and(|a| a == "disable"));
                if !st.success() && !tolerated {
                    bail!("{program} failed ({st})");
                }
            }
            Step::CreateService { .. } | Step::DeleteService { .. } => scm(s)?,
        }
    }
    if dry_run {
        println!("(dry run: nothing was changed)");
    }
    Ok(())
}

#[cfg(windows)]
fn scm(step: &Step) -> Result<()> {
    use std::ffi::OsString;
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    match step {
        Step::CreateService {
            name,
            display_name,
            binary,
            args,
            account,
        } => {
            let m = ServiceManager::local_computer(
                None::<&str>,
                ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
            )
            .context("opening the service manager (run as administrator)")?;
            let info = ServiceInfo {
                name: OsString::from(name),
                display_name: OsString::from(display_name),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path: binary.clone(),
                launch_arguments: args.iter().map(OsString::from).collect(),
                dependencies: vec![],
                account_name: (!account.eq_ignore_ascii_case("LocalSystem"))
                    .then(|| OsString::from(account)),
                account_password: None,
            };
            let svc = m
                .create_service(&info, ServiceAccess::CHANGE_CONFIG)
                .with_context(|| format!("creating service {name}"))?;
            svc.set_description(
                "qemu-ui: QEMU web UI daemon. Stopping it leaves running VMs running; they are reattached on start.",
            )?;
            Ok(())
        }
        Step::DeleteService { name } => {
            let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                .context("opening the service manager (run as administrator)")?;
            let svc = m
                .open_service(
                    name,
                    ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
                )
                .with_context(|| format!("opening service {name}"))?;
            if svc.query_status()?.current_state != ServiceState::Stopped {
                let _ = svc.stop();
            }
            svc.delete()
                .with_context(|| format!("deleting service {name}"))?;
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(not(windows))]
fn scm(_step: &Step) -> Result<()> {
    bail!("Windows services can only be managed on Windows")
}

/// `service status`.
pub fn status() -> Result<()> {
    #[cfg(windows)]
    {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        let m = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        match m.open_service(WINDOWS_SERVICE, ServiceAccess::QUERY_STATUS) {
            Ok(s) => println!(
                "service {WINDOWS_SERVICE}: {:?}",
                s.query_status()?.current_state
            ),
            Err(_) => println!("service {WINDOWS_SERVICE}: not installed"),
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let st = std::process::Command::new("systemctl")
            .args(["status", "--no-pager", DAEMON_UNIT, HELPER_UNIT])
            .stdin(std::process::Stdio::null())
            .status()
            .context("running systemctl")?;
        // `systemctl status` exits non-zero for stopped/missing units; the
        // printed status is the answer.
        let _ = st;
        Ok(())
    }
}

/// Is this process running as root / an administrator?
pub fn privileged() -> bool {
    if cfg!(windows) {
        // `net session` succeeds only in an elevated shell.
        std::process::Command::new("net")
            .arg("session")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    } else {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
    }
}

/// Find `.tools/qemu` or `web/dist` above the binary's directory (a
/// project-local install keeps them in the checkout).
pub fn find_up(start: &Path, rel: &[&str]) -> Option<PathBuf> {
    start.ancestors().find_map(|d| {
        let p = rel.iter().fold(d.to_path_buf(), |p, r| p.join(r));
        p.is_dir().then_some(p)
    })
}

/// Strip Windows' `\\?\` prefix so paths read normally in service configs.
pub fn plain(p: PathBuf) -> PathBuf {
    let s = p.display().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install() -> Install {
        Install {
            daemon: PathBuf::from("/opt/qemu-ui/target/release/qemu-uid"),
            helper: Some(PathBuf::from("/opt/qemu-ui/target/release/qemu-ui-helper")),
            helper_config: PathBuf::from("/etc/qemu-ui/helper.toml"),
            account: "qemu-ui".into(),
            data_dir: PathBuf::from("/var/lib/qemu-ui"),
            listen: "127.0.0.1:7480".into(),
            qemu_dir: Some(PathBuf::from("/opt/qemu-ui/.tools/qemu")),
            web_dir: Some(PathBuf::from("/opt/qemu-ui/web dist")),
            kvm_group: true,
        }
    }

    #[test]
    fn daemon_unit_keeps_vms_across_restarts_and_quotes_paths() {
        let u = render_daemon_unit(&install());
        for want in [
            "KillMode=process\n",
            "RuntimeDirectory=qemu-ui\n",
            "RuntimeDirectoryPreserve=yes\n",
            "User=qemu-ui\n",
            "SupplementaryGroups=kvm\n",
            "ExecStart=/opt/qemu-ui/target/release/qemu-uid --data-dir /var/lib/qemu-ui --run-dir /run/qemu-ui --listen 127.0.0.1:7480 --web-dir \"/opt/qemu-ui/web dist\"\n",
            "Environment=QEMU_UI_QEMU_DIR=/opt/qemu-ui/.tools/qemu\n",
        ] {
            assert!(u.contains(want), "missing {want:?} in:\n{u}");
        }
        assert!(
            !u.contains(
                "
MemoryDenyWriteExecute="
            ),
            "TCG needs W+X pages"
        );
        for p in [
            "@USER@",
            "@DAEMON@",
            "@DATA_DIR@",
            "@LISTEN@",
            "@EXTRA_ARGS@",
            "@WORK_DIR@",
            "@ENVIRONMENT@",
        ] {
            assert!(!u.contains(p), "{p} left in:\n{u}");
        }
    }

    #[test]
    fn daemon_unit_without_kvm_or_extras() {
        let mut i = install();
        i.kvm_group = false;
        i.qemu_dir = None;
        i.web_dir = None;
        let u = render_daemon_unit(&i);
        assert!(!u.contains("SupplementaryGroups"));
        assert!(!u.contains("Environment="));
        assert!(u.contains("--listen 127.0.0.1:7480\n"));
        assert!(!u.contains("\n\n\n"));
    }

    #[test]
    fn helper_unit_matches_the_daemons_default_socket() {
        let i = install();
        let u = render_helper_unit(&i, i.helper.as_ref().unwrap());
        assert!(
            u.contains("--state-dir /run/qemu-ui-helper --config /etc/qemu-ui/helper.toml\n"),
            "{u}"
        );
        assert!(u.contains("RuntimeDirectory=qemu-ui-helper\n"));
        assert!(u.contains("KillMode=process\n"));
        for p in ["@HELPER@", "@HELPER_CONFIG@", "@ENVIRONMENT@"] {
            assert!(!u.contains(p), "{p} left in:\n{u}");
        }
    }

    #[test]
    fn systemd_quoting() {
        assert_eq!(sd_quote("/a/b"), "/a/b");
        assert_eq!(sd_quote("/a b"), "\"/a b\"");
        assert_eq!(sd_quote("50%"), "50%%");
        assert_eq!(sd_quote("a\"b"), "a\\\"b");
        assert_eq!(sd_quote(""), "\"\"");
    }

    #[test]
    fn linux_plan_writes_both_units_then_enables() {
        let steps = linux_install_plan(&install());
        let written: Vec<_> = steps
            .iter()
            .filter_map(|s| match s {
                Step::WriteFile { path, .. } => Some(path.display().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(written.len(), 2);
        assert!(written[0].ends_with("qemu-ui.service"));
        assert!(written[1].ends_with("qemu-ui-helper.service"));
        assert_eq!(
            steps.last(),
            Some(&Step::Run {
                program: "systemctl".into(),
                args: vec![
                    "enable".into(),
                    "--now".into(),
                    HELPER_UNIT.into(),
                    DAEMON_UNIT.into()
                ],
            })
        );
        let text: String = steps.iter().map(ToString::to_string).collect();
        assert!(text.contains("useradd --system"));
        assert!(
            text.contains("write /etc/systemd/system/qemu-ui.service:\n    [Unit]")
                || text.contains("    # qemu-ui daemon")
        );
    }

    #[test]
    fn windows_plan_uses_a_virtual_account_and_absolute_locations() {
        let mut i = install();
        i.account = WINDOWS_DEFAULT_ACCOUNT.into();
        i.data_dir = PathBuf::from(r"C:\ProgramData\qemu-ui");
        i.daemon = PathBuf::from(r"C:\qemu-ui\target\release\qemu-uid.exe");
        i.qemu_dir = Some(PathBuf::from(r"C:\qemu-ui\.tools\qemu"));
        let steps = windows_install_plan(&i);
        let Some(Step::CreateService { args, account, .. }) = steps
            .iter()
            .find(|s| matches!(s, Step::CreateService { .. }))
        else {
            panic!("no service step: {steps:?}");
        };
        assert_eq!(account, "NT SERVICE\\qemu-ui");
        assert_eq!(
            &args[..5],
            [
                "--service",
                "--data-dir",
                r"C:\ProgramData\qemu-ui",
                "--listen",
                "127.0.0.1:7480"
            ]
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--qemu-dir", r"C:\qemu-ui\.tools\qemu"])
        );
        assert!(steps.iter().any(|s| matches!(s, Step::Run { program, args } if program == "icacls" && args[2] == "NT SERVICE\\qemu-ui:(OI)(CI)M")));
        // LocalSystem needs no grant.
        i.account = "LocalSystem".into();
        assert!(
            !windows_install_plan(&i)
                .iter()
                .any(|s| matches!(s, Step::Run { program, .. } if program == "icacls"))
        );
    }

    #[test]
    fn dry_run_prints_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("qui-svc-dry-{}", std::process::id()));
        let target = dir.join("x.service");
        let steps = vec![
            Step::WriteFile {
                path: target.clone(),
                contents: "[Unit]\n".into(),
            },
            Step::Run {
                program: "definitely-not-a-program".into(),
                args: vec![],
            },
        ];
        execute(&steps, true).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn plain_strips_verbatim_prefix() {
        assert_eq!(plain(PathBuf::from(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(plain(PathBuf::from("/x")), PathBuf::from("/x"));
    }
}
