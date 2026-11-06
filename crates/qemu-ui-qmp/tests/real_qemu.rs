//! Integration test against a real QEMU binary. Skipped (passes vacuously)
//! when no `qemu-system-x86_64` is found in `QEMU_UI_QEMU_DIR` or `PATH`.

use std::path::PathBuf;
use std::process::Stdio;

use qemu_ui_qmp::Qmp;

fn find_qemu() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("QEMU_UI_QEMU_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

#[tokio::test]
async fn query_real_qemu_over_stdio() {
    let Some(qemu) = find_qemu() else {
        eprintln!("skipping: qemu-system-x86_64 not found");
        return;
    };
    let mut child = tokio::process::Command::new(qemu)
        .args([
            "-machine",
            "none",
            "-nodefaults",
            "-display",
            "none",
            "-S",
            "-qmp",
            "stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let qmp = Qmp::from_child(&mut child).await.unwrap();
    assert!(qmp.greeting().version.major >= 8);

    let machines = qmp.execute("query-machines", None).await.unwrap();
    let names: Vec<&str> = machines
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert!(names.iter().any(|n| n.starts_with("pc-q35-")), "{names:?}");

    let err = qmp.execute("no-such-command", None).await.unwrap_err();
    assert!(err.is_command_not_found());

    let _ = qmp.execute("quit", None).await;
    let status = child.wait().await.unwrap();
    assert!(status.success());
}

/// Spike M0-14 regression: QMP on an AF_UNIX socket must accept a new client
/// after the previous one disconnects (daemon restart → reattach), on Linux
/// and Windows alike.
#[tokio::test]
async fn unix_socket_qmp_reconnects() {
    let Some(qemu) = find_qemu() else {
        eprintln!("skipping: qemu-system-x86_64 not found");
        return;
    };
    // Short path: AF_UNIX is limited to 108 bytes.
    let dir = std::env::temp_dir().join(format!("qui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("q.sock");
    if qemu_ui_qmp::transport::check_unix_path(&sock).is_err() {
        eprintln!("skipping: temp dir path too long for AF_UNIX");
        return;
    }
    let chardev = format!("socket,id=qmp,path={},server=on,wait=off", sock.display());
    let mut child = tokio::process::Command::new(qemu)
        .args(["-machine", "none", "-nodefaults", "-display", "none", "-S"])
        .args(["-chardev", &chardev, "-mon", "chardev=qmp,mode=control"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    for round in 0..3 {
        let mut qmp = None;
        for _ in 0..50 {
            if let Ok(q) = Qmp::connect_unix(&sock).await {
                qmp = Some(q);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let qmp = qmp.unwrap_or_else(|| panic!("round {round}: could not connect"));
        let st = qmp.execute("query-status", None).await.unwrap();
        assert_eq!(st["status"], "prelaunch", "round {round}");
        drop(qmp);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
}
