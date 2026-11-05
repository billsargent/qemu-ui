# Contributing

## Setup

```sh
scripts/bootstrap.sh     # idempotent; installs into .tools/
source scripts/env.sh
```

- **Windows** uses the `x86_64-pc-windows-gnu` Rust host and w64devkit, so Visual Studio is
  not required. Use Git Bash. If you already have the MSVC toolchain, that works too.
- **Linux** uses your system C compiler and distribution QEMU packages.

Tool versions are pinned in `scripts/bootstrap.sh`. Change them in a dedicated commit.

## Checks

Run these before opening a PR; CI runs the same on Windows and Linux:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # also regenerates web/src/api/generated
(cd web && npm run build)
```

If `cargo test` changes files under `web/src/api/generated`, commit them; CI fails when
they are stale.

Tests that need a real QEMU (`qemu-ui-qmp/tests/real_qemu.rs`, the probe smoke test) pass
without running anything when no `qemu-system-x86_64` is found. CI always has one.

## Conventions

- **Work items**: every PR references the plan's work item IDs and issues
  (`Closes #5 (M0-04)`). Plan changes go through PRs to `plan.md`.
- **Capabilities are data.** No `if windows` in the UI. Platform differences go in
  `data/parity.toml` or come from the probe.
- **Never a shell.** QEMU is spawned with argv arrays; user values never pass through a
  shell or string interpolation.
- **Keep `qemu-ui-core` I/O-free** so it stays deterministic and unit-testable.
- **No personal data** in commits, issues or logs: strip local paths, hostnames and
  usernames from anything you paste.
