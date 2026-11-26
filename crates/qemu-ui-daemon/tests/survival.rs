//! Process survival (plan.md §3.2, M1-06): the real daemon binary is stopped
//! the ways a service manager stops it, and the VM must keep running and be
//! reattached by the next daemon.
//!
//! * Unix: SIGTERM (what `systemctl stop`/`restart` sends; with
//!   `KillMode=process` it reaches only the daemon) and SIGKILL.
//! * Windows: process termination (service stop that times out, crash).
//!
//! Skips when no QEMU is installed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

fn find(exe: &str) -> Option<PathBuf> {
    let exe = format!("{exe}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs: Vec<PathBuf> = std::env::var_os("QEMU_UI_QEMU_DIR")
        .map(|d| vec![PathBuf::from(d)])
        .unwrap_or_default();
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Point the daemon at the QEMU this test found, wherever it came from.
fn qemu_dir_arg() -> Vec<String> {
    find("qemu-system-x86_64")
        .and_then(|p| {
            p.parent()
                .map(|d| vec!["--qemu-dir".into(), d.display().to_string()])
        })
        .unwrap_or_default()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Daemon {
    child: Child,
    base: String,
    token: String,
}

impl Daemon {
    // Reaped in `kill`, which every test path ends with.
    #[allow(clippy::zombie_processes)]
    fn start(data: &Path, port: u16) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_qemu-uid"))
            .arg("--data-dir")
            .arg(data)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .args(qemu_dir_arg())
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(data.join("daemon-test.log"))
                    .unwrap(),
            ))
            .spawn()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}/api/v1");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(token) = std::fs::read_to_string(data.join("api-token"))
                && ureq::get(format!("{base}/health"))
                    .header("Authorization", &format!("Bearer {}", token.trim()))
                    .call()
                    .is_ok()
            {
                return Daemon {
                    child,
                    base,
                    token: token.trim().to_string(),
                };
            }
            assert!(Instant::now() < deadline, "daemon did not come up");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn get(&self, path: &str) -> Value {
        ureq::get(format!("{}{path}", self.base))
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut r = ureq::post(format!("{}{path}", self.base))
            .header("Authorization", &format!("Bearer {}", self.token))
            .config()
            .http_status_as_error(false)
            .build()
            .send_json(body)
            .map_err(|e| e.to_string())?;
        let ok = r.status().is_success();
        let v: Value = r.body_mut().read_json().unwrap_or(Value::Null);
        if ok { Ok(v) } else { Err(v.to_string()) }
    }

    fn state(&self, id: &str) -> String {
        self.get(&format!("/vms/{id}"))["summary"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn wait_state(&self, id: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.state(id) != want {
            assert!(
                Instant::now() < deadline,
                "VM never reached {want}: {}",
                self.state(id)
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Stop the daemon the way a service manager would.
    fn kill(mut self, how: &str) {
        match how {
            #[cfg(unix)]
            "term" => {
                let st = Command::new("kill")
                    .args(["-TERM", &self.child.id().to_string()])
                    .status()
                    .unwrap();
                assert!(st.success());
            }
            _ => self.child.kill().unwrap(),
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn qemu_alive(pid: u32) -> bool {
    let mut sys = System::new().unwrap();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(Pid::from_u32(pid)).is_some()
}

/// The running VM's QEMU pid, from the daemon's own database.
fn qemu_pid(data: &Path, id: &str) -> u32 {
    let db = rusqlite::Connection::open(data.join("qemu-ui.db")).unwrap();
    db.query_row(
        "SELECT pid FROM vm_run WHERE vm_id = ?1 AND ended_at IS NULL ORDER BY id DESC LIMIT 1",
        [id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap() as u32
}

#[test]
fn vms_survive_the_daemon_being_stopped_and_are_reattached() {
    if find("qemu-system-x86_64").is_none() || find("qemu-img").is_none() {
        eprintln!("skipping: QEMU not found");
        return;
    }
    // Short: per-VM unix socket paths must stay under 108 bytes.
    let data = if cfg!(windows) {
        std::env::temp_dir().join(format!("qui-surv-{}", std::process::id()))
    } else {
        PathBuf::from(format!("/tmp/qui-surv-{}", std::process::id()))
    };
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let port = free_port();

    let d = Daemon::start(&data, port);
    // Wait for the whole first scan: on a runner with every qemu-system-*
    // installed this takes minutes. Later daemons reuse the cached snapshot.
    let deadline = Instant::now() + Duration::from_secs(900);
    let arch = loop {
        let c = d.get("/host/capabilities");
        let has_x86 = c["caps"]["binaries"]
            .as_array()
            .is_some_and(|b| b.iter().any(|b| b["arch"] == "x86_64"));
        if c["scan"]["running"] == false && c["caps"].is_object() {
            let probed: Vec<String> = c["caps"]["binaries"]
                .as_array()
                .map(|b| {
                    b.iter()
                        .map(|b| format!("{} {}", b["arch"], b["binary"]["path"]))
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                has_x86,
                "scan finished without qemu-system-x86_64: probed {probed:?}; failed {}; scan {}",
                c["caps"]["failed"], c["scan"]
            );
            break "x86_64";
        }
        assert!(
            Instant::now() < deadline,
            "first scan did not finish: {}",
            c["scan"]
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let mut spec = d.get(&format!("/vms/defaults?arch={arch}&name=survivor"))["spec"].clone();
    spec["accel"] = json!("tcg");
    spec["cpu"]["model"] = json!("qemu64");
    spec["memory"]["size_mib"] = json!(128);
    let id = d.post("/vms", json!({ "spec": spec })).unwrap()["summary"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    d.post(&format!("/vms/{id}/actions/start"), json!({}))
        .unwrap();
    d.wait_state(&id, "running");
    let pid = qemu_pid(&data, &id);

    // Whatever happens below, never leave QEMU running.
    struct Cleanup(u32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let mut sys = System::new().unwrap();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[Pid::from_u32(self.0)]),
                true,
                ProcessRefreshKind::nothing(),
            );
            if let Some(p) = sys.process(Pid::from_u32(self.0)) {
                p.kill();
            }
        }
    }
    let _cleanup = Cleanup(pid);

    let ways: &[&str] = if cfg!(unix) {
        &["term", "kill"]
    } else {
        &["kill"]
    };
    let mut d = d;
    for how in ways {
        d.kill(how);
        std::thread::sleep(Duration::from_millis(500));
        assert!(qemu_alive(pid), "QEMU died with the daemon ({how})");
        d = Daemon::start(&data, port);
        d.wait_state(&id, "running");
        assert_eq!(
            qemu_pid(&data, &id),
            pid,
            "reattached to the same QEMU ({how})"
        );
    }

    d.post(&format!("/vms/{id}/actions/force-stop"), json!({}))
        .unwrap();
    d.wait_state(&id, "stopped");
    d.kill("kill");
    let _ = std::fs::remove_dir_all(&data);
}
