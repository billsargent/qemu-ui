//! Real-QEMU tests for image operations (M2-04), through the HTTP router.

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use qemu_ui_core::spec::ImageRef;
use qemu_ui_core::vm::VmState;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::supervisor::test_support::*;

const T: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

async fn call(router: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7480")
        .header("authorization", format!("Bearer {T}"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let req = b
        .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Start an operation and wait for its job; returns the finished job.
async fn run(router: &axum::Router, uri: &str, body: Value) -> Value {
    let (s, job) = call(router, "POST", uri, Some(body)).await;
    assert_eq!(s, 202, "{uri}: {job}");
    let id = job["id"].as_str().unwrap().to_string();
    for _ in 0..400 {
        let (_, j) = call(router, "GET", &format!("/api/v1/jobs/{id}"), None).await;
        if !matches!(j["status"].as_str(), Some("queued" | "running")) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {id} did not finish");
}

async fn qimg(img: &Path, args: &[&str]) {
    let ok = crate::probe::command(img)
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(ok.success(), "qemu-img {args:?}");
}

async fn vsize(router: &axum::Router, pool: &str, name: &str) -> u64 {
    let (_, d) = call(
        router,
        "GET",
        &format!("/api/v1/pools/{pool}/images/{name}"),
        None,
    )
    .await;
    d["virtual_size"].as_u64().unwrap()
}

#[test]
fn resize_compact_rename() {
    let (Some(qemu), Some(img)) = (find("qemu-system-x86_64"), find("qemu-img")) else {
        eprintln!("skipping: qemu not found");
        return;
    };
    let data = short_data_dir("iops");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let caps = host_caps(&qemu, &img).await;
        let sv = make(&data, &caps).await;
        let app = crate::api::App::new(sv.scanner.clone(), sv.clone(), T.into());
        let r = crate::api::router(app, None);
        let dir = sv.pools.pool_dir("default").unwrap();
        let p = |n: &str| dir.join(n).display().to_string();
        const MIB: u64 = 1 << 20;

        // --- offline resize
        qimg(&img, &["create", "-q", "-f", "qcow2", &p("a.qcow2"), "64M"]).await;
        let j = run(
            &r,
            "/api/v1/pools/default/images/a.qcow2/resize",
            json!({"size_bytes": 128 * MIB}),
        )
        .await;
        assert_eq!(j["status"], "succeeded", "{j}");
        assert_eq!(vsize(&r, "default", "a.qcow2").await, 128 * MIB);
        let (s, e) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/a.qcow2/resize",
            Some(json!({"size_bytes": 96 * MIB})),
        )
        .await;
        assert_eq!(s, 422, "shrink needs confirmation: {e}");
        let j = run(
            &r,
            "/api/v1/pools/default/images/a.qcow2/resize",
            json!({"size_bytes": 96 * MIB, "shrink": true}),
        )
        .await;
        assert_eq!(j["status"], "succeeded", "{j}");
        assert_eq!(vsize(&r, "default", "a.qcow2").await, 96 * MIB);
        qimg(&img, &["create", "-q", "-f", "raw", &p("r.img"), "8M"]).await;
        let j = run(
            &r,
            "/api/v1/pools/default/images/r.img/resize",
            json!({"size_bytes": 16 * MIB}),
        )
        .await;
        assert_eq!(
            (j["status"].as_str(), vsize(&r, "default", "r.img").await),
            (Some("succeeded"), 16 * MIB)
        );

        // --- compact drops allocated clusters that contain only zeros
        // (preallocation=full is unavailable on Windows hosts).
        qimg(
            &img,
            &["create", "-q", "-f", "qcow2", &p("fat.qcow2"), "32M"],
        )
        .await;
        let io = find("qemu-io").expect("qemu-io ships with qemu-img");
        let ok = crate::probe::command(&io)
            .args(["-f", "qcow2", "-c", "write -P 0 0 16M"])
            .arg(dir.join("fat.qcow2"))
            .status()
            .await
            .unwrap();
        assert!(ok.success());
        let before = std::fs::metadata(dir.join("fat.qcow2")).unwrap().len();
        let j = run(
            &r,
            "/api/v1/pools/default/images/fat.qcow2/compact",
            json!({}),
        )
        .await;
        assert_eq!(j["status"], "succeeded", "{j}");
        let after = std::fs::metadata(dir.join("fat.qcow2")).unwrap().len();
        assert!(
            after < before / 4,
            "compaction reclaimed space: {before} -> {after}"
        );
        assert_eq!(
            vsize(&r, "default", "fat.qcow2").await,
            32 * MIB,
            "guest-visible size unchanged"
        );
        let (_, c) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/fat.qcow2/check",
            None,
        )
        .await;
        assert_eq!(c["ok"], true, "{c}");
        assert!(
            !std::fs::read_dir(&dir).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".compact-")),
            "no temp files left"
        );

        // --- an overlay keeps its backing file through compaction
        qimg(
            &img,
            &["create", "-q", "-f", "qcow2", &p("base.qcow2"), "16M"],
        )
        .await;
        qimg(
            &img,
            &[
                "create",
                "-q",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                "base.qcow2",
                &p("top.qcow2"),
            ],
        )
        .await;
        let io = Some(io);
        if let Some(io) = &io {
            // Some guest data in the overlay, so there is something to copy.
            let ok = crate::probe::command(io)
                .args(["-f", "qcow2", "-c", "write -P 0x5a 0 1M"])
                .arg(dir.join("top.qcow2"))
                .status()
                .await
                .unwrap();
            assert!(ok.success());
        }
        let j = run(
            &r,
            "/api/v1/pools/default/images/top.qcow2/compact",
            json!({"compress": true}),
        )
        .await;
        assert_eq!(j["status"], "succeeded", "{j}");
        let (_, d) = call(&r, "GET", "/api/v1/pools/default/images/top.qcow2", None).await;
        assert_eq!(
            d["backing_chain"][0]["name"], "base.qcow2",
            "chain intact: {d}"
        );
        if let Some(io) = &io {
            let out = crate::probe::command(io)
                .args(["-f", "qcow2", "-c", "read -P 0x5a 0 1M"])
                .arg(dir.join("top.qcow2"))
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success()
                    && !String::from_utf8_lossy(&out.stdout)
                        .contains("Pattern verification failed"),
                "data preserved"
            );
        }

        // --- refusals
        let (s, e) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/base.qcow2/resize",
            Some(json!({"size_bytes": 32 * MIB})),
        )
        .await;
        assert_eq!(s, 409, "backing files keep their size: {e}");
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/base.qcow2/rename",
            Some(json!({"new_name": "x.qcow2"})),
        )
        .await;
        assert_eq!(s, 409, "backing files keep their name");
        qimg(&img, &["snapshot", "-c", "s1", &p("a.qcow2")]).await;
        let (s, e) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/a.qcow2/compact",
            Some(json!({})),
        )
        .await;
        assert_eq!(s, 409, "snapshots would be lost: {e}");

        // --- rename
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/r.img/rename",
            Some(json!({"new_name": "renamed.img"})),
        )
        .await;
        assert_eq!(s, 204);
        assert!(dir.join("renamed.img").exists() && !dir.join("r.img").exists());
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/renamed.img/rename",
            Some(json!({"new_name": "../out.img"})),
        )
        .await;
        assert_eq!(s, 422, "traversal refused");

        // --- live grow of a running VM's disk
        let id = small_vm(&sv, &caps, "live").await;
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/live.qcow2/rename",
            Some(json!({"new_name": "y.qcow2"})),
        )
        .await;
        assert_eq!(s, 409, "referenced by a VM");
        sv.start(&id).await.map_err(|e| format!("{e:?}")).unwrap();
        let j = run(
            &r,
            "/api/v1/pools/default/images/live.qcow2/resize",
            json!({"size_bytes": 64 * MIB}),
        )
        .await;
        assert_eq!(j["status"], "succeeded", "{j}");
        assert!(j["title"].as_str().unwrap().contains("live"), "{j}");
        // QEMU itself reports the new size.
        let (_, qmp, node, _) = sv
            .live_disks(&ImageRef {
                pool: "default".into(),
                name: "live.qcow2".into(),
            })
            .remove(0);
        let nodes = qmp.execute("query-named-block-nodes", None).await.unwrap();
        let n = nodes
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["node-name"] == node.as_str())
            .unwrap()
            .clone();
        assert_eq!(n["image"]["virtual-size"].as_u64(), Some(64 * MIB), "{n}");
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/live.qcow2/resize",
            Some(json!({"size_bytes": 48 * MIB, "shrink": true})),
        )
        .await;
        assert_eq!(s, 409, "no live shrink");
        let (s, _) = call(
            &r,
            "POST",
            "/api/v1/pools/default/images/live.qcow2/compact",
            Some(json!({})),
        )
        .await;
        assert_eq!(s, 409, "no compaction while open");
        sv.force_stop(&id)
            .await
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        wait_state(&sv, &id, VmState::Stopped).await;
        assert_eq!(
            vsize(&r, "default", "live.qcow2").await,
            64 * MIB,
            "persisted after stop"
        );
    });
    drop(rt);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn one_operation_per_image() {
    let r = ImageRef {
        pool: "p".into(),
        name: "x.qcow2".into(),
    };
    let g = super::BusyGuard::acquire(&r).expect("first acquire");
    assert!(super::BusyGuard::acquire(&r).is_err());
    drop(g);
    assert!(super::BusyGuard::acquire(&r).is_ok());
}
