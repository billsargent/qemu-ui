//! Lint matrix (plan.md §6.3, M4-06): every installed `qemu-system-*`
//! binary checks the specs qemu-ui generates for its architecture through
//! the real dry lint. Opt-in (`QEMU_UI_LINT_MATRIX=1`) because it probes
//! every binary; CI runs it in its own job.
//!
//! Binaries are found in `QEMU_UI_QEMU_DIR` when set, else on `PATH`.
//! `QEMU_UI_LINT_MATRIX_ARCHS=x86_64,aarch64` limits the run.
//!
//! The golden spec corpus (X-01, `data/corpus/*.json`) runs in the same
//! job: one hand-written spec per architecture and machine family, built on
//! the editor's default for that binary, pinned to the named machine, with
//! the listed disk and CD-ROM buses and an optional JSON merge patch. Every
//! corpus spec whose binary is installed must pass dry lint, unless it
//! carries a `known_failure` saying why not.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use qemu_ui_core::caps::{Binary, HostCaps, HostInfo};
    use qemu_ui_core::spec::{CacheMode, DiskBus, DiskSpec, ImageRef, Media, VmSpec};

    use crate::supervisor::test_support::*;

    /// Architectures whose generated spec is known not to lint, with why.
    /// Anything failing outside this list fails the test.
    const KNOWN: &[(&str, &str)] = &[(
        "m68k",
        "the default machine (mcf5208evb) only starts with -kernel; direct kernel boot is not supported yet",
    )];

    /// QEMU system-emulation targets. Distributions add other names (Debian:
    /// `arm64`, `armhf`, `x86_64-microvm`, …) that are wrappers or links to
    /// these, so only real target names count.
    const TARGETS: &[&str] = &[
        "aarch64",
        "alpha",
        "arm",
        "avr",
        "hexagon",
        "hppa",
        "i386",
        "loongarch64",
        "m68k",
        "microblaze",
        "microblazeel",
        "mips",
        "mips64",
        "mips64el",
        "mipsel",
        "or1k",
        "ppc",
        "ppc64",
        "riscv32",
        "riscv64",
        "rx",
        "s390x",
        "sh4",
        "sh4eb",
        "sparc",
        "sparc64",
        "tricore",
        "x86_64",
        "xtensa",
        "xtensaeb",
    ];

    /// `qemu-system-<arch>` binaries, one per arch.
    fn binaries() -> Vec<(String, PathBuf)> {
        let dirs: Vec<PathBuf> = match std::env::var_os("QEMU_UI_QEMU_DIR") {
            Some(d) => vec![PathBuf::from(d)],
            None => std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
        };
        let only: Option<Vec<String>> = std::env::var("QEMU_UI_LINT_MATRIX_ARCHS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.split(',').map(|a| a.trim().to_string()).collect());
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut found: Vec<(String, PathBuf)> = rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let stem = name.strip_suffix(".exe").unwrap_or(&name);
                    let arch = stem.strip_prefix("qemu-system-")?;
                    // Windows ships console-less `…w.exe` twins; skip them.
                    if cfg!(windows)
                        && arch.ends_with('w')
                        && dir
                            .join(format!("qemu-system-{}.exe", &arch[..arch.len() - 1]))
                            .exists()
                    {
                        return None;
                    }
                    Some((arch.to_string(), e.path()))
                })
                .collect();
            found.sort();
            for (arch, path) in found {
                if !TARGETS.contains(&arch.as_str()) || out.iter().any(|(a, _)| a == &arch) {
                    continue;
                }
                if only.as_ref().is_some_and(|o| !o.contains(&arch)) {
                    continue;
                }
                out.push((arch, path));
            }
        }
        out
    }

    async fn caps_for(arch: &str, qemu: &Path, img: &Path) -> Result<HostCaps, String> {
        let host = crate::scan::host_info();
        let bin = Binary {
            id: arch.into(),
            path: qemu.display().to_string(),
            arch: Some(arch.into()),
            version: String::new(),
            sha256: String::new(),
            source: "path".into(),
        };
        let caps = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            crate::probe::probe_binary(bin, &host),
        )
        .await
        .map_err(|_| "probe timed out".to_string())?
        .map_err(|e| format!("probe failed: {e:#}"))?;
        Ok(HostCaps {
            host: HostInfo {
                os: host.os,
                arch: host.arch,
            },
            binaries: vec![caps],
            failed: vec![],
            qemu_img: Some(Binary {
                id: "img".into(),
                path: img.display().to_string(),
                arch: None,
                version: String::new(),
                sha256: String::new(),
                source: "path".into(),
            }),
            firmware: vec![],
            parity: vec![],
            scanned_at: 0,
        })
    }

    /// The specs to lint for one arch: the editor's default, and the default
    /// plus a disk on the machine's preferred available bus.
    async fn specs(
        sv: &crate::supervisor::Supervisor,
        caps: &HostCaps,
    ) -> Vec<(&'static str, VmSpec)> {
        let bc = &caps.binaries[0];
        // As the "new VM" form gets it (see `api_vms::defaults`).
        let (probe, _) = qemu_ui_core::vm::default_spec("matrix", bc);
        let machine_buses = sv.machine_buses(bc, &probe.machine.type_name).await;
        let (mut base, _) = qemu_ui_core::vm::default_spec_on("matrix", bc, |_| machine_buses);
        base.accel = "tcg".into(); // hardware may be absent on runners
        if base.cpu.model.as_deref() == Some("host") {
            base.cpu.model = None;
        }
        base.memory.size_mib = 256;
        let mut out = vec![("default", base.clone())];

        let buses = sv.machine_buses(bc, &base.machine.type_name).await;
        let bus = match &buses {
            Some(m) => {
                let support = m.disk_buses(bc);
                [
                    DiskBus::Virtio,
                    DiskBus::Scsi,
                    DiskBus::Ide,
                    DiskBus::Nvme,
                    DiskBus::Usb,
                ]
                .into_iter()
                .find(|b| support.iter().any(|s| s.bus == *b && s.available))
            }
            None => Some(DiskBus::Virtio),
        };
        if let Some(bus) = bus {
            let mut with_disk = base;
            with_disk.disks.push(DiskSpec {
                image: ImageRef {
                    pool: "default".into(),
                    name: "matrix.qcow2".into(),
                },
                bus,
                media: Media::Disk,
                readonly: false,
                boot_index: Some(1),
                cache: CacheMode::None,
                aio: None,
                discard: false,
                slot: None,
            });
            out.push(("disk", with_disk));
        }
        out
    }

    #[test]
    fn lint_matrix_across_installed_archs() {
        if std::env::var("QEMU_UI_LINT_MATRIX").as_deref() != Ok("1") {
            eprintln!("skipping: set QEMU_UI_LINT_MATRIX=1 to run the lint matrix");
            return;
        }
        let Some(img) = find("qemu-img") else {
            panic!("the lint matrix needs qemu-img");
        };
        let bins = binaries();
        assert!(!bins.is_empty(), "no qemu-system-* binaries found");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut failures = Vec::new();
        let mut table = Vec::new();
        for (arch, qemu) in &bins {
            let data = short_data_dir(&format!("lm{}", arch.len()));
            let result: Result<Vec<(&str, bool, String)>, String> = rt.block_on(async {
                let caps = caps_for(arch, qemu, &img).await?;
                let sv = make(&data, &caps).await;
                sv.pools
                    .create(
                        &img,
                        &ImageRef {
                            pool: "default".into(),
                            name: "matrix.qcow2".into(),
                        },
                        32 << 20,
                        "qcow2",
                    )
                    .await
                    .map_err(|e| format!("creating the test image: {e:#}"))?;
                let mut rows = Vec::new();
                for (variant, spec) in specs(&sv, &caps).await {
                    let r = tokio::time::timeout(
                        std::time::Duration::from_secs(120),
                        sv.lint(None, spec),
                    )
                    .await
                    .map_err(|_| format!("{variant}: lint timed out"))?;
                    match r {
                        Ok(r) => {
                            let first = r
                                .diagnostics
                                .iter()
                                .find(|d| d.severity == qemu_ui_core::compile::Severity::Error)
                                .map(|d| format!("{} ({}): {}", d.rule, d.field, d.message))
                                .unwrap_or_default();
                            rows.push((variant, r.ok, first));
                        }
                        Err(e) => rows.push((variant, false, format!("{e:?}"))),
                    }
                }
                Ok(rows)
            });
            let _ = std::fs::remove_dir_all(&data);
            let known = KNOWN.iter().find(|(a, _)| a == arch);
            match result {
                Ok(rows) => {
                    for (variant, ok, why) in rows {
                        table.push(format!(
                            "{arch:<14} {variant:<8} {}",
                            if ok {
                                "ok".to_string()
                            } else {
                                format!("FAIL {why}")
                            }
                        ));
                        if !ok && known.is_none() {
                            failures.push(format!("{arch}/{variant}: {why}"));
                        }
                    }
                }
                Err(e) => {
                    table.push(format!("{arch:<14} -        FAIL {e}"));
                    if known.is_none() {
                        failures.push(format!("{arch}: {e}"));
                    }
                }
            }
        }
        eprintln!("lint matrix:\n{}", table.join("\n"));
        assert!(
            failures.is_empty(),
            "unexpected lint failures:\n{}",
            failures.join("\n")
        );
    }

    /// One corpus entry (`data/corpus/<name>.json`).
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CorpusEntry {
        arch: String,
        machine: String,
        #[serde(default)]
        disk: Option<DiskBus>,
        #[serde(default)]
        cdrom: Option<DiskBus>,
        /// RFC 7386 merge patch over the built spec.
        #[serde(default)]
        patch: Option<serde_json::Value>,
        #[serde(default)]
        known_failure: Option<String>,
        #[allow(dead_code)]
        note: String,
    }

    fn corpus() -> Vec<(String, CorpusEntry)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/corpus");
        let mut out: Vec<(String, CorpusEntry)> = std::fs::read_dir(&dir)
            .expect("data/corpus")
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                let text = std::fs::read_to_string(&p).unwrap();
                let e: CorpusEntry = serde_json::from_str(&text)
                    .unwrap_or_else(|err| panic!("data/corpus/{name}.json: {err}"));
                (name, e)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
        match (target, patch) {
            (serde_json::Value::Object(t), serde_json::Value::Object(p)) => {
                for (k, v) in p {
                    if v.is_null() {
                        t.remove(k);
                    } else {
                        merge_patch(t.entry(k.clone()).or_insert(serde_json::Value::Null), v);
                    }
                }
            }
            (t, p) => *t = p.clone(),
        }
    }

    fn disk(name: &str, bus: DiskBus, media: Media, boot: u32) -> DiskSpec {
        DiskSpec {
            image: ImageRef {
                pool: "default".into(),
                name: name.into(),
            },
            bus,
            media,
            readonly: media == Media::Cdrom,
            boot_index: Some(boot),
            cache: CacheMode::None,
            aio: None,
            discard: false,
            slot: None,
        }
    }

    /// Build a corpus entry's spec for this binary.
    async fn corpus_spec(
        sv: &crate::supervisor::Supervisor,
        caps: &HostCaps,
        e: &CorpusEntry,
    ) -> Result<VmSpec, String> {
        let bc = &caps.binaries[0];
        let machine = qemu_ui_core::compile::pin_machine(&e.machine, bc)?;
        let buses = sv.machine_buses(bc, &machine).await;
        let (mut spec, _) = qemu_ui_core::vm::default_spec_on("corpus", bc, |_| buses);
        spec.machine.type_name = machine;
        spec.accel = "tcg".into();
        if spec.cpu.model.as_deref() == Some("host") {
            spec.cpu.model = None;
        }
        spec.memory.size_mib = 256;
        if let Some(bus) = e.disk {
            spec.disks.push(disk("corpus.qcow2", bus, Media::Disk, 1));
        }
        if let Some(bus) = e.cdrom {
            spec.disks.push(disk("corpus.iso", bus, Media::Cdrom, 2));
        }
        if let Some(p) = &e.patch {
            let mut v = serde_json::to_value(&spec).map_err(|e| e.to_string())?;
            merge_patch(&mut v, p);
            spec = serde_json::from_value(v).map_err(|e| format!("patch: {e}"))?;
        }
        Ok(spec)
    }

    /// The corpus files are well-formed (runs everywhere, no QEMU needed).
    #[test]
    fn corpus_files_parse() {
        let c = corpus();
        assert!(c.len() >= 8, "the corpus covers the main machine families");
        for (name, e) in &c {
            assert!(TARGETS.contains(&e.arch.as_str()), "{name}: unknown arch");
            assert!(
                name.starts_with(&format!("{}-", e.arch)),
                "{name}: file names start with the arch"
            );
        }
    }

    type Row = (String, Result<(), String>);

    #[test]
    fn lint_matrix_golden_corpus() {
        if std::env::var("QEMU_UI_LINT_MATRIX").as_deref() != Ok("1") {
            eprintln!("skipping: set QEMU_UI_LINT_MATRIX=1 to run the golden corpus");
            return;
        }
        let Some(img) = find("qemu-img") else {
            panic!("the golden corpus needs qemu-img");
        };
        let bins = binaries();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut failures = Vec::new();
        let mut table = Vec::new();
        let entries = corpus();
        let archs: std::collections::BTreeSet<String> =
            entries.iter().map(|(_, e)| e.arch.clone()).collect();
        for arch in archs {
            let mine: Vec<&(String, CorpusEntry)> =
                entries.iter().filter(|(_, e)| e.arch == arch).collect();
            let Some((_, qemu)) = bins.iter().find(|(a, _)| *a == arch) else {
                for (name, _) in mine {
                    table.push(format!("{name:<28} skipped (no qemu-system-{arch})"));
                }
                continue;
            };
            let data = short_data_dir(&format!("gc{}", arch.len()));
            let rows: Result<Vec<Row>, String> = rt.block_on(async {
                let caps = caps_for(&arch, qemu, &img).await?;
                let sv = make(&data, &caps).await;
                for n in ["corpus.qcow2", "corpus.iso"] {
                    let format = if n.ends_with(".iso") { "raw" } else { "qcow2" };
                    sv.pools
                        .create(
                            &img,
                            &ImageRef {
                                pool: "default".into(),
                                name: n.into(),
                            },
                            32 << 20,
                            format,
                        )
                        .await
                        .map_err(|e| format!("creating {n}: {e:#}"))?;
                }
                let mut rows = Vec::new();
                for (name, e) in &mine {
                    let r = match corpus_spec(&sv, &caps, e).await {
                        Err(err) => Err(err),
                        Ok(spec) => match tokio::time::timeout(
                            std::time::Duration::from_secs(120),
                            sv.lint(None, spec),
                        )
                        .await
                        {
                            Err(_) => Err("lint timed out".into()),
                            Ok(Err(err)) => Err(format!("{err:?}")),
                            Ok(Ok(r)) if r.ok => Ok(()),
                            Ok(Ok(r)) => Err(r
                                .diagnostics
                                .iter()
                                .find(|d| d.severity == qemu_ui_core::compile::Severity::Error)
                                .map(|d| format!("{} ({}): {}", d.rule, d.field, d.message))
                                .unwrap_or_else(|| "not ok".into())),
                        },
                    };
                    rows.push((name.clone(), r));
                }
                Ok(rows)
            });
            let _ = std::fs::remove_dir_all(&data);
            match rows {
                Err(err) => {
                    for (name, e) in mine {
                        table.push(format!("{name:<28} FAIL {err}"));
                        if e.known_failure.is_none() {
                            failures.push(format!("{name}: {err}"));
                        }
                    }
                }
                Ok(rows) => {
                    for (name, r) in rows {
                        let known = mine
                            .iter()
                            .find(|(n, _)| *n == name)
                            .and_then(|(_, e)| e.known_failure.as_ref());
                        match (r, known) {
                            (Ok(()), None) => table.push(format!("{name:<28} ok")),
                            (Ok(()), Some(_)) => {
                                table.push(format!("{name:<28} ok (marked known_failure)"));
                                failures
                                    .push(format!("{name}: passes now; remove its known_failure"));
                            }
                            (Err(err), Some(why)) => {
                                table.push(format!("{name:<28} known failure: {why} ({err})"))
                            }
                            (Err(err), None) => {
                                table.push(format!("{name:<28} FAIL {err}"));
                                failures.push(format!("{name}: {err}"));
                            }
                        }
                    }
                }
            }
        }
        eprintln!("golden corpus:\n{}", table.join("\n"));
        assert!(
            failures.is_empty(),
            "golden corpus failures:\n{}",
            failures.join("\n")
        );
    }
}
