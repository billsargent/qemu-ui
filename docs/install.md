# Install

qemu-ui is built from source. Everything the build needs is installed inside
the project folder under `.tools/`; nothing is installed system-wide.

## Build

```sh
scripts/bootstrap.sh          # Rust, a C toolchain (Windows), QEMU (Windows), web deps
source scripts/env.sh         # put .tools on PATH for this shell

cargo build --release -p qemu-ui-daemon -p qemu-ui-cli
(cd web && npm run build)     # the daemon serves web/dist
```

On Windows run these from Git Bash. On Linux install QEMU from your
distribution, for example `apt install qemu-system qemu-utils ovmf`, plus
`swtpm` for TPM 2.0, `passt` for passt networking and `virtiofsd` for shared
folders if you want them.

The daemon looks for QEMU in `.tools/qemu`, then in each `--qemu-dir`, then on
`PATH`. The System page lists what it found and what each binary can do.

## Run

```sh
qemu-uid                       # http://127.0.0.1:7480
```

Useful options (each also has an environment variable, see `qemu-uid --help`):

| Option | Meaning |
|---|---|
| `--listen` | Address to listen on. Default `127.0.0.1:7480`. |
| `--data-dir` | Database, VM directories, logs, TLS files. |
| `--qemu-dir` | Extra directory to search for QEMU binaries (repeatable). |
| `--tls auto\|on\|off` | `auto`: HTTPS beyond loopback, plain HTTP on loopback. |
| `--tls-cert`, `--tls-key` | Your own certificate instead of the generated one. |

## First-run setup

Open the UI from the same machine. Until setup is done a banner links to the
setup page, where you create the single admin account. Setup is only accepted
from loopback.

The daemon refuses to listen on anything but loopback until setup is done.
After that, `--listen 0.0.0.0:7480` serves HTTPS with a generated certificate;
download the CA from the System page (HTTPS card) and trust it on each device.

## Run as a service

```sh
qemu-ui service install --dry-run   # print every step and the unit files
qemu-ui service install             # needs root / an elevated shell
qemu-ui service status
```

- **Linux:** systemd units for the daemon (a dedicated system user, plus the
  `kvm` group when present) and the privileged helper. `KillMode=process`, so
  restarting the daemon leaves VMs running.
- **Windows:** a service running as `NT SERVICE\qemu-ui`, granted access to the
  data directory. Stopping the service leaves VMs running.

See [Process survival](process-survival.md) for the details.

## CLI

Create an API token on the System page or with `qemu-ui tokens create <name>`,
then see [Command-line client](cli.md).
