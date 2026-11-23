# Command-line client and config-as-code

`qemu-ui` speaks the same HTTP API as the web UI.

## Connecting

| Option | Env | Meaning |
|---|---|---|
| `--url` | `QEMU_UI_URL` | Daemon URL (default `http://127.0.0.1:7480`) |
| `--token` | `QEMU_UI_TOKEN` | API token. Create one with `qemu-ui tokens create <name>` or on the System page. |
| `--data-dir` | `QEMU_UI_DATA_DIR` | Without `--token`, the local bootstrap token is read from `<data-dir>/api-token` (works from the daemon's own machine only). |
| `--ca-cert` | `QEMU_UI_CA_CERT` | CA to trust for `https://` URLs. Download it from `/api/v1/tls/ca.pem`. On the daemon's machine `<data-dir>/tls/ca.pem` is used automatically. |

`--json` prints the raw API reply for any listing.

## Commands

```
qemu-ui host | rescan
qemu-ui vms
qemu-ui vm show|create|update|delete|clone|argv|serial-log ...
qemu-ui start|stop|force-stop|pause|resume|reset <vm>
qemu-ui networks [list | delete <net>]
qemu-ui snapshots list|take|revert|delete <vm> ...
qemu-ui pools
qemu-ui jobs [list | cancel <id>]
qemu-ui tokens [list | create <name> | revoke <id>]
qemu-ui audit [--limit N]
qemu-ui export [-o file.json|file.toml] [--toml]
qemu-ui apply <file> [--dry-run] [--prune]
qemu-ui service install|uninstall|status
```

VMs, networks and snapshots can be named by name or id.

## Manifests

`qemu-ui export` writes networks and VMs as a manifest (JSON, or TOML for a
`.toml` file). `qemu-ui apply` makes the daemon match one, and applying the
same manifest twice changes nothing.

```toml
manifest_version = 1

[[networks]]
name = "lab"
config = { kind = "nat", subnet = "10.77.0.0/24" }

[[vms]]
name = "web"
arch = "x86_64"
memory = { size_mib = 2048 }
nics = [{ backend = { kind = "network", network = "lab" } }]
```

- Everything is matched by name. NICs name their network, so a manifest
  works on another host.
- A VM entry may be partial. Fields it leaves out keep their current value,
  or the daemon's default for a new VM (`arch` is required to create one).
  Daemon-assigned values such as MAC addresses are kept, so they never show
  up as changes.
- Networks and VMs missing from the manifest are left alone, unless you pass
  `--prune`.
- Disk images are referenced, not created: create them first (UI, or
  `qemu-ui` against the storage API).
- `--dry-run` prints the plan: `+` create, `~` change (with the fields), `-`
  delete. Without it, changes run in order and stop at the first failure.
