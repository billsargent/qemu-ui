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
| `create_tap {vm_id, nic, network, multi_queue}` (M3-05) | `ip tuntap add dev qui-<vm_id>-<nic> mode tap user <daemon uid> [multi_queue]`, `ip link set … master <bridge>`, `up` | Linux only; `network` is `{kind: "bridge", bridge}` (bridge in the allowlist and present) or `{kind: "host-only"}` (enabled in the config; the bridge is created on first use); the name is derived, never supplied; the owner is the configured daemon user, never a uid from the request; an existing interface with that name is refused unless the helper created it |
| `delete_tap {ifname}` (M3-05) | `ip link delete dev <ifname>` | Only taps this helper created (recorded in its state directory, so it survives restarts) |
| `network_policy` (M3-05) | The allowlisted bridges and whether host-only is enabled | — (read-only) |

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

[network]                        # M3-05 (Linux)
bridges = ["br0"]                # existing bridges VMs may join

[network.host_only]              # optional: a private bridge with the host on it
bridge = "qui-hostonly"          # default
address = "10.213.0.1/24"        # the host's address on it (default)
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

## Tap networks (M3-05, M3-06)

- **Linux, bridged and host-only.** On each start the daemon asks for one tap
  per NIC on such a network, records the names, and asks for their deletion
  when the run ends or the start fails. QEMU (unprivileged) opens the tap by
  name (`-netdev tap,ifname=…,script=no,downscript=no`), which works because
  the helper made the daemon user its owner; `vhost=on` is added when
  `/dev/vhost-net` is usable, and `queues=N` for multiqueue NICs (the tap is
  then created with `multi_queue`). The helper never configures addresses on
  existing bridges; the host-only bridge is the only interface it creates,
  with the address from its own configuration.
- **Windows, TAP.** TAP-Windows adapters are created by the driver's
  installer, and opening one needs no privileges, so the helper is not
  involved: a TAP network names the adapter and QEMU attaches with
  `-netdev tap,ifname=<adapter>`. Putting guests on the LAN is a one-time
  administrator step (bridge the TAP adapter with the physical adapter in
  Network Connections), which the UI explains. TAP-Windows has a single
  queue, so multiqueue is refused there.
- Guest addresses on tap-backed networks come from the host's neighbour table
  (`ip neigh` / `Get-NetNeighbor`), matched by the NIC's MAC.

## Per-VM cgroups (M6-05)

CPU and memory limits for a running VM are a cgroup v2 group per VM,
`<mount>/<parent>/vm-<id>` (`parent` defaults to `qemu-ui`). Off unless the
administrator enables it:

```toml
[isolation]
cgroups = true
# cgroup_parent = "qemu-ui"
```

- `apply_vm_limits { vm_id, pid, cpu_max_percent, cpu_weight, memory_max_mib }`
  creates or updates the group and moves `pid` into it. The request never
  names a file: the helper writes only `cpu.max`, `cpu.weight`,
  `memory.max` and `memory.swap.max` (derived by
  `qemu_ui_core::limits::cgroup_v2_settings`), plus `cgroup.subtree_control`
  on the parents to enable the `cpu` and `memory` controllers.
- The pid must be a `qemu-system-*` process (`/proc/<pid>/exe`) whose real
  uid is the daemon user. A daemon that asks for any other process is
  refused, so the request cannot be used to throttle or move arbitrary
  processes.
- `release_vm_limits { vm_id }` removes the group after QEMU exits.
- Disk IO limits do not need the helper: QEMU throttle groups enforce them
  on every platform.

### Per-VM users (designed, not yet implemented)

Running each VM's QEMU as its own uid needs the helper to allocate a uid
from a configured range, chown the VM's run dir and hand the daemon file
descriptors for the images (so images stay owned by the daemon user), and
spawn QEMU through `systemd-run --scope --uid=` or an equivalent. That is a
larger change to how disks are opened; it is tracked as a follow-up.

### LSM profiles

`data/apparmor/qemu-ui-vm` is an AppArmor profile template for QEMU
processes started by qemu-ui (run dirs, pools, firmware). Loading it is a
manual administrator step (`apparmor_parser -r`), documented in the
template; the daemon only detects whether AppArmor or SELinux is active
and reports it.

## Windows (M6-06)

QEMU is assigned to an unnamed job object when the VM has CPU or memory
limits: `JobMemoryLimit` and a hard-capped CPU rate. Unnamed, so no other
local process can pre-create the job and keep control of it. The job is
never kill-on-close, so VMs keep surviving daemon exit; the daemon holds
its handle to change limits live.
`qemu-ui-winsec` (the only crate allowed `unsafe`) wraps the Win32 calls.
It can also run a process with a restricted token (privileges removed,
Administrators deny-only); spawning QEMU that way needs the daemon's
process code to move to `CreateProcessAsUserW` and is a follow-up. Under a
service, a restricted child cannot open the service session's desktop (its
ACL grants Administrators, which is deny-only) and fails with
`STATUS_DLL_INIT_FAILED`, so that follow-up must give QEMU a desktop it can
access (or a dedicated one) as well.

## Out of scope here
