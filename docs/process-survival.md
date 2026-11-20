# Process survival rules (M1-06, M6-09)

A VM is a separate QEMU process. It must keep running when the daemon stops,
restarts, crashes or is upgraded, and the next daemon must find it again
(plan.md §3.2). This page states the rules per platform and how they are
tested.

## What the daemon does on every platform

- QEMU (and its sidecars: virtiofsd, passt) is spawned **detached**: stdin is
  null, stdout/stderr go to a per-run log file (never a pipe a dead daemon
  would stop draining), in its own process group.
- Every run records the QEMU **pid and process start time**. A pid alone is
  not an identity (pids are reused); reattach checks both.
- On start the daemon **reattaches**: for every run without an end time it
  checks the process identity, reconnects QMP and resumes monitoring. A run
  whose process is gone is closed as crashed.
- Shutdown (Ctrl-C, SIGTERM, a Windows service Stop) stops serving the API
  and leaves VMs alone.

## Linux (systemd)

`qemu-ui service install` writes `/etc/systemd/system/qemu-ui.service` from
`data/systemd/qemu-ui.service.in`:

- `KillMode=process`: `systemctl stop|restart qemu-ui` signals only the
  daemon. QEMU processes stay in the unit's cgroup and keep running; systemd
  logs them as "left-over process" on the next start, which is expected.
- `RuntimeDirectory=qemu-ui` with `RuntimeDirectoryPreserve=yes`: the per-VM
  monitor sockets under `/run/qemu-ui` survive the restart, so the next
  daemon can reconnect.
- The daemon handles SIGTERM as a graceful shutdown.
- `systemctl kill qemu-ui` (default `--kill-whom=all`) and `KillMode=control-group`
  would kill VMs; don't use them. Per-VM systemd scopes (each VM in its own
  cgroup) arrive with M6-05 isolation and keep the same rule.

The privileged helper's unit (`qemu-ui-helper.service`) also uses
`KillMode=process`, so disk exports serving running VMs survive a helper
restart.

## Windows

- QEMU is created with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` and
  `CREATE_BREAKAWAY_FROM_JOB`. If the daemon runs inside a job object that
  forbids breakaway, the spawn falls back to staying in the job; then the
  job's owner decides (a kill-on-close job would take VMs down with it).
- The service control manager does not put services in a job object, so
  stopping or restarting the `qemu-ui` service (installed by
  `qemu-ui service install`) leaves VMs running.
- Service account: by default the per-service virtual account
  `NT SERVICE\qemu-ui` (no password, no interactive logon). The installer
  grants it Modify on the data directory
  (`icacls <data-dir> /grant "NT SERVICE\qemu-ui:(OI)(CI)M"`). Pool
  directories and ISO folders outside the data directory need the same grant;
  without it the daemon cannot open those images. `--account LocalSystem`
  opts out (not recommended).

## Tests

- `crates/qemu-ui-daemon/tests/survival.rs` runs the real daemon binary,
  starts a VM, then stops the daemon (Linux: SIGTERM, then SIGKILL; Windows:
  process termination), checks QEMU is still alive, and starts a new daemon
  that must report the VM running on the same QEMU process. It runs in CI on
  both platforms.
- `supervisor::full_lifecycle_with_reattach` covers the in-process side:
  reattach, then stop/start/crash detection.
- Not covered automatically: an actual `systemctl restart` (CI runners do not
  install units). The unit's `KillMode=process` and `RuntimeDirectoryPreserve`
  are checked by unit-generation tests in `qemu-ui-cli`.
