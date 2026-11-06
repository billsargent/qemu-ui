# qemu-ui

A browser-based control plane for QEMU: every architecture, every machine type, every
command-line option, real disk/network/display management, and VM sessions that survive
closing the browser.

> **Status: early development (milestone M1).** Create, configure, run and use VMs from
> the browser: a spec editor that shows the exact QEMU command line, "Check with QEMU"
> validation, browser graphics and serial consoles, and VMs that keep running when the
> daemon or browser goes away. Storage, networking and hardening follow in M2–M6. See
> [plan.md](plan.md) for the full specification and roadmap.

## Quick start

Everything the project needs is installed into the project folder under `.tools/`
(gitignored). Nothing is installed system-wide.

```sh
scripts/bootstrap.sh          # Rust, a C toolchain (Windows), QEMU (Windows), web deps
source scripts/env.sh         # put .tools on PATH for this shell

cargo run -p qemu-ui-daemon   # starts qemu-uid on http://127.0.0.1:7480
```

In a second shell:

```sh
cd web && npm run dev         # UI on http://127.0.0.1:5173, proxied to the daemon
# or: npm run build, then open http://127.0.0.1:7480 (the daemon serves web/dist)

cargo run -p qemu-ui-cli -- host   # the same data from the CLI
cargo run -p qemu-ui-cli -- vms    # list VMs; also start/stop/force-stop/pause/resume <name>
```

The API accepts same-origin requests from the web UI. Other clients (the CLI,
curl) authenticate with the local token the daemon writes to `api-token` in its
data directory; the CLI finds it automatically (`--data-dir` if you changed it).

On Windows, run these from Git Bash. On Linux, install QEMU from your distribution
(`apt install qemu-system qemu-utils ovmf`, or your distribution's equivalents). The daemon
looks in `.tools/qemu`, then any `--qemu-dir`, then `PATH`.

## Layout

| Path | What |
|---|---|
| `crates/qemu-ui-core` | Pure types and parsers: capability model, firmware descriptors, parity ledger |
| `crates/qemu-ui-qmp` | Async QMP client |
| `crates/qemu-ui-daemon` | `qemu-uid`: discovery, probe, store, HTTP API |
| `crates/qemu-ui-cli` | `qemu-ui`: CLI over the same HTTP API |
| `web/` | React + TypeScript UI. API types in `src/api/generated` are generated from Rust |
| `data/` | Data-driven behaviour: `parity.toml` (platform support ledger) |
| `scripts/` | `bootstrap.sh`, `env.sh` |

## Security

qemu-ui can start processes with access to host disks and devices, so it is
**root-equivalent**. Until authentication and TLS land (milestone M6), the daemon refuses to
listen on anything but loopback. See plan.md §13.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
