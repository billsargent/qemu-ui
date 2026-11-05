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
