# Privileged helper (M2-10) and physical disks (M2-11)

Status: design, for review. Implementation follows in the same work items.

## Why a helper

The daemon is root-equivalent through what it lets users configure, but it
should not *run* with privileges: a bug in a web-facing process with root
rights is a host compromise. A few operations do need privileges:

| Operation | Linux | Windows |
|---|---|---|
| Open a physical disk | root or `disk` group | Administrator |
| Create a tap device and attach it to a bridge | `CAP_NET_ADMIN` | (TAP-Windows adapters are created by its installer; opening one needs no admin) |
| Per-VM user and cgroup (M6-05) | root | n/a |

`qemu-ui-helper` is a small, separately installed process that performs only
these operations, for the daemon only, after validating every request
against an administrator-owned configuration. A compromised daemon can make
the helper do exactly what the configuration already allows, and nothing else.

## Physical disks: export, don't hand over

The obvious design on Linux is to open the device in the helper and pass the
file descriptor to QEMU (`SCM_RIGHTS`, then QMP `add-fd`). Windows has no
equivalent: QEMU cannot be handed an open disk handle, so it would have to run
elevated to open `\\.\PhysicalDriveN` itself, which defeats the point.

Instead, on **both** platforms the helper runs a privileged
`qemu-storage-daemon` (part of every QEMU install) that opens the device and
exports it over NBD on an AF_UNIX socket. The VM's QEMU stays unprivileged and
attaches the export as an ordinary block node:

```
helper (privileged) ─spawns─► qemu-storage-daemon (privileged)
                                --blockdev host_device|file  /dev/disk/by-id/… | \\.\PhysicalDriveN
                                --nbd-server addr.type=unix,addr.path=<exports>/<id>.sock
                                --export type=nbd,name=<id>,writable=off|on
qemu-system-* (unprivileged) ── -blockdev {"driver":"nbd","server":{"type":"unix","path":…},"export":<id>}
```

Verified on a Windows host with QEMU 11.1: `qemu-storage-daemon` serves an
export on an AF_UNIX socket and an unprivileged `qemu-system-x86_64` attaches
it; a wrong export name is refused ("Requested export not available").
(`qemu-nbd` on Windows rejects drive-letter socket paths as "not absolute",
which is another reason to use the storage daemon.)

Consequences:

- One code path for both platforms; no `unsafe` and no fd passing.
- The storage daemon only ever opens the one allowlisted device it was started for.
- Read-only is enforced by the export (`writable=off`), not just by QEMU.
- The cost is an NBD hop over a local socket; for passthrough of removable
  media, forensics images and installers that is acceptable. Raw throughput
  workloads should use images in pools.
- Exports survive daemon restarts (the helper owns them) and are re-attached
  like VMs: by recorded pid + start time, then QMP to the storage daemon.

## Transport and authentication

- **Linux**: AF_UNIX socket `/run/qemu-ui-helper/helper.sock`, mode `0660`,
  group `qemu-ui`. The helper checks the peer's uid (`SO_PEERCRED`, available
  safely through tokio's `UnixStream::peer_cred`) against the configured daemon
  user and rejects anything else.
- **Windows**: AF_UNIX socket in a directory whose ACL grants access only to
  `SYSTEM`, Administrators and the daemon's account. Windows AF_UNIX has no
  peer credentials, so a shared secret (written by the installer into a file
  readable only by the daemon's account) is required on every connection as
  well.
- Export sockets live in a directory writable only by the helper and readable
  by the daemon's account, so other local users cannot attach exports.

## Protocol

JSON lines with request ids, like QMP. Requests are a **closed enum**; there is
no "run a command" or "open a path" request.

| Request | Effect | Validation |
|---|---|---|
| `hello {version, secret?}` | Version handshake | Peer uid (Linux) / secret (Windows) |
| `list_disks` | Disks with model, size, bus, removable, partitions, mounted/online volumes, system-disk flag | — (read-only) |
| `export_disk {disk_id, writable}` | Start a storage daemon exporting the disk | Disk is in the allowlist; not the system/boot disk; Linux: no mounted partition and no holders (`/sys/block/*/holders`, LVM/MD/dm); Windows: every volume offline; `writable` only if the allowlist entry permits it |
| `unexport {export_id}` | Stop it | Export exists and was created by this daemon |
| `list_exports` | Current exports | — |
| `create_tap {vm_id, bridge}` (M3) | `ip tuntap add … user <daemon uid>`, attach to bridge | Bridge in the allowlist; name derived from `vm_id` in the `qui-` namespace |
| `delete_tap {name}` (M3) | Remove it | Name in the `qui-` namespace and created by the helper |

Disk ids are stable identifiers, never paths supplied by the daemon: Linux
`/dev/disk/by-id/<id>`, Windows the disk's unique id (`Get-Disk` `UniqueId`)
mapped to `\\.\PhysicalDriveN` by the helper at export time, so renumbering
after a reboot cannot redirect an export to a different disk.

## Configuration (administrator-owned)

`/etc/qemu-ui/helper.toml` (Linux, root-owned `0644`) or
`%ProgramData%\qemu-ui\helper.toml` (Windows, Administrators-writable):

```toml
daemon_user = "qemu-ui"          # Linux: only this uid may connect

[disks]
allow_removable = true           # USB sticks, card readers
allow_writable = false           # default: exports are read-only
allow = ["usb-SanDisk_Ultra_4C53…", "wwn-0x5000c500…"]   # explicit ids
deny_system_disk = true          # cannot be turned off

[network]                        # M3
bridges = ["br0"]
```

Nothing is allowed by default: a fresh install can list disks but export
none until the administrator adds an id or enables removable media.

## Windows offline volumes

Windows locks online volumes, so a disk with mounted volumes cannot be opened
for writing (and reading it while Windows writes is unsafe). The helper does
**not** take disks offline itself. The UI explains the requirement and shows the
exact command for the user to run in an elevated shell
(`Set-Disk -Number N -IsOffline $true`), with a warning when the disk holds the
system or any in-use volume. The "USB attach" alternative (M2-12) is offered for
removable devices.

## Audit

Every request is logged by the helper (requesting uid or "secret ok", request,
validation result, outcome) to its own log, which the daemon cannot modify. The
daemon also records helper calls in its audit log (M6-04).

## Packaging

- Linux: `qemu-ui-helper.service` (system unit, `User=root`,
  `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`,
  `RestrictAddressFamilies=AF_UNIX AF_NETLINK`, `CapabilityBoundingSet=
  CAP_NET_ADMIN CAP_SYS_ADMIN CAP_DAC_OVERRIDE CAP_SETUID CAP_SETGID`).
- Windows: a service running as `LocalSystem` (M6-09). For development, the
  helper can be run from an elevated shell.
- The daemon works without the helper; features that need it are shown as
  "needs setup: install the helper" through the parity ledger.

## Out of scope here

- Per-VM users and cgroups (M6-05) reuse this transport.
- TAP and bridges are implemented in M3-05; only the protocol shape is fixed here.
