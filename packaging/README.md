# Packaging

## Linux (systemd)

`systemd/qemu-ui.service` runs the daemon as a user service. The important
line is `KillMode=process`: QEMU processes started by the daemon live in the
service's cgroup, and systemd's default (`control-group`) would kill all of
them whenever the service stops or restarts. With `KillMode=process`, only
the daemon is stopped; VMs keep running and the next daemon instance
re-attaches to them (verified by pid, process start time and QMP).

Check after installing:

```sh
qemu-ui start <vm>
systemctl --user restart qemu-ui
qemu-ui vms              # the VM is still running, same process
```

Per-VM transient scopes (each VM in its own cgroup, with CPU/memory/IO
limits) are planned for M6-05 and will replace this.

## Windows

The daemon spawns QEMU detached, in its own process group, and breaks away
from the daemon's job object where allowed, so ending the daemon process does
not end VMs. Running as a Windows service is M6-09.
